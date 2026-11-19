//! Each attempt owns one cancellable gateway login worker.

use super::{
    AccountError, Arc, Console, HostPrincipal, HostRequest, HostRequestContext, HostResponse,
    MutationRefusal, OperatorAction, RequestRefusal, ValidationCode, failed, refused, validation,
    worker,
};
use plakd_wire_types::console::{LoginPhase, SecretInput};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, watch};

mod gateway;
#[cfg(test)]
mod tests;

const LIFETIME: Duration = Duration::from_secs(15 * 60);

#[derive(Default)]
pub(in super::super) struct Logins {
    attempts: BTreeMap<String, Attempt>,
    starts: BTreeMap<HostPrincipal, VecDeque<Instant>>,
}

struct Attempt {
    owner: HostRequestContext,
    account: String,
    cancel: watch::Sender<bool>,
    input: Option<mpsc::Sender<Completion>>,
    progress: Arc<Mutex<Progress>>,
    expires: Instant,
    last_poll: Option<Instant>,
}
impl Drop for Attempt {
    fn drop(&mut self) {
        let _ = self.cancel.send(true);
    }
}
struct Progress {
    phase: LoginPhase,
    provenance_warning: bool,
    warning: Option<String>,
}

struct Completion {
    pasted: SecretInput,
    reply: oneshot::Sender<Result<(), AccountError>>,
}
type LoginReply = (
    HostResponse,
    Option<oneshot::Receiver<Result<(), AccountError>>>,
);

struct LoginWork<'a> {
    id: &'a str,
    account: &'a str,
    notice_revision: &'a str,
    replace_newer: bool,
    cancel: watch::Receiver<bool>,
    pasted: mpsc::Receiver<Completion>,
    progress: &'a Mutex<Progress>,
    reply: oneshot::Sender<Result<HostResponse, AccountError>>,
}
impl Progress {
    fn pending(&self) -> bool {
        matches!(
            self.phase,
            LoginPhase::Starting
                | LoginPhase::AwaitingInput
                | LoginPhase::Exchanging
                | LoginPhase::Cancelling
        )
    }
}

impl Logins {
    pub(in super::super) fn disconnect(
        &mut self,
        principal: &HostPrincipal,
        connection: u64,
        generation: u64,
    ) {
        self.attempts.retain(|_, attempt| {
            &attempt.owner.principal != principal
                || attempt.owner.connection_id != connection
                || attempt.owner.host_generation != generation
        });
    }

    fn admit<T>(
        &mut self,
        owner: HostRequestContext,
        account: String,
        cancel: watch::Sender<bool>,
        input: mpsc::Sender<Completion>,
        progress: Arc<Mutex<Progress>>,
        admission: impl FnOnce() -> Result<T, MutationRefusal>,
    ) -> Result<(String, T), MutationRefusal> {
        let now = Instant::now();
        self.attempts.retain(|_, attempt| now < attempt.expires);
        let active: Vec<_> = self
            .attempts
            .values()
            .filter(|attempt| {
                attempt
                    .progress
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .pending()
            })
            .collect();
        if self.attempts.len() >= 128
            || active.len() >= 3
            || active.iter().any(|attempt| attempt.account == account)
            || active
                .iter()
                .filter(|attempt| attempt.owner.principal == owner.principal)
                .count()
                >= 2
        {
            return Err(MutationRefusal::Quota(RequestRefusal::RateLimited {
                retry_after_ms: 1_000,
            }));
        }
        self.starts.retain(|_, times| {
            times.retain(|time| now.duration_since(*time) < Duration::from_secs(60));
            !times.is_empty()
        });
        if !self.starts.contains_key(&owner.principal) && self.starts.len() >= 1024 {
            return Err(MutationRefusal::Quota(RequestRefusal::RateLimited {
                retry_after_ms: 60_000,
            }));
        }
        let times = self.starts.entry(owner.principal.clone()).or_default();
        if times.len() >= 3 {
            return Err(MutationRefusal::Quota(RequestRefusal::RateLimited {
                retry_after_ms: 60_000,
            }));
        }
        let admitted = admission()?;
        times.push_back(now);
        let id = uuid::Uuid::new_v4().to_string();
        self.attempts.insert(
            id.clone(),
            Attempt {
                owner,
                account,
                cancel,
                input: Some(input),
                progress,
                expires: now + LIFETIME,
                last_poll: None,
            },
        );
        Ok((id, admitted))
    }
}

