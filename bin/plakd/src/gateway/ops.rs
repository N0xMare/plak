//! Account verbs against today's `PlakousApplication`. Caller keys, flags and
//! drain never come through here. Login completion for a device code is polled:
//! the provider wait runs on the application executor, and each poll only
//! observes it.

use plakous::gateway::protocol::admin::{
    AccountStatusRow, AdminRefusal, AdminRequest, AdminResponse, LoginMethod, LoginPhase,
    LogoutMode,
};
use plakous::gateway::protocol::facts::{
    AccountFacts, CredentialFacts, CredentialStatus, KeyFacts, KeySource, RefreshRequest,
    WireIdentity,
};
use plakous::gateway::protocol::refusal::GatewayRefusal;
use plakous::gateway::protocol::{BoundedText, UuidText};
use plakous::gateway::protocol::{GatewayAccount, SecretText};
use plakous::subscriptions::{
    NativeAuthStatus, NativeLoginCompletion, NativeLoginMode, NativeSubscriptionError,
};
use plakous_hosted::account::AccountRef;
use tokio::sync::oneshot;

use super::bridge::{AccountBridge, AttemptBook};
use crate::commands::application::host::{
    HostAccountError, logout_host_account, prepare_host_login,
};
use crate::commands::application::{
    AccountOps as _, LoginOutcome, LoginResponse, PlakousApplication,
};
use crate::home::paths::PlakPaths;
use crate::presentation::CredentialState;

struct LiveLogin {
    method: LoginMethod,
    pending: Option<(AccountRef, plakous::subscriptions::PendingNativeLogin)>,
    device: Option<DeviceLogin>,
}

struct DeviceLogin {
    result: oneshot::Receiver<Result<NativeAuthStatus, NativeSubscriptionError>>,
    task: tokio::task::AbortHandle,
}

impl Drop for DeviceLogin {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl DeviceLogin {
    fn outcome(&mut self, attempt: &UuidText) -> Option<AdminResponse> {
        Some(match self.result.try_recv() {
            Ok(Ok(_)) => progress(attempt.clone(), LoginPhase::Succeeded),
            Ok(Err(NativeSubscriptionError::AccountReplacementRefused)) => {
                refused(AdminRefusal::LocalAdminRequired {})
            }
            Ok(Err(_)) => refused(AdminRefusal::ProviderRejected {}),
            Err(oneshot::error::TryRecvError::Closed) => refused(AdminRefusal::OutcomeUnknown {}),
            Err(oneshot::error::TryRecvError::Empty) => return None,
        })
    }
}

pub(crate) struct OpsBridge {
    app: PlakousApplication,
    home: PlakPaths,
    attempts: AttemptBook<LiveLogin>,
    instance: UuidText,
}

impl OpsBridge {
    pub(crate) fn new(app: PlakousApplication, home: PlakPaths, instance: UuidText) -> Self {
        Self {
            app,
            home,
            attempts: AttemptBook::new(),
            instance,
        }
    }

    fn account(text: &GatewayAccount) -> Result<AccountRef, AdminRefusal> {
        text.as_str()
            .parse()
            .map_err(|_| AdminRefusal::WrongAccountClass {})
    }

    fn facts_for(&mut self, account: &GatewayAccount) -> Result<AccountFacts, GatewayRefusal> {
        let parsed = Self::account(account).map_err(|_| GatewayRefusal::AccountUnknown {})?;
        // An API-key account has no login state: it is ready exactly when a
        // key resolves.
        let (status, key) = if parsed.vertical().is_api() {
            let key = key_facts(account.as_str(), &self.home)?;
            let status = match key.source {
                KeySource::Environment | KeySource::File => CredentialStatus::Ready,
                KeySource::Missing => CredentialStatus::SignedOut,
            };
            (status, Some(key))
        } else {
            let status = self
                .app
                .offline_status(&parsed)
                .map_err(|_| GatewayRefusal::Internal {})?;
            (credential_status(status.credential), None)
        };
        Ok(AccountFacts {
            account: account.clone(),
            instance: self.instance.clone(),
            observed_at_unix_ms: super::callers::now_ms(),
            credential: CredentialFacts {
                status,
                generation: None,
                access_expires_at_unix_ms: None,
                selection_epoch_digest: None,
            },
            identity: None,
            wire_identity: WireIdentity::default(),
            profile: None,
            key,
        })
    }

    fn accounts(&mut self) -> Result<Vec<GatewayAccount>, AdminRefusal> {
        self.app
            .accounts()
            .map_err(|_| AdminRefusal::Unavailable {})?
            .into_iter()
            .map(|account| {
                GatewayAccount::new(account.to_string()).map_err(|_| AdminRefusal::Unavailable {})
            })
            .collect()
    }

