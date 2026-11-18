//! Account verbs against today's `PlakousApplication`. Caller keys, flags and
//! drain never come through here. Login completion for a device code is polled:
//! the provider wait runs on the application executor, and each poll only
//! observes it.

use std::sync::mpsc::{Receiver, sync_channel};

use plakous::gateway::protocol::admin::{
    AdminRefusal, AdminRequest, AdminResponse, LoginMethod, LoginPhase, LogoutMode,
};
use plakous::gateway::protocol::facts::{
    AccountFacts, CredentialFacts, CredentialStatus, KeyFacts, KeySource, RefreshRequest,
    WireIdentity,
};
use plakous::gateway::protocol::refusal::GatewayRefusal;
use plakous::gateway::protocol::{BoundedText, UuidText};
use plakous::gateway::protocol::{GatewayAccount, SecretText};
use plakous::subscriptions::{NativeLoginCompletion, NativeLoginMode};
use plakous_hosted::account::AccountRef;

use super::bridge::{AccountBridge, AttemptBook};
use crate::commands::application::{
    AccountOps as _, LoginOutcome, LoginResponse, PlakousApplication, Revocation,
};
use crate::home::paths::PlakPaths;
use crate::presentation::CredentialState;

struct LiveLogin {
    method: LoginMethod,
    pending: Option<(AccountRef, plakous::subscriptions::PendingNativeLogin)>,
    phase: LoginPhase,
    device: Option<
        Receiver<
            Result<
                plakous::subscriptions::NativeAuthStatus,
                plakous::subscriptions::NativeSubscriptionError,
            >,
        >,
    >,
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
        let status = self
            .app
            .offline_status(&parsed)
            .map_err(|_| GatewayRefusal::Internal {})?;
        Ok(AccountFacts {
            account: account.clone(),
            instance: self.instance.clone(),
            observed_at_unix_ms: super::callers::now_ms(),
            credential: CredentialFacts {
                status: credential_status(status.credential),
                generation: None,
                access_expires_at_unix_ms: None,
                selection_epoch_digest: None,
            },
            identity: None,
            wire_identity: WireIdentity::default(),
            profile: None,
            key: key_facts(account.as_str(), &self.home),
        })
    }
}

