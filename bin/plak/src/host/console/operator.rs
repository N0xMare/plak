//! Process-local consent and mutation admission; never persisted authority.

use super::{Console, HostPrincipal, HostRequestContext, refused, validation};
use plakd_wire_types::console::{
    HostResponse, OperatorAction, OperatorConsent, ReadRefusal, ValidationCode,
};
use plakous_host_protocol::wire::{ConsoleCapability, RequestRefusal};
use std::collections::BTreeMap;
use std::sync::PoisonError;
use std::time::{Duration, Instant};

const TTL: Duration = Duration::from_secs(60);
const PRINCIPALS: usize = 1024;

struct Grant {
    owner: HostRequestContext,
    action: OperatorAction,
    expires: Instant,
    confirmed: bool,
}

#[derive(Default)]
pub(super) struct State {
    grants: BTreeMap<String, Grant>,
    quotas: BTreeMap<HostPrincipal, Quota>,
}

struct Quota {
    since: Instant,
    credentials: u8,
    config: u8,
    active: bool,
}

pub(super) fn credentials(action: &OperatorAction) -> bool {
    !matches!(
        action,
        OperatorAction::ConfigPatch { .. }
            | OperatorAction::TemplatePut { .. }
            | OperatorAction::ProfilePut { .. }
    )
}

pub(super) fn account(action: &OperatorAction) -> Option<&str> {
    match action {
        OperatorAction::AccountKeySet { account_id }
        | OperatorAction::AccountKeyClear { account_id }
        | OperatorAction::AccountRefresh { account_id }
        | OperatorAction::AccountLogout { account_id, .. }
        | OperatorAction::AccountRemove { account_id }
        | OperatorAction::AccountLoginStart { account_id, .. } => Some(account_id),
        _ => None,
    }
}

impl Console {
    pub(super) fn write_capability(
        &self,
        principal: &HostPrincipal,
        action: &OperatorAction,
    ) -> Result<(), RequestRefusal> {
        let (enabled, capability) = if credentials(action) {
            (
                self.switches.web_credentials_write,
                ConsoleCapability::CredentialsWrite,
            )
        } else {
            (
                self.switches.web_config_write,
                ConsoleCapability::ConfigWrite,
            )
        };
        if matches!(principal, HostPrincipal::Socket(_)) || enabled {
            Ok(())
        } else {
            Err(RequestRefusal::CapabilityDisabled { capability })
        }
    }

    pub(super) fn prepare(
        &self,
        context: HostRequestContext,
        action: OperatorAction,
    ) -> Result<HostResponse, RequestRefusal> {
        self.write_capability(&context.principal, &action)?;
        if let Err(reason) = validate(&action) {
            return Ok(refused(reason));
        }
        let now = Instant::now();
        let mut state = self.operator.lock().unwrap_or_else(PoisonError::into_inner);
        state.grants.retain(|_, grant| now < grant.expires);
        if state.grants.len() >= 4096
            || state
                .grants
                .values()
                .filter(|grant| grant.owner.principal == context.principal)
                .count()
                >= 4
        {
            return Err(RequestRefusal::RateLimited {
                retry_after_ms: 60_000,
            });
        }
        let nonce = uuid::Uuid::new_v4().to_string();
        let summary = summary(&action);
        state.grants.insert(
            nonce.clone(),
            Grant {
                owner: context,
                action: action.clone(),
                expires: now + TTL,
                confirmed: false,
            },
        );
        Ok(HostResponse::OperatorPrepared {
            consent: OperatorConsent {
                action,
                summary,
                nonce,
                expires_in_ms: 60_000,
            },
        })
    }

    pub(super) fn confirm(
        &self,
        context: &HostRequestContext,
        nonce: &str,
    ) -> Result<HostResponse, RequestRefusal> {
        let now = Instant::now();
        let mut state = self.operator.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(grant) = state.grants.get(nonce) else {
            return Ok(refused(ReadRefusal::ConfirmationExpired {}));
        };
        if !same_owner(&grant.owner, context) || grant.confirmed || now >= grant.expires {
            return Ok(refused(ReadRefusal::ConfirmationExpired {}));
        }
        self.write_capability(&context.principal, &grant.action)?;
        let Some(mut grant) = state.grants.remove(nonce) else {
            return Ok(refused(ReadRefusal::ConfirmationExpired {}));
        };
        grant.confirmed = true;
        let expires_in_ms = u64::try_from((grant.expires - now).as_millis()).unwrap_or(0);
        let confirmation = uuid::Uuid::new_v4().to_string();
        state.grants.insert(confirmation.clone(), grant);
        Ok(HostResponse::OperatorConfirmed {
            confirmation,
            expires_in_ms,
        })
    }

    pub(super) fn consume(
        &self,
        context: &HostRequestContext,
        action: &OperatorAction,
        confirmation: &str,
    ) -> Result<(), ReadRefusal> {
        let mut state = self.operator.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(grant) = state.grants.get(confirmation) else {
            return Err(ReadRefusal::ConfirmationRequired {});
        };
        if !same_owner(&grant.owner, context) || !grant.confirmed || &grant.action != action {
            return Err(ReadRefusal::ConfirmationRequired {});
        }
        let grant = state
            .grants
            .remove(confirmation)
            .ok_or(ReadRefusal::ConfirmationRequired {})?;
        if Instant::now() >= grant.expires {
            return Err(ReadRefusal::ConfirmationExpired {});
        }
        Ok(())
    }