    fn status(
        &mut self,
        accounts: Vec<GatewayAccount>,
    ) -> Result<Vec<AccountStatusRow>, AdminRefusal> {
        let selected = if accounts.is_empty() {
            self.accounts()?
        } else {
            accounts
        };
        self.facts(&selected)
            .map_err(|refusal| match refusal {
                GatewayRefusal::AccountUnknown {} => AdminRefusal::WrongAccountClass {},
                _ => AdminRefusal::Unavailable {},
            })
            .map(|facts| {
                facts
                    .into_iter()
                    .map(|facts| AccountStatusRow {
                        facts,
                        last_refresh: None,
                    })
                    .collect()
            })
    }
}

impl AccountBridge for OpsBridge {
    fn call(&mut self, local_admin: bool, request: AdminRequest) -> AdminResponse {
        match request {
            AdminRequest::List {} => self
                .accounts()
                .map_or_else(refused, |accounts| AdminResponse::Accounts { accounts }),
            AdminRequest::Status { accounts } => self
                .status(accounts)
                .map_or_else(refused, |rows| AdminResponse::Status { rows }),
            AdminRequest::KeySet { account, secret } => self.key_set(&account, secret.expose()),
            AdminRequest::KeyClear { account } => self.key_clear(&account),
            AdminRequest::Logout { account, mode } => self.logout(&account, mode),
            AdminRequest::Refresh { account } => self.refresh_account(&account),
            AdminRequest::Remove { account } => self.remove(&account),
            AdminRequest::LoginStart {
                account,
                notice_revision,
                replace_newer,
            } => self.login_start(
                local_admin,
                account,
                notice_revision.as_str(),
                replace_newer,
            ),
            AdminRequest::LoginComplete { attempt, pasted } => {
                self.login_complete(&attempt, pasted.expose())
            }
            AdminRequest::LoginPoll { attempt } => self.login_poll(&attempt),
            AdminRequest::LoginCancel { attempt } => self.login_cancel(&attempt),
            AdminRequest::Allowance { account } => self.allowance(&account),
            AdminRequest::Disable { .. }
            | AdminRequest::Enable { .. }
            | AdminRequest::CallerAdd { .. }
            | AdminRequest::CallerList {}
            | AdminRequest::CallerRotate { .. }
            | AdminRequest::CallerRemove { .. }
            | AdminRequest::Drain {} => refused(AdminRefusal::Unavailable {}),
        }
    }

    fn facts(&mut self, accounts: &[GatewayAccount]) -> Result<Vec<AccountFacts>, GatewayRefusal> {
        accounts
            .iter()
            .map(|account| self.facts_for(account))
            .collect()
    }

    fn refresh(&mut self, request: &RefreshRequest) -> Result<AccountFacts, GatewayRefusal> {
        let account =
            Self::account(&request.account).map_err(|_| GatewayRefusal::AccountUnknown {})?;
        let due = match &request.mode {
            plakous::gateway::protocol::facts::RefreshMode::IfDue { .. } => {
                self.app.access_token_expired(&account).unwrap_or(true)
            }
            plakous::gateway::protocol::facts::RefreshMode::AfterRejection { .. } => true,
        };
        if due {
            self.app
                .refresh_now(&account)
                .map_err(|_| GatewayRefusal::Internal {})?;
        }
        self.facts_for(&request.account)
    }
}

impl OpsBridge {
    /// Whether the slot holds a login: anything but signed out or revoked.
    /// A record this build cannot read counts as signed in.
    fn signed_in(&mut self, account: &AccountRef) -> bool {
        self.app.offline_status(account).map_or(true, |status| {
            !matches!(
                status.credential,
                CredentialState::SignedOut | CredentialState::Revoked
            )
        })
    }

    fn key_set(&mut self, account: &GatewayAccount, secret: &str) -> AdminResponse {
        if let Some(variable) = crate::home::secrets::overriding_env_var(account.as_str()) {
            return match BoundedText::new(variable.to_owned()) {
                Ok(variable) => refused(AdminRefusal::EnvironmentOverride { variable }),
                Err(_) => refused(AdminRefusal::Unavailable {}),
            };
        }
        match crate::home::secrets::set_api_key(
            &self.home,
            account.as_str(),
            zeroize::Zeroizing::new(secret.to_owned()),
        ) {
            Ok(()) => changed(account.clone(), None),
            Err(error) => {
                let text = error.to_string();
                if text.contains("not an API-key account") {
                    refused(AdminRefusal::WrongAccountClass {})
                } else {
                    refused(AdminRefusal::ProviderRejected {})
                }
            }
        }
    }

