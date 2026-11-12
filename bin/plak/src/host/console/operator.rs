//! Process-local consent and mutation admission; never persisted authority.

use super::{Console, HostPrincipal, HostRequestContext, refused, validation};
use plakd_wire_types::console::{
    HostResponse, OperatorAction, OperatorConsent, ReadRefusal, ValidationCode,
};
use plakous_host_protocol::wire::{ConsoleCapability, RequestRefusal};
use std::collections::{BTreeMap, VecDeque};
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

pub(super) struct ReservedConsent {
    token: String,
    grant: Grant,
}

#[derive(Default)]
pub(super) struct State {
    grants: BTreeMap<String, Grant>,
    quotas: BTreeMap<HostPrincipal, Quota>,
}

#[derive(Default)]
struct Quota {
    credentials: VecDeque<Instant>,
    config: VecDeque<Instant>,
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
    pub(super) async fn prepare_operator(
        &self,
        context: HostRequestContext,
        action: OperatorAction,
    ) -> Result<HostResponse, RequestRefusal> {
        self.write_capability(&context.principal, &action)?;
        if let Err(reason) = validate(&action) {
            return Ok(refused(reason));
        }
        let detail = if let OperatorAction::AccountLoginStart {
            account_id,
            notice_revision,
            ..
        } = &action
        {
            let account = match super::accounts::exact(account_id) {
                Ok(account) => account,
                Err(reason) => return Ok(refused(reason)),
            };
            let Some(provider) = account.vertical().capability().native_provider() else {
                return Ok(refused(validation(
                    "account_id",
                    ValidationCode::InvalidAccount,
                )));
            };
            let Ok(notice) = plakous::subscriptions::NativeSubscriptions::notice(provider) else {
                return Ok(refused(validation(
                    "notice_revision",
                    ValidationCode::InvalidValue,
                )));
            };
            let now = u64::try_from(super::clock::now_ms()).unwrap_or(u64::MAX) / 1_000;
            if notice.notice_revision() != notice_revision
                || notice.is_killed()
                || now >= notice.expires_at()
            {
                return Ok(refused(validation(
                    "notice_revision",
                    ValidationCode::InvalidValue,
                )));
            }
            let subscriptions = match super::accounts::reader(&self.state)
                .and_then(|reader| reader.subscriptions_for(&account))
            {
                Ok(subscriptions) => subscriptions,
                Err(reason) => return Ok(refused(reason)),
            };
            let Ok(status) =
                crate::commands::support::offline_subscription_status(&subscriptions, provider)
                    .await
            else {
                return Ok(refused(ReadRefusal::AccountOperationFailed {}));
            };
            let newer =
                status.credential == crate::presentation::CredentialState::WrittenByNewerBuild;
            let identity = subscriptions
                .upstream_account_fingerprint(provider)
                .await
                .ok()
                .flatten()
                .map_or_else(
                    || "unknown or signed out".into(),
                    |identity| format!("{identity:?}"),
                );
            Some(format!(
                " Experimental integration {}. Provenance: {}. Source revision: {}. Reviewed: {}; expires: {}. Stored credential written by a newer build: {}. Current safe upstream identity digest: {}.",
                notice.profile_id(),
                notice.provenance(),
                notice.source_revision(),
                notice.reviewed_at(),
                notice.expires_at(),
                newer,
                identity
            ))
        } else {
            None
        };
        let mut response = self.prepare(context, action)?;
        if let (HostResponse::OperatorPrepared { consent }, Some(detail)) = (&mut response, detail)
        {
            consent.summary.push_str(&detail);
        }
        Ok(response)
    }

    pub(super) fn write_capability(
        &self,
        principal: &HostPrincipal,
        action: &OperatorAction,
    ) -> Result<(), RequestRefusal> {
        if matches!(
            action,
            OperatorAction::TerminalOpen { .. }
                | OperatorAction::TerminalAttach { .. }
                | OperatorAction::TerminalRelease { .. }
        ) {
            return self.terminal_capability(principal);
        }
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
        state.check_grant(context, action, confirmation)?;
        state.grants.remove(confirmation);
        Ok(())
    }