impl AccountBridge for OpsBridge {
    fn call(&mut self, local_admin: bool, request: AdminRequest) -> AdminResponse {
        match request {
            AdminRequest::List {} => {
                let accounts = self.app.accounts().unwrap_or_default();
                AdminResponse::Accounts {
                    accounts: accounts
                        .iter()
                        .filter_map(|account| GatewayAccount::new(account.to_string()).ok())
                        .collect(),
                }
            }
            AdminRequest::Status { accounts } => {
                let selected = if accounts.is_empty() {
                    self.app
                        .accounts()
                        .unwrap_or_default()
                        .iter()
                        .filter_map(|account| GatewayAccount::new(account.to_string()).ok())
                        .collect()
                } else {
                    accounts
                };
                let mut rows = Vec::new();
                for account in selected {
                    if let Ok(facts) = self.facts_for(&account) {
                        rows.push(plakous::gateway::protocol::admin::AccountStatusRow {
                            facts,
                            last_refresh: None,
                        });
                    }
                }
                AdminResponse::Status { rows }
            }
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
            AdminRequest::Allowance { account } => AdminResponse::Allowance {
                account,
                observed_at_unix_ms: None,
                view: None,
                unavailable: bounded64("not-observed"),
            },
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
        let result = if mode == LogoutMode::Local {
            self.app.logout_local(&parsed).map(|_| true)
        } else {
            self.app.revoke(&parsed).map(|revocation| match revocation {
                Revocation::Revoked(_) => false,
                Revocation::Unusable => true,
            })
        };
        match result {
            Ok(remain) => changed(account.clone(), Some(remain)),
            Err(_) => refused(AdminRefusal::OutcomeUnknown {}),
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
        if let Err(error) = crate::commands::application::host::prepare_host_login(
            &mut self.app,
            &parsed,
            notice_revision,
            replace_newer,
        ) {
            return refused(login_prep(error.message()));
        }
        let _ = local_admin;
        let Ok(challenge) = self.app.begin_login(&parsed, NativeLoginMode::Remote) else {
            return refused(AdminRefusal::ProviderRejected {});
        };
        let method = match challenge.completion {
            NativeLoginCompletion::DeviceCode => LoginMethod::DeviceCode,
            NativeLoginCompletion::AuthorizationCodeOrRedirectUrl => LoginMethod::Paste,
            NativeLoginCompletion::Automatic => {
                self.app.end_login_attempt(&parsed);
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
            let (tx, rx) = sync_channel(1);
            if self
                .app
                .executor
                .block_on(async move {
                    tokio::spawn(async move {
                        let result = pending.complete().await;
                        let _ = tx.send(result);
                    });
                })
                .is_err()
            {
                return refused(AdminRefusal::Unavailable {});
            }
            (None, Some(rx))
        } else {
            (Some(challenge.pending), None)
        };
        let Ok(id) = self.attempts.begin(
            account,
            LiveLogin {
                method,
                pending: stored_pending,
                phase: LoginPhase::AwaitingInput,
                device,
            },
        ) else {
            return refused(AdminRefusal::AttemptLimit {});
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
        let Some(pending) = found.payload.pending.take() else {
            return refused(AdminRefusal::AttemptUnknown {});
        };
        let id = found.id.clone();
        match self
            .app
            .finish_login(pending, LoginResponse::Pasted(pasted))
        {
            Ok(LoginOutcome::Completed(_)) => {
                let _ = self.attempts.take(attempt);
                progress(id, LoginPhase::Succeeded)
            }
            Ok(LoginOutcome::Cancelled) | Ok(LoginOutcome::TimedOut) => {
                refused(AdminRefusal::PasteInvalid {})
            }
            Err(_) => refused(AdminRefusal::ProviderRejected {}),
        }
    }

    fn login_poll(&mut self, attempt: &UuidText) -> AdminResponse {
        if self.attempts.poll_allowed(attempt).is_err() {
            return refused(AdminRefusal::AttemptLimit {});
        }
        let Ok(found) = self.attempts.get_mut(attempt) else {
            return refused(AdminRefusal::AttemptUnknown {});
        };
        if let Some(rx) = &found.payload.device {
            match rx.try_recv() {
                Ok(Ok(_)) => {
                    let id = found.id.clone();
                    let _ = self.attempts.take(attempt);
                    return progress(id, LoginPhase::Succeeded);
                }
                Ok(Err(_)) => return refused(AdminRefusal::ProviderRejected {}),
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    let id = found.id.clone();
                    return progress(id, LoginPhase::AwaitingInput);
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    return refused(AdminRefusal::OutcomeUnknown {});
                }
            }
        }
        let id = found.id.clone();
        let phase = found.payload.phase;
        progress(id, phase)
    }

    fn login_cancel(&mut self, attempt: &UuidText) -> AdminResponse {
        let Ok(found) = self.attempts.take(attempt) else {
            return refused(AdminRefusal::AttemptUnknown {});
        };
        if let Some((account, _)) = found.payload.pending {
            self.app.end_login_attempt(&account);
        }
        progress(found.id, LoginPhase::Cancelled)
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

fn key_facts(account: &str, home: &PlakPaths) -> Option<KeyFacts> {
    let status = crate::home::secrets::key_status(home, account).ok()?;
    let source = match status.source {
        crate::home::secrets::KeySource::Env => KeySource::Environment,
        crate::home::secrets::KeySource::File => KeySource::File,
        crate::home::secrets::KeySource::Missing => KeySource::Missing,
    };
    Some(KeyFacts {
        source,
        fingerprint: status
            .fingerprint
            .and_then(|text| BoundedText::new(text).ok()),
        env_var: status
            .env_var
            .and_then(|text| BoundedText::new(text.to_owned()).ok()),
    })
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

fn bounded64(text: &str) -> Option<BoundedText<64>> {
    BoundedText::new(text.to_owned()).ok()
}