    fn key_clear(&mut self, account: &GatewayAccount) -> AdminResponse {
        if let Some(variable) = crate::home::secrets::overriding_env_var(account.as_str()) {
            return match BoundedText::new(variable.to_owned()) {
                Ok(variable) => refused(AdminRefusal::EnvironmentOverride { variable }),
                Err(_) => refused(AdminRefusal::Unavailable {}),
            };
        }
        match crate::home::secrets::clear_api_key(&self.home, account.as_str()) {
            Ok(_) => changed(account.clone(), None),
            Err(error) if error.to_string().contains("not an API-key account") => {
                refused(AdminRefusal::WrongAccountClass {})
            }
            Err(_) => refused(AdminRefusal::Unavailable {}),
        }
    }

    fn logout(&mut self, account: &GatewayAccount, mode: LogoutMode) -> AdminResponse {
        let Ok(parsed) = Self::account(account) else {
            return refused(AdminRefusal::WrongAccountClass {});
        };
        if parsed.vertical().is_api() {
            return refused(AdminRefusal::WrongAccountClass {});
        }
        match logout_host_account(&mut self.app, &parsed, mode == LogoutMode::Local) {
            Ok(remain) => changed(account.clone(), remain),
            Err(HostAccountError::Refused(_cause)) => refused(AdminRefusal::Unavailable {}),
            Err(HostAccountError::OutcomeUnknown(_cause)) => {
                refused(AdminRefusal::OutcomeUnknown {})
            }
        }
    }

    fn refresh_account(&mut self, account: &GatewayAccount) -> AdminResponse {
        let Ok(parsed) = Self::account(account) else {
            return refused(AdminRefusal::WrongAccountClass {});
        };
        if self.app.refresh_now(&parsed).is_err() {
            return refused(AdminRefusal::Unavailable {});
        }
        match self.facts_for(account) {
            Ok(facts) => AdminResponse::Refreshed {
                facts: Box::new(facts),
            },
            Err(_) => refused(AdminRefusal::Unavailable {}),
        }
    }

    fn remove(&mut self, account: &GatewayAccount) -> AdminResponse {
        let Ok(parsed) = Self::account(account) else {
            return refused(AdminRefusal::WrongAccountClass {});
        };
        match self.app.remove_account(&parsed) {
            Ok(()) => changed(account.clone(), None),
            Err(error) => {
                let text = error.to_string();
                if text.contains("signed") {
                    refused(AdminRefusal::AccountSignedIn {})
                } else if text.contains("default") {
                    refused(AdminRefusal::DefaultNotRemovable {})
                } else {
                    refused(AdminRefusal::Unavailable {})
                }
            }
        }
    }

    fn login_start(
        &mut self,
        local_admin: bool,
        account: GatewayAccount,
        notice_revision: &str,
        replace_newer: bool,
    ) -> AdminResponse {
        let Ok(parsed) = Self::account(&account) else {
            return refused(AdminRefusal::WrongAccountClass {});
        };
        if let Err(refusal) = self.attempts.check_start(&account) {
            return refused(refusal);
        }
        // Captured before the login can retire an unusable record: a remote
        // admin may only re-login the account this slot held (spec 04 §2.11).
        let keep_account = !local_admin && self.signed_in(&parsed);
        if let Err(error) =
            prepare_host_login(&mut self.app, &parsed, notice_revision, replace_newer)
        {
            self.app.end_login_attempt(&parsed);
            return refused(login_prep(error.message()));
        }
        let challenge = self.app.begin_login(&parsed, NativeLoginMode::Remote);
        self.app.end_login_attempt(&parsed);
        let Ok(mut challenge) = challenge else {
            return refused(AdminRefusal::ProviderRejected {});
        };
        if keep_account {
            challenge.pending.1.keep_signed_in_account();
        }
        let method = match challenge.completion {
            NativeLoginCompletion::DeviceCode => LoginMethod::DeviceCode,
            NativeLoginCompletion::AuthorizationCodeOrRedirectUrl => LoginMethod::Paste,
            NativeLoginCompletion::Automatic => {
                return refused(AdminRefusal::DeviceLoginUnavailable {});
            }
        };
        let Ok(authorize_url) = SecretText::new(zeroize::Zeroizing::new(challenge.authorize_url))
        else {
            return refused(AdminRefusal::Unavailable {});
        };
        let user_code = challenge.device_code.as_ref().and_then(|code| {
            SecretText::new(zeroize::Zeroizing::new(code.user_code.as_str().to_owned())).ok()
        });
        let expires_in_ms = challenge
            .device_code
            .as_ref()
            .map(|code| u64::try_from(code.expires_in.as_millis()).unwrap_or(0))
            .unwrap_or(15 * 60 * 1000);
        let (stored_pending, device) = if method == LoginMethod::DeviceCode {
            let (_account, pending) = challenge.pending;
            let (tx, result) = oneshot::channel();
            let Ok(task) = self.app.executor.block_on(async move {
                let task = tokio::spawn(async move {
                    let _ = tx.send(pending.complete().await);
                });
                task.abort_handle()
            }) else {
                return refused(AdminRefusal::Unavailable {});
            };
            (None, Some(DeviceLogin { result, task }))
        } else {
            (Some(challenge.pending), None)
        };
        let id = match self.attempts.begin(
            account,
            LiveLogin {
                method,
                pending: stored_pending,
                device,
            },
        ) {
            Ok(id) => id,
            Err(refusal) => return refused(refusal),
        };
        AdminResponse::Challenge {
            attempt: id,
            method,
            authorize_url,
            user_code,
            expires_in_ms,
            warning: None,
        }
    }

