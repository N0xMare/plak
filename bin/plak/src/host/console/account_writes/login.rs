//! Each attempt owns its application, pending token and cancellable worker.

use super::*;
use crate::commands::application::{HostLoginStartError, LoginOutcome};
use crate::presentation::CredentialState;
use plakd_wire_types::console::{LoginMethod, LoginPhase, SecretInput};
use plakous::subscriptions::NativeLoginCompletion;
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};
use tokio::sync::{oneshot, watch};
use zeroize::Zeroizing;

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
    input: Option<oneshot::Sender<SecretInput>>,
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

    fn admit(
        &mut self,
        owner: HostRequestContext,
        account: String,
        cancel: watch::Sender<bool>,
        input: oneshot::Sender<SecretInput>,
        progress: Arc<Mutex<Progress>>,
    ) -> Result<String, RequestRefusal> {
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
            return Err(RequestRefusal::RateLimited {
                retry_after_ms: 1_000,
            });
        }
        self.starts.retain(|_, times| {
            times.retain(|time| now.duration_since(*time) < Duration::from_secs(60));
            !times.is_empty()
        });
        if !self.starts.contains_key(&owner.principal) && self.starts.len() >= 1024 {
            return Err(RequestRefusal::RateLimited {
                retry_after_ms: 60_000,
            });
        }
        let times = self.starts.entry(owner.principal.clone()).or_default();
        if times.len() >= 3 {
            return Err(RequestRefusal::RateLimited {
                retry_after_ms: 60_000,
            });
        }
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
        Ok(id)
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
                )
                .await;
        }
        let (HostRequest::AccountLoginPoll { attempt: id }
        | HostRequest::AccountLoginCancel { attempt: id }
        | HostRequest::AccountLoginComplete { attempt: id, .. }) = &request
        else {
            unreachable!()
        };
        let id = id.clone();
        let mut logins = self.logins.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(attempt) = logins.attempts.get_mut(&id) else {
            return Ok(refused(validation("attempt", ValidationCode::UnknownId)));
        };
        if !owner_matches(&attempt.owner, &context) {
            return Ok(refused(validation("attempt", ValidationCode::UnknownId)));
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
        if now >= attempt.expires && progress.pending() {
            let _ = attempt.cancel.send(true);
            progress.phase = LoginPhase::Expired;
        }
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
                    return Ok(refused(validation("pasted", ValidationCode::InvalidValue)));
                }
                let Some(input) = attempt.input.take() else {
                    return Ok(refused(validation(
                        "attempt",
                        ValidationCode::InvalidAction,
                    )));
                };
                if input.send(pasted).is_err() {
                    progress.phase = LoginPhase::Failed;
                } else {
                    progress.phase = LoginPhase::Exchanging;
                }
            }
            _ => unreachable!(),
        }
        operator::audit(
            &context,
            "login_progress",
            Some(&attempt.account),
            "success",
        );
        Ok(HostResponse::LoginProgress {
            attempt: id.clone(),
            phase: progress.phase,
            provenance_warning: progress.provenance_warning,
        })
    }

    async fn start_login(
        self: Arc<Self>,
        context: HostRequestContext,
        account: String,
        notice_revision: String,
        replace_newer: bool,
        confirmation: String,
    ) -> Result<HostResponse, RequestRefusal> {
        let action = OperatorAction::AccountLoginStart {
            account_id: account.clone(),
            notice_revision: notice_revision.clone(),
            replace_newer,
        };
        self.write_capability(&context.principal, &action)?;
        let permit = match self.mutation(&context, &action, &confirmation) {
            Ok(permit) => permit,
            Err(reason) => return reason.outcome(),
        };
        let (cancel, receiver) = watch::channel(false);
        let (input, pasted) = oneshot::channel();
        let (reply, challenge) = oneshot::channel();
        let progress = Arc::new(Mutex::new(Progress {
            phase: LoginPhase::Starting,
            provenance_warning: false,
        }));
        let id = self
            .logins
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .admit(
                context.clone(),
                account.clone(),
                cancel.clone(),
                input,
                Arc::clone(&progress),
            )?;
        let task_id = id.clone();
        let owner = context.clone();
        let mut guard = CancelStart(Some(cancel));
        if worker(move || {
            let _permit = permit;
            let result = self.run_login(
                &owner,
                &task_id,
                &account,
                &notice_revision,
                replace_newer,
                receiver,
                pasted,
                &progress,
                reply,
            );
            let mut progress = progress.lock().unwrap_or_else(PoisonError::into_inner);
            if progress.pending() {
                progress.phase = if result.is_ok() {
                    LoginPhase::Succeeded
                } else {
                    LoginPhase::Failed
                };
            }
            operator::audit(
                &owner,
                "login_finished",
                Some(&account),
                if result.is_ok() { "success" } else { "error" },
            );
        })
        .is_err()
        {
            return Ok(refused(failed(false)));
        }
        let result = challenge.await.unwrap_or_else(|_| Err(failed(false)));
        guard.0 = None;
        Ok(result.unwrap_or_else(refused))
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "one worker owns the exact pending login and its bounded channels"
    )]
    fn run_login(
        &self,
        owner: &HostRequestContext,
        id: &str,
        account: &str,
        notice_revision: &str,
        replace_newer: bool,
        mut cancel: watch::Receiver<bool>,
        pasted: oneshot::Receiver<SecretInput>,
        progress: &Mutex<Progress>,
        reply: oneshot::Sender<Result<HostResponse, ReadRefusal>>,
    ) -> Result<(), ReadRefusal> {
        let mut reply = Some(reply);
        let mut app = self.application()?;
        let account = super::super::accounts::exact(account)?;
        let start = (|| {
            let notice = app.notice(&account).map_err(|_| failed(false))?;
            if notice.notice_revision != notice_revision {
                return Err(validation("notice_revision", ValidationCode::InvalidValue));
            }
            app.prepare_login(&account).map_err(|_| failed(false))?;
            let status = app.offline_status(&account).map_err(|_| failed(false))?;
            if matches!(
                status.compatibility,
                CompatibilityState::Inactive | CompatibilityState::Unsupported
            ) {
                return Err(validation("notice_revision", ValidationCode::InvalidValue));
            }
            if status.credential == CredentialState::WrittenByNewerBuild {
                if !replace_newer {
                    return Err(validation("replace_newer", ValidationCode::InvalidValue));
                }
                app.replace_newer_build(&account)
                    .map_err(|_| failed(true))?;
            }
            app.acknowledge_exact(&account, notice_revision)
                .map_err(|_| failed(false))?;
            if *cancel.borrow() {
                return Err(failed(false));
            }
            app.begin_host_login(&account).map_err(|error| match error {
                HostLoginStartError::DeviceUnavailable => ReadRefusal::DeviceLoginUnavailable {},
                HostLoginStartError::Failed => failed(false),
            })
        })();
        let challenge = match start {
            Ok(challenge) => challenge,
            Err(reason) => {
                if let Some(reply) = reply.take() {
                    let _ = reply.send(Err(reason.clone()));
                }
                app.end_login_attempt(&account);
                return Err(reason);
            }
        };
        let device = challenge.completion == NativeLoginCompletion::DeviceCode;
        if challenge.completion == NativeLoginCompletion::Automatic {
            return Err(failed(false));
        }
        let timeout = challenge
            .device_code
            .as_ref()
            .map_or(LIFETIME, |code| code.expires_in.min(LIFETIME));
        let expires = Instant::now() + timeout;
        let code = challenge
            .device_code
            .map(|code| SecretInput::new(code.user_code).map_err(|_| failed(false)))
            .transpose()?;
        let response = HostResponse::LoginChallenge {
            attempt: id.into(), method: if device { LoginMethod::DeviceCode } else { LoginMethod::Paste }, authorize_url: SecretInput::new(Zeroizing::new(challenge.authorize_url)).map_err(|_| failed(false))?, user_code: code,
            expires_in_ms: u64::try_from(timeout.as_millis()).unwrap_or(0), warning: "Only enter this code on the provider's page you explicitly opened. Never share it. This logs in the serving machine, not your browser's machine.".into(),
        };
        progress
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .phase = if device {
            LoginPhase::Exchanging
        } else {
            LoginPhase::AwaitingInput
        };
        if reply
            .take()
            .is_none_or(|reply| reply.send(Ok(response)).is_err())
        {
            return Err(failed(false));
        }
        let pasted = if device {
            None
        } else {
            let value = app.executor.block_on(async {
                if *cancel.borrow() { return None; }
                tokio::select! { biased; _ = cancel.changed() => None, value = tokio::time::timeout(timeout, pasted) => value.ok().and_then(Result::ok) }
            }).map_err(|_| failed(false))?;
            if value.is_none() {
                progress
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .phase = if Instant::now() >= expires {
                    LoginPhase::Expired
                } else {
                    LoginPhase::Cancelled
                };
                app.end_login_attempt(&account);
                return Ok(());
            }
            value
        };
        let result = app
            .finish_host_login(
                challenge.pending,
                pasted.as_ref().map(SecretInput::expose),
                expires.saturating_duration_since(Instant::now()),
                cancel,
            )
            .map_err(|_| failed(true));
        app.end_login_attempt(&account);
        match result? {
            LoginOutcome::Completed(CredentialState::Ready) => {
                progress
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .phase = LoginPhase::Succeeded;
                progress
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .provenance_warning = !app.record_login_host(&account);
                operator::audit(owner, "login_ready", Some(account.as_str()), "success");
            }
            LoginOutcome::Cancelled => {
                progress
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .phase = LoginPhase::Cancelled
            }
            LoginOutcome::TimedOut => {
                progress
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .phase = LoginPhase::Expired
            }
            _ => return Err(failed(true)),
        }
        Ok(())
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