    pub(super) fn admit_mutation(
        &self,
        principal: &HostPrincipal,
        credential: bool,
    ) -> Result<(), RequestRefusal> {
        let now = Instant::now();
        let mut state = self.operator.lock().unwrap_or_else(PoisonError::into_inner);
        state
            .quotas
            .retain(|_, quota| quota.active || now.duration_since(quota.since) < TTL);
        if !state.quotas.contains_key(principal) && state.quotas.len() >= PRINCIPALS {
            return Err(RequestRefusal::RateLimited {
                retry_after_ms: 60_000,
            });
        }
        let quota = state.quotas.entry(principal.clone()).or_insert(Quota {
            since: now,
            credentials: 0,
            config: 0,
            active: false,
        });
        if !quota.active && now.duration_since(quota.since) >= TTL {
            *quota = Quota {
                since: now,
                credentials: 0,
                config: 0,
                active: false,
            };
        }
        let count = if credential {
            &mut quota.credentials
        } else {
            &mut quota.config
        };
        if quota.active || *count >= if credential { 10 } else { 30 } {
            return Err(RequestRefusal::RateLimited {
                retry_after_ms: if quota.active {
                    1_000
                } else {
                    u64::try_from(
                        TTL.saturating_sub(now.duration_since(quota.since))
                            .as_millis(),
                    )
                    .unwrap_or(60_000)
                },
            });
        }
        *count += 1;
        quota.active = true;
        Ok(())
    }

    pub(super) fn end_mutation(&self, principal: &HostPrincipal) {
        if let Some(quota) = self
            .operator
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .quotas
            .get_mut(principal)
        {
            quota.active = false;
        }
    }

    pub fn disconnect(&self, principal: &HostPrincipal, connection: u64, generation: u64) {
        self.operator
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .grants
            .retain(|_, grant| {
                &grant.owner.principal != principal
                    || grant.owner.connection_id != connection
                    || grant.owner.host_generation != generation
            });
    }
}

fn same_owner(left: &HostRequestContext, right: &HostRequestContext) -> bool {
    left.principal == right.principal
        && left.connection_id == right.connection_id
        && left.host_generation == right.host_generation
}

fn validate(action: &OperatorAction) -> Result<(), ReadRefusal> {
    if let Some(id) = account(action) {
        let account = super::accounts::exact(id)?;
        let valid = match action {
            OperatorAction::AccountKeySet { .. } | OperatorAction::AccountKeyClear { .. } => {
                account.vertical().is_api()
            }
            OperatorAction::AccountRemove { .. } => account.is_named(),
            _ => !account.vertical().is_api(),
        };
        if !valid {
            return Err(validation("action", ValidationCode::InvalidAction));
        }
    }
    match action {
        OperatorAction::TemplatePut { id, if_match }
        | OperatorAction::ProfilePut { id, if_match } => {
            if crate::home::secrets::validate_secret_name(id).is_err() || if_match.len() != 64 {
                return Err(validation("id", ValidationCode::InvalidId));
            }
        }
        OperatorAction::ConfigPatch { if_match, keys } => {
            if if_match.len() != 64
                || keys.is_empty()
                || keys.len() > 64
                || keys
                    .iter()
                    .any(|key| !super::definitions::editable_key(key))
            {
                return Err(validation("keys", ValidationCode::ReadOnly));
            }
        }
        OperatorAction::AccountLoginStart {
            notice_revision, ..
        } if notice_revision.len() > 128 => {
            return Err(validation("notice_revision", ValidationCode::InvalidValue));
        }
        _ => {}
    }
    Ok(())
}

fn summary(action: &OperatorAction) -> String {
    if let Some(id) = account(action) {
        return match action {
            OperatorAction::AccountKeySet { .. } | OperatorAction::AccountKeyClear { .. } => {
                format!(
                    "Change API key for {id} on this serving host. Its binding fingerprint changes; existing sessions require explicit rebind. An environment override refuses without changing files."
                )
            }
            OperatorAction::AccountLoginStart {
                notice_revision,
                replace_newer,
                ..
            } => format!(
                "Log in to {id} on this host under notice {notice_revision}. Replace newer-build credentials: {replace_newer}. Replaces this selected account for all builds and homes of this user on this machine; other machines are separate login authorities."
            ),
            OperatorAction::AccountLogout { local_only, .. } => format!(
                "Log out {id} on this serving machine. Local retirement only: {local_only}. A local retirement does not revoke the remote grant; failed revocation is not success."
            ),
            OperatorAction::AccountRefresh { .. } => format!(
                "Explicitly refresh {id} on this serving machine using its claimed refresh workflow. This is not a status reread."
            ),
            _ => format!(
                "Remove named account {id} on this serving host. No default or other named account is selected as a fallback."
            ),
        };
    }
    "Publish allowlisted home configuration on this serving host. Templates can select tools and execute on_mint/env on a runner; root and roster choices grant deferred execution. Existing epochs and frozen child rosters are unchanged. No automatic rebind, carry or restart.".into()
}

pub(super) fn audit(
    context: &HostRequestContext,
    action: &'static str,
    target: Option<&str>,
    category: &'static str,
) {
    let (face, login) = match &context.principal {
        HostPrincipal::Socket(uid) => ("socket", uid.to_string()),
        HostPrincipal::Web(login) => ("web", login.clone()),
    };
    eprintln!(
        "{}",
        serde_json::json!({"audit":"operator", "time_ms":super::clock::now_ms(), "login":login, "face":face, "connection_id":context.connection_id, "request_id":context.request_id, "action":action, "target":target, "category":category})
    );
}

#[cfg(test)]
mod tests;