    fn login_complete(&mut self, attempt: &UuidText, pasted: &str) -> AdminResponse {
        let Ok(found) = self.attempts.get_mut(attempt) else {
            return refused(AdminRefusal::AttemptUnknown {});
        };
        if found.payload.method != LoginMethod::Paste {
            return refused(AdminRefusal::PasteInvalid {});
        }
        let Some((_, pending)) = &found.payload.pending else {
            return refused(AdminRefusal::AttemptUnknown {});
        };
        if pending.validate_remote_input(pasted).is_err() {
            return refused(AdminRefusal::PasteInvalid {});
        }
        let Ok(found) = self.attempts.take(attempt) else {
            return refused(AdminRefusal::AttemptUnknown {});
        };
        let Some(pending) = found.payload.pending else {
            return refused(AdminRefusal::AttemptUnknown {});
        };
        match self
            .app
            .finish_login(pending, LoginResponse::Pasted(pasted))
        {
            Ok(LoginOutcome::Completed(_)) => progress(found.id, LoginPhase::Succeeded),
            Ok(LoginOutcome::Cancelled) | Ok(LoginOutcome::TimedOut) => {
                refused(AdminRefusal::PasteInvalid {})
            }
            Err(error) if replacement_refused(error.message()) => {
                refused(AdminRefusal::LocalAdminRequired {})
            }
            Err(_) => refused(AdminRefusal::ProviderRejected {}),
        }
    }

    fn login_poll(&mut self, attempt: &UuidText) -> AdminResponse {
        if let Err(refusal) = self.attempts.poll_allowed(attempt) {
            return refused(refusal);
        }
        let Ok(found) = self.attempts.get_mut(attempt) else {
            return refused(AdminRefusal::AttemptUnknown {});
        };
        if let Some(outcome) = found
            .payload
            .device
            .as_mut()
            .and_then(|device| device.outcome(attempt))
        {
            let _ = self.attempts.take(attempt);
            return outcome;
        }
        progress(found.id.clone(), LoginPhase::AwaitingInput)
    }

    fn login_cancel(&mut self, attempt: &UuidText) -> AdminResponse {
        let Ok(mut found) = self.attempts.take(attempt) else {
            return refused(AdminRefusal::AttemptUnknown {});
        };
        if let Some(outcome) = found
            .payload
            .device
            .as_mut()
            .and_then(|device| device.outcome(attempt))
        {
            return outcome;
        }
        progress(found.id, LoginPhase::Cancelled)
    }

