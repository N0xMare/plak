//! Account writes reuse the CLI account service without terminal presentation.

mod login;
mod remote;
#[cfg(test)]
mod tests;
pub(super) use login::Logins;

use super::{Console, HostPrincipal, HostRequestContext, operator, refused, validation};
use plakd_wire_types::console::{
    HostRequest, HostResponse, OperatorAction, ReadRefusal, ValidationCode,
};
use plakous_host_protocol::wire::RequestRefusal;
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

#[derive(Clone, Debug)]
pub(super) enum AccountError {
    Refused(ReadRefusal),
    Unknown,
}
impl From<ReadRefusal> for AccountError {
    fn from(reason: ReadRefusal) -> Self {
        Self::Refused(reason)
    }
}
impl AccountError {
    fn response(self) -> HostResponse {
        match self {
            Self::Refused(reason) => refused(reason),
            Self::Unknown => HostResponse::OperationOutcomeUnknown {},
        }
    }
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
        self.admit_mutation(context, action, confirmation)?;
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
            self.audit(
                &owner,
                "account_write",
                target.as_deref(),
                if result.is_ok() { "success" } else { "error" },
                None,
            );
            result.unwrap_or_else(AccountError::response)
        })
        .map_err(AccountError::response);
        let result = match result {
            Ok(result) => result.await,
            Err(response) => return Ok(response),
        }
        .unwrap_or_else(|_| HostResponse::OperationOutcomeUnknown {});
        Ok(result)
    }

    fn execute_account(&self, request: HostRequest) -> Result<HostResponse, AccountError> {
        let selected = action(&request)
            .and_then(|action| operator::account(&action).map(str::to_owned))
            .ok_or_else(|| failed(false))?;
        super::accounts::exact(&selected)?;
        let link = self.custody.admin().ok_or_else(|| failed(false))?;
        remote::execute(link, &selected, request)
    }
}

fn worker<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<tokio::sync::oneshot::Receiver<T>, AccountError> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("operator-account".into())
        .spawn(move || {
            let _ = sender.send(work());
        })
        .map_err(|_| failed(false))?;
    Ok(receiver)
}

fn failed(outcome_unknown: bool) -> AccountError {
    if outcome_unknown {
        AccountError::Unknown
    } else {
        AccountError::Refused(ReadRefusal::AccountOperationFailed {})
    }
}