impl Console {
    pub(in super::super) async fn login_request(
        self: Arc<Self>,
        context: HostRequestContext,
        request: HostRequest,
    ) -> Result<HostResponse, RequestRefusal> {
        if let HostRequest::AccountLoginStart {
            account_id,
            notice_revision,
            replace_newer,
            confirmation,
        } = request
        {
            return self
                .start_login(
                    context,
                    account_id,
                    notice_revision,
                    replace_newer,
                    confirmation,
                    Self::run_login,
                )
                .await;
        }
        let (response, checked) = self.update_login(&context, request)?;
        if let Some(checked) = checked
            && let Err(reason) = checked.await.unwrap_or_else(|_| Err(failed(false)))
        {
            return Ok(reason.response());
        }
        Ok(response)
    }

    fn update_login(
        &self,
        context: &HostRequestContext,
        request: HostRequest,
    ) -> Result<LoginReply, RequestRefusal> {
        let id = attempt_id(&request).to_owned();
        let mut logins = self.logins.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(attempt) = logins.attempts.get_mut(&id) else {
            return Ok((
                refused(validation("attempt", ValidationCode::UnknownId)),
                None,
            ));
        };
        if !owner_matches(&attempt.owner, context) {
            return Ok((
                refused(validation("attempt", ValidationCode::UnknownId)),
                None,
            ));
        }
        self.write_capability(
            &context.principal,
            &OperatorAction::AccountRefresh {
                account_id: attempt.account.clone(),
            },
        )?;
        let now = Instant::now();
        let mut progress = attempt
            .progress
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if progress.pending() && attempt.cancel.receiver_count() == 0 {
            progress.phase = LoginPhase::OutcomeUnknown;
        }
        if now >= attempt.expires && progress.pending() {
            let _ = attempt.cancel.send(true);
            progress.phase = LoginPhase::Expired;
        }
        let mut checked = None;
        match request {
            HostRequest::AccountLoginPoll { .. } => {
                if attempt
                    .last_poll
                    .is_some_and(|last| now.duration_since(last) < Duration::from_secs(1))
                {
                    return Err(RequestRefusal::RateLimited {
                        retry_after_ms: 1_000,
                    });
                }
                attempt.last_poll = Some(now);
            }
            HostRequest::AccountLoginCancel { .. } => {
                let _ = attempt.cancel.send(true);
                attempt.input = None;
                if progress.pending() {
                    progress.phase = LoginPhase::Cancelling;
                }
            }
            HostRequest::AccountLoginComplete { pasted, .. } => {
                if progress.phase != LoginPhase::AwaitingInput
                    || pasted.expose().len()
                        > crate::presentation::MAX_REMOTE_AUTHORIZATION_INPUT_BYTES
                {
                    return Ok((
                        refused(validation("pasted", ValidationCode::InvalidValue)),
                        None,
                    ));
                }
                let Some(input) = &attempt.input else {
                    return Ok((
                        refused(validation("attempt", ValidationCode::InvalidAction)),
                        None,
                    ));
                };
                let (reply, accepted) = oneshot::channel();
                if input.try_send(Completion { pasted, reply }).is_err() {
                    progress.phase = LoginPhase::Failed;
                } else {
                    progress.phase = LoginPhase::Exchanging;
                    checked = Some(accepted);
                }
            }
            _ => unreachable!(),
        }
        let phase = progress.phase;
        let provenance_warning = progress.provenance_warning;
        let warning = progress.warning.clone();
        let account = attempt.account.clone();
        drop(progress);
        drop(logins);
        self.audit(context, "login_progress", Some(&account), "success", None);
        Ok((
            HostResponse::LoginProgress {
                attempt: id.clone(),
                phase,
                provenance_warning,
                warning,
            },
            checked,
        ))
    }

