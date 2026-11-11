//! Account writes reuse the CLI account service without terminal presentation.

mod login;
#[cfg(test)]
mod tests;
pub(super) use login::Logins;

use super::{Console, HostPrincipal, HostRequestContext, operator, refused, validation};
use crate::commands::application::{AccountOps, PlakousApplication, Revocation, StoredCredential};
use crate::presentation::CompatibilityState;
use plakd_wire_types::console::{
    HostRequest, HostResponse, OperatorAction, ReadRefusal, ValidationCode,
};
use plakous_host_protocol::wire::RequestRefusal;
use plakous_hosted::account::AccountRef;
use std::sync::Arc;

pub(super) fn action(request: &HostRequest) -> Option<OperatorAction> {
    Some(match request {
        HostRequest::AccountKeySet { account_id, .. } => OperatorAction::AccountKeySet {
            account_id: account_id.clone(),
        },
        HostRequest::AccountKeyClear { account_id, .. } => OperatorAction::AccountKeyClear {
            account_id: account_id.clone(),
        },
        HostRequest::AccountLogout {
            account_id,
            local_only,
            ..
        } => OperatorAction::AccountLogout {
            account_id: account_id.clone(),
            local_only: *local_only,
        },
        HostRequest::AccountRefresh { account_id, .. } => OperatorAction::AccountRefresh {
            account_id: account_id.clone(),
        },
        HostRequest::AccountRemove { account_id, .. } => OperatorAction::AccountRemove {
            account_id: account_id.clone(),
        },
        HostRequest::AccountLoginStart {
            account_id,
            notice_revision,
            replace_newer,
            ..
        } => OperatorAction::AccountLoginStart {
            account_id: account_id.clone(),
            notice_revision: notice_revision.clone(),
            replace_newer: *replace_newer,
        },
        _ => return None,
    })
}

pub(super) fn is_request(request: &HostRequest) -> bool {
    action(request).is_some()
        || matches!(
            request,
            HostRequest::AccountLoginPoll { .. }
                | HostRequest::AccountLoginCancel { .. }
                | HostRequest::AccountLoginComplete { .. }
        )
}

pub(super) struct Mutation {
    pub console: Arc<Console>,
    pub principal: HostPrincipal,
}

pub(super) enum MutationRefusal {
    Consent(ReadRefusal),
    Quota(RequestRefusal),
}
impl MutationRefusal {
    pub fn outcome(self) -> Result<HostResponse, RequestRefusal> {
        match self {
            Self::Consent(reason) => Ok(refused(reason)),
            Self::Quota(reason) => Err(reason),
        }
    }
}
impl Drop for Mutation {
    fn drop(&mut self) {
        self.console.end_mutation(&self.principal);
    }
}

impl Console {
    pub(super) fn mutation(
        self: &Arc<Self>,
        context: &HostRequestContext,
        action: &OperatorAction,
        confirmation: &str,
    ) -> Result<Mutation, MutationRefusal> {
        self.consume(context, action, confirmation)
            .map_err(MutationRefusal::Consent)?;
        self.admit_mutation(&context.principal, operator::credentials(action))
            .map_err(MutationRefusal::Quota)?;
        Ok(Mutation {
            console: Arc::clone(self),
            principal: context.principal.clone(),
        })
    }

    pub(super) async fn account_write(
        self: Arc<Self>,
        context: HostRequestContext,
        request: HostRequest,
    ) -> Result<HostResponse, RequestRefusal> {
        if matches!(
            request,
            HostRequest::AccountLoginStart { .. }
                | HostRequest::AccountLoginPoll { .. }
                | HostRequest::AccountLoginComplete { .. }
                | HostRequest::AccountLoginCancel { .. }
        ) {
            return self.login_request(context, request).await;
        }
        let Some(action) = action(&request) else {
            return Ok(refused(validation(
                "request",
                ValidationCode::InvalidAction,
            )));
        };
        self.write_capability(&context.principal, &action)?;
        let (HostRequest::AccountKeySet { confirmation, .. }
        | HostRequest::AccountKeyClear { confirmation, .. }
        | HostRequest::AccountLogout { confirmation, .. }
        | HostRequest::AccountRefresh { confirmation, .. }
        | HostRequest::AccountRemove { confirmation, .. }) = &request
        else {
            unreachable!()
        };
        let permit = match self.mutation(&context, &action, confirmation) {
            Ok(permit) => permit,
            Err(reason) => return reason.outcome(),
        };
        let owner = context.clone();
        let target = operator::account(&action).map(str::to_owned);
        let result = worker(move || {
            let _permit = permit;
            let result = self.execute_account(request);
            operator::audit(
                &owner,
                "account_write",
                target.as_deref(),
                if result.is_ok() { "success" } else { "error" },
            );
            result.unwrap_or_else(refused)
        })
        .map_err(refused);
        let result = match result {
            Ok(result) => result.await,
            Err(response) => return Ok(response),
        }
        .unwrap_or_else(|_| {
            refused(ReadRefusal::AccountOperationFailed {
                outcome_unknown: true,
            })
        });
        Ok(result)
    }