    fn allowance(&mut self, account: &GatewayAccount) -> AdminResponse {
        let Ok(parsed) = Self::account(account) else {
            return refused(AdminRefusal::WrongAccountClass {});
        };
        if parsed.vertical().is_api() {
            return super::allowance::not_observed(account);
        }
        self.app.subscription_usage(&parsed).map_or_else(
            |_| refused(AdminRefusal::Unavailable {}),
            |observation| super::allowance::subscription(account, observation),
        )
    }
}

fn credential_status(state: CredentialState) -> CredentialStatus {
    match state {
        CredentialState::SignedOut => CredentialStatus::SignedOut,
        CredentialState::Ready => CredentialStatus::Ready,
        CredentialState::RefreshRequired => CredentialStatus::RefreshRequired,
        CredentialState::Refreshing => CredentialStatus::Refreshing,
        CredentialState::Quarantined => CredentialStatus::Quarantined,
        CredentialState::LoginRequired => CredentialStatus::LoginRequired,
        CredentialState::ProfileChanged => CredentialStatus::ProfileChanged,
        CredentialState::WrittenByNewerBuild => CredentialStatus::WrittenByNewerBuild,
        CredentialState::Revoked => CredentialStatus::Revoked,
        CredentialState::Unsupported => CredentialStatus::LoginRequired,
    }
}

fn key_facts(account: &str, home: &PlakPaths) -> Result<KeyFacts, GatewayRefusal> {
    let status =
        crate::home::secrets::key_status(home, account).map_err(|_| GatewayRefusal::Internal {})?;
    let source = match status.source {
        crate::home::secrets::KeySource::Env => KeySource::Environment,
        crate::home::secrets::KeySource::File => KeySource::File,
        crate::home::secrets::KeySource::Missing => KeySource::Missing,
    };
    Ok(KeyFacts {
        source,
        fingerprint: status
            .fingerprint
            .and_then(|text| BoundedText::new(text).ok()),
        env_var: status
            .env_var
            .and_then(|text| BoundedText::new(text.to_owned()).ok()),
    })
}

/// The store refused a login that would replace the slot's account.
fn replacement_refused(message: &str) -> bool {
    message.contains("account_replacement_refused")
}

fn login_prep(message: &str) -> AdminRefusal {
    if message.contains("notice") {
        AdminRefusal::NoticeMismatch {}
    } else if message.contains("newer-build") {
        AdminRefusal::WrittenByNewerBuild {}
    } else if message.contains("inactive") {
        AdminRefusal::Unavailable {}
    } else {
        AdminRefusal::ProviderRejected {}
    }
}

fn changed(account: GatewayAccount, remote_grant_may_remain: Option<bool>) -> AdminResponse {
    AdminResponse::Changed {
        account,
        remote_grant_may_remain,
    }
}

fn refused(refusal: AdminRefusal) -> AdminResponse {
    AdminResponse::Refused { refusal }
}

fn progress(attempt: UuidText, phase: LoginPhase) -> AdminResponse {
    AdminResponse::Progress {
        attempt,
        phase,
        warning: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bridge(directory: &tempfile::TempDir) -> OpsBridge {
        let home = PlakPaths::explicit(directory.path().join("home"));
        let app = PlakousApplication::for_test_accounts(directory.path().join("state")).unwrap();
        let instance = UuidText::new(uuid::Uuid::new_v4().to_string()).unwrap();
        OpsBridge::new(app, home, instance)
    }

    /// The store's refusal reaches the admin caller as `local_admin_required`
    /// for the default and for a named slot.
    #[test]
    fn a_refused_replacement_is_recognised_for_default_and_named_slots() {
        let directory = tempfile::tempdir().unwrap();
        let bridge = bridge(&directory);
        for account in ["claude", "claude/work", "codex", "grok/work"] {
            let failure = bridge.app.account_failure(
                NativeSubscriptionError::AccountReplacementRefused,
                &account.parse().unwrap(),
            );
            assert!(
                replacement_refused(failure.message()),
                "{account}: {}",
                failure.message()
            );
        }
        let other = bridge.app.account_failure(
            NativeSubscriptionError::Authentication,
            &"claude".parse().unwrap(),
        );
        assert!(!replacement_refused(other.message()));
    }

    #[test]
    fn cancelling_a_settled_device_login_returns_its_result() {
        let directory = tempfile::tempdir().unwrap();
        let mut bridge = bridge(&directory);
        let (send, result) = oneshot::channel();
        let task = bridge
            .app
            .executor
            .block_on(async { tokio::spawn(std::future::pending::<()>()).abort_handle() })
            .unwrap();
        let attempt = bridge
            .attempts
            .begin(
                GatewayAccount::new("codex".to_owned()).unwrap(),
                LiveLogin {
                    method: LoginMethod::DeviceCode,
                    pending: None,
                    device: Some(DeviceLogin { result, task }),
                },
            )
            .unwrap();
        send.send(Ok(NativeAuthStatus::Ready)).unwrap();
        assert_eq!(
            bridge.login_cancel(&attempt),
            progress(attempt, LoginPhase::Succeeded)
        );
    }

    /// An empty slot has no account to keep: a remote login into it is not
    /// held to one.
    #[test]
    fn an_empty_slot_is_not_signed_in() {
        let directory = tempfile::tempdir().unwrap();
        let mut bridge = bridge(&directory);
        assert!(!bridge.signed_in(&"claude".parse().unwrap()));
    }
}
