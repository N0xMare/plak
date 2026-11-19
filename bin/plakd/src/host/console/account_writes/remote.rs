//! Host account writes when a credential gateway holds the store.
//!
//! The gateway is the writer. These calls do not open the local subscription
//! store or the API-key shelf.

use plakd_wire_types::console::{
    HostRequest, HostResponse, ReadAction, ReadRefusal, ReadRemedy, ReadScope, SecretInput,
    ValidationCode,
};
use plakous::gateway::protocol::admin::{AdminRefusal, AdminRequest, AdminResponse, LogoutMode};
use plakous::gateway::protocol::facts::CredentialStatus;
use plakous::gateway::protocol::{GatewayAccount, SecretText};

use super::{AccountError, failed, validation};
use crate::gateway::AdminLink;

pub(super) fn execute(
    link: &AdminLink,
    selected: &str,
    request: HostRequest,
) -> Result<HostResponse, AccountError> {
    let account = GatewayAccount::new(selected.to_owned()).map_err(|_| failed(false))?;
    let mut remote_grant_may_remain = None;
    match request {
        HostRequest::AccountKeySet { secret, .. } => {
            changed(
                link,
                AdminRequest::KeySet {
                    account,
                    secret: key_secret(secret)?,
                },
            )?;
        }
        HostRequest::AccountKeyClear { .. } => {
            changed(link, AdminRequest::KeyClear { account })?;
        }
        HostRequest::AccountRemove { .. } => {
            changed(link, AdminRequest::Remove { account })?;
        }
        HostRequest::AccountLogout { local_only, .. } => {
            let mode = if local_only {
                LogoutMode::Local
            } else {
                LogoutMode::Remote
            };
            remote_grant_may_remain = changed(link, AdminRequest::Logout { account, mode })?;
        }
        HostRequest::AccountRefresh { .. } => refresh(link, account)?,
        _ => return Err(validation("request", ValidationCode::InvalidAction).into()),
    }
    Ok(HostResponse::AccountChanged {
        account_id: selected.to_owned(),
        remote_grant_may_remain,
    })
}

fn key_secret(secret: SecretInput) -> Result<SecretText, AccountError> {
    let text = secret.expose();
    if text.len() > crate::presentation::MAX_REMOTE_AUTHORIZATION_INPUT_BYTES
        || text.trim().is_empty()
        || text.chars().any(char::is_control)
    {
        return Err(validation("secret", ValidationCode::InvalidValue).into());
    }
    SecretText::new(secret.into_zeroizing()).map_err(|_| failed(false))
}

fn changed(link: &AdminLink, request: AdminRequest) -> Result<Option<bool>, AccountError> {
    match link.call(&request).map_err(|_| failed(true))? {
        AdminResponse::Changed {
            remote_grant_may_remain,
            ..
        } => Ok(remote_grant_may_remain),
        AdminResponse::Refused { refusal } => Err(from_refusal(&refusal)),
        _ => Err(failed(false)),
    }
}

fn refresh(link: &AdminLink, account: GatewayAccount) -> Result<(), AccountError> {
    match link
        .call(&AdminRequest::Refresh { account })
        .map_err(|_| failed(true))?
    {
        AdminResponse::Refreshed { facts }
            if facts.credential.status == CredentialStatus::Ready =>
        {
            Ok(())
        }
        AdminResponse::Refreshed { .. } => Err(failed(true)),
        AdminResponse::Refused { refusal } => Err(from_refusal(&refusal)),
        _ => Err(failed(false)),
    }
}

fn from_refusal(refusal: &AdminRefusal) -> AccountError {
    match refusal {
        AdminRefusal::OutcomeUnknown {} => failed(true),
        AdminRefusal::EnvironmentOverride { variable } => ReadRefusal::EnvironmentOverride {
            proven_no_effect: true,
            variable: variable.as_str().to_owned(),
            remedies: vec![ReadRemedy {
                action: ReadAction::RestartHostWithoutOverride,
                account_id: None,
            }],
        }
        .into(),
        AdminRefusal::AccountBusy {} => ReadRefusal::Busy {
            scope: ReadScope::Accounts,
            retry_after_ms: 1_000,
        }
        .into(),
        _ => failed(false),
    }
}