    async fn start_login(
        self: Arc<Self>,
        context: HostRequestContext,
        account: String,
        notice_revision: String,
        replace_newer: bool,
        confirmation: String,
        run: impl FnOnce(&Self, LoginWork<'_>) -> Result<(), AccountError> + Send + 'static,
    ) -> Result<HostResponse, RequestRefusal> {
        let action = OperatorAction::AccountLoginStart {
            account_id: account.clone(),
            notice_revision: notice_revision.clone(),
            replace_newer,
        };
        self.write_capability(&context.principal, &action)?;
        let (cancel, receiver) = watch::channel(false);
        let (input, pasted) = mpsc::channel(1);
        let (reply, challenge) = oneshot::channel();
        let progress = Arc::new(Mutex::new(Progress {
            phase: LoginPhase::Starting,
            provenance_warning: false,
            warning: None,
        }));
        let admission = self
            .logins
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .admit(
                context.clone(),
                account.clone(),
                cancel.clone(),
                input,
                Arc::clone(&progress),
                || self.mutation(&context, &action, &confirmation),
            );
        let (id, permit) = match admission {
            Ok(admitted) => admitted,
            Err(reason) => return reason.outcome(),
        };
        // Login quotas own the pending attempt; single-flight fences only admission.
        drop(permit);
        let task_id = id.clone();
        let owner = context.clone();
        let registry = Arc::clone(&self);
        let mut guard = CancelStart(Some(cancel));
        if worker(move || {
            let result = run(
                &self,
                LoginWork {
                    id: &task_id,
                    account: &account,
                    notice_revision: &notice_revision,
                    replace_newer,
                    cancel: receiver,
                    pasted,
                    progress: &progress,
                    reply,
                },
            );
            let mut progress = progress.lock().unwrap_or_else(PoisonError::into_inner);
            if progress.pending() {
                progress.phase = if matches!(result, Err(AccountError::Unknown)) {
                    LoginPhase::OutcomeUnknown
                } else if result.is_ok() {
                    LoginPhase::Succeeded
                } else {
                    LoginPhase::Failed
                };
            }
            let category = match progress.phase {
                LoginPhase::Succeeded => "success",
                LoginPhase::Cancelled => "cancelled",
                LoginPhase::Expired => "expired",
                LoginPhase::OutcomeUnknown => "outcome_unknown",
                _ => "error",
            };
            drop(progress);
            self.audit(&owner, "login_finished", Some(&account), category, None);
        })
        .is_err()
        {
            registry
                .logins
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .attempts
                .remove(&id);
            return Ok(failed(false).response());
        }
        let result = challenge.await.unwrap_or_else(|_| Err(failed(false)));
        guard.0 = None;
        Ok(result.unwrap_or_else(AccountError::response))
    }

    fn run_login(&self, work: LoginWork<'_>) -> Result<(), AccountError> {
        let link = self.custody.admin().ok_or_else(|| failed(false))?;
        gateway::run(link, work)
    }
}

fn attempt_id(request: &HostRequest) -> &str {
    match request {
        HostRequest::AccountLoginPoll { attempt }
        | HostRequest::AccountLoginCancel { attempt }
        | HostRequest::AccountLoginComplete { attempt, .. } => attempt,
        _ => unreachable!(),
    }
}

fn owner_matches(left: &HostRequestContext, right: &HostRequestContext) -> bool {
    left.principal == right.principal
        && left.connection_id == right.connection_id
        && left.host_generation == right.host_generation
}

struct CancelStart(Option<watch::Sender<bool>>);
impl Drop for CancelStart {
    fn drop(&mut self) {
        if let Some(cancel) = &self.0 {
            let _ = cancel.send(true);
        }
    }
}