    pub(super) fn reserve_consent(
        &self,
        context: &HostRequestContext,
        action: &OperatorAction,
        confirmation: &str,
    ) -> Result<ReservedConsent, ReadRefusal> {
        let mut state = self.operator.lock().unwrap_or_else(PoisonError::into_inner);
        state.check_grant(context, action, confirmation)?;
        let grant = state
            .grants
            .remove(confirmation)
            .ok_or(ReadRefusal::ConfirmationRequired {})?;
        Ok(ReservedConsent {
            token: confirmation.into(),
            grant,
        })
    }

    pub(super) fn restore_consent(&self, reservation: ReservedConsent) {
        let mut state = self.operator.lock().unwrap_or_else(PoisonError::into_inner);
        if Instant::now() < reservation.grant.expires && state.grants.len() < 4096 {
            state
                .grants
                .entry(reservation.token)
                .or_insert(reservation.grant);
        }
    }

    pub(super) fn admit_mutation(
        &self,
        context: &HostRequestContext,
        action: &OperatorAction,
        confirmation: &str,
    ) -> Result<(), super::account_writes::MutationRefusal> {
        use super::account_writes::MutationRefusal;
        let mut state = self.operator.lock().unwrap_or_else(PoisonError::into_inner);
        state
            .check_grant(context, action, confirmation)
            .map_err(MutationRefusal::Consent)?;
        state
            .admit_mutation(&context.principal, credentials(action))
            .map_err(MutationRefusal::Quota)?;
        state.grants.remove(confirmation);
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
        self.terminal_disconnect(&HostRequestContext {
            principal: principal.clone(),
            connection_id: connection,
            host_generation: generation,
            request_id: 0,
        });
        self.logins
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .disconnect(principal, connection, generation);
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

impl State {
    fn check_grant(
        &self,
        context: &HostRequestContext,
        action: &OperatorAction,
        confirmation: &str,
    ) -> Result<(), ReadRefusal> {
        let Some(grant) = self.grants.get(confirmation) else {
            return Err(ReadRefusal::ConfirmationRequired {});
        };
        if !same_owner(&grant.owner, context) || !grant.confirmed || &grant.action != action {
            return Err(ReadRefusal::ConfirmationRequired {});
        }
        if Instant::now() >= grant.expires {
            return Err(ReadRefusal::ConfirmationExpired {});
        }
        Ok(())
    }

    fn admit_mutation(
        &mut self,
        principal: &HostPrincipal,
        credential: bool,
    ) -> Result<(), RequestRefusal> {
        let now = Instant::now();
        self.quotas.retain(|_, quota| {
            quota
                .credentials
                .retain(|time| now.duration_since(*time) < TTL);
            quota.config.retain(|time| now.duration_since(*time) < TTL);
            quota.active || !quota.credentials.is_empty() || !quota.config.is_empty()
        });
        if !self.quotas.contains_key(principal) && self.quotas.len() >= PRINCIPALS {
            return Err(RequestRefusal::RateLimited {
                retry_after_ms: 60_000,
            });
        }
        let quota = self.quotas.entry(principal.clone()).or_default();
        let count = if credential {
            &mut quota.credentials
        } else {
            &mut quota.config
        };
        if quota.active || count.len() >= if credential { 10 } else { 30 } {
            return Err(RequestRefusal::RateLimited {
                retry_after_ms: if quota.active {
                    1_000
                } else {
                    u64::try_from(
                        TTL.saturating_sub(now.duration_since(*count.front().unwrap_or(&now)))
                            .as_millis(),
                    )
                    .unwrap_or(60_000)
                },
            });
        }
        count.push_back(now);
        quota.active = true;
        Ok(())
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
        if !account.vertical().capability().compiled {
            return Err(super::unavailable(
                plakd_wire_types::console::ReadScope::Accounts,
                plakd_wire_types::console::UnavailableCode::NotCompiled,
            ));
        }
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
            if crate::product_config::validate_template_name(id).is_err() || if_match.len() != 64 {
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
    if matches!(
        action,
        OperatorAction::TerminalOpen { .. } | OperatorAction::TerminalAttach { .. }
    ) {
        return "Grant interactive shell control on the selected runner as its user. The shell is not confined to the workspace and may read that user's files, network and credentials. Input acceptance is not command completion. A confirmed takeover revokes the previous controller; never replay uncertain input.".into();
    }
    if matches!(action, OperatorAction::TerminalRelease { .. }) {
        return "Close only your operator terminals and wait for cleanup before releasing this session. Another principal's terminal is never implicitly closed. Unproven termination retains its pin.".into();
    }
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

#[cfg(test)]
mod tests;