    fn application(&self) -> Result<PlakousApplication, ReadRefusal> {
        PlakousApplication::new(self.state.clone(), None, None)
            .map(|app| app.with_account_home(Some(self.paths.clone())))
            .map_err(|_| failed(false))
    }

    fn execute_account(&self, request: HostRequest) -> Result<HostResponse, ReadRefusal> {
        let selected = action(&request)
            .and_then(|action| operator::account(&action).map(str::to_owned))
            .ok_or_else(|| failed(false))?;
        let account = super::accounts::exact(&selected)?;
        let mut remote_grant_may_remain = false;
        match request {
            HostRequest::AccountKeySet { secret, .. } => {
                env_override(&selected)?;
                crate::home::secrets::set_api_key(&self.paths, &selected, secret.into_zeroizing())
                    .map_err(|_| failed(false))?;
            }
            HostRequest::AccountKeyClear { .. } => {
                env_override(&selected)?;
                crate::home::secrets::clear_api_key(&self.paths, &selected)
                    .map_err(|_| failed(false))?;
            }
            HostRequest::AccountRemove { .. } => {
                if account.vertical().is_api() {
                    env_override(&selected)?;
                }
                self.application()?
                    .remove_account(&account)
                    .map_err(|_| failed(false))?;
            }
            HostRequest::AccountRefresh { .. } => {
                let mut app = self.application()?;
                require_current(&mut app, &account)?;
                app.refresh_now(&account).map_err(|_| failed(true))?;
            }
            HostRequest::AccountLogout { local_only, .. } => {
                let mut app = self.application()?;
                remote_grant_may_remain = logout(&mut app, &account, local_only)?;
            }
            _ => return Err(validation("request", ValidationCode::InvalidAction)),
        }
        Ok(HostResponse::AccountChanged {
            account_id: selected,
            remote_grant_may_remain,
            provenance_warning: false,
        })
    }
}

fn worker<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<tokio::sync::oneshot::Receiver<T>, ReadRefusal> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("operator-account".into())
        .spawn(move || {
            let _ = sender.send(work());
        })
        .map_err(|_| failed(false))?;
    Ok(receiver)
}

fn env_override(account: &str) -> Result<(), ReadRefusal> {
    if crate::home::secrets::env_var_for_account(account)
        .is_some_and(|key| std::env::var_os(key).is_some())
    {
        return Err(ReadRefusal::EnvironmentOverride {
            proven_no_effect: true,
        });
    }
    Ok(())
}

fn require_current(app: &mut impl AccountOps, account: &AccountRef) -> Result<(), ReadRefusal> {
    let status = app.offline_status(account).map_err(|_| failed(false))?;
    if status.compatibility != CompatibilityState::Current {
        return Err(validation("notice_revision", ValidationCode::InvalidValue));
    }
    Ok(())
}

fn logout(
    app: &mut impl AccountOps,
    account: &AccountRef,
    local_only: bool,
) -> Result<bool, ReadRefusal> {
    let stored = app.stored_credential(account).map_err(|_| failed(false))?;
    if local_only
        || matches!(
            stored,
            StoredCredential::Absent | StoredCredential::Unusable
        )
    {
        app.logout_local(account).map_err(|_| failed(true))?;
        return Ok(!matches!(stored, StoredCredential::Absent));
    }
    require_current(app, account)?;
    match app.revoke(account).map_err(|_| failed(true))? {
        Revocation::Revoked(_) => Ok(false),
        Revocation::Unusable => {
            app.logout_local(account).map_err(|_| failed(true))?;
            Ok(true)
        }
    }
}

fn failed(outcome_unknown: bool) -> ReadRefusal {
    ReadRefusal::AccountOperationFailed { outcome_unknown }
}
