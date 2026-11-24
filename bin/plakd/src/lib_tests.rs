#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]

use std::io::Cursor;

use super::*;
use crate::command::{RunEffort, run_effort_levels, run_effort_usage};
use crate::commands::host_config::{AttachEligibility, attach_eligibility, build_host_config};
use crate::commands::session::seed_new_session_binding;
use crate::commands::support::join_host_worker_bounded;
use plakous::agent::ReasoningEffort;
use plakous::subscriptions::NativeLoginMode;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::commands::application::CliNotice;
use crate::commands::login::read_remote_authorization_interactive;
use crate::commands::support::{
    CODEX_CATALOG_SNAPSHOT_MISSING, QUIT_NOTICE, QUIT_NOTICE_DELAY, cli_provider_name,
    join_host_worker_bounded_with_notice, native_subscription_failure,
    persist_miss_native_tool_mode, run_composition,
};
use crate::presentation::{CliModelSummary, CliStatus};
use plakous::model::SubscriptionUsageRead;
use plakous::subscriptions::{NativeModelVisibility, NativeSubscriptionError, NativeToolMode};

fn args(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

/// One ladder: `--effort` and template effort both read
/// `plakous_hosted::effort`. A level added there must reach the CLI
/// without a second list being edited.
#[test]
fn the_cli_effort_ladder_is_the_shared_table() {
    for level in plakous_hosted::effort::LEVELS {
        assert!(
            RunEffort::parse(level).is_some(),
            "{level} is on the shared ladder but not in --effort"
        );
    }
    assert_eq!(
        run_effort_levels(),
        plakous_hosted::effort::levels_pipe_list()
    );
    assert_eq!(
        run_effort_usage(),
        format!(
            "effort must be {}",
            plakous_hosted::effort::levels_pipe_list()
        )
    );
    assert_eq!(RunEffort::parse("ULTRA"), Some(RunEffort::Ultra));
    assert_eq!(RunEffort::parse("turbo"), None);
}

#[test]
fn persist_miss_fail_closes_codex_and_leaves_grok_claude_unset() {
    assert_eq!(
        persist_miss_native_tool_mode("codex").unwrap_err(),
        CODEX_CATALOG_SNAPSHOT_MISSING
    );
    assert_eq!(persist_miss_native_tool_mode("grok").unwrap(), None);
    assert_eq!(persist_miss_native_tool_mode("claude").unwrap(), None);
    assert_eq!(persist_miss_native_tool_mode("openrouter").unwrap(), None);
}

#[test]
fn exit_codes_match_the_documented_classification() {
    assert_eq!(CliFailure::new("mentions login").exit_code(), 1);
    assert_eq!(
        CliFailure::classified(CliFailureKind::Usage, "anything").exit_code(),
        2
    );
    assert_eq!(
        CliFailure::classified(CliFailureKind::Auth, "anything").exit_code(),
        3
    );
    assert_eq!(
        CliFailure::classified(CliFailureKind::Agent, "anything").exit_code(),
        5
    );
    assert_eq!(
        CliFailure::classified(CliFailureKind::RecoveryRequired, "anything").exit_code(),
        5
    );
}

#[test]
fn native_contract_drift_is_auth_exit_three_without_relogin_advice() {
    let error = NativeSubscriptionError::CompatibilityDrift {
        provider: NativeProvider::Codex,
        reason: plakous::subscriptions::CompatibilityDrift::GrantedScope,
    };
    let failure = native_subscription_failure(
        error,
        Some(NativeProvider::Codex),
        Path::new("/nonexistent-plakd-state"),
    );
    assert_eq!(failure.exit_code(), 3);
    assert!(failure.to_string().contains("install a newer build"));
    assert!(!failure.to_string().contains("plakd login"));
}

#[test]
fn native_subscription_failures_name_their_repair_and_class() {
    let state = Path::new("/nonexistent-plakd-state");
    let codex = Some(NativeProvider::Codex);
    for (error, account, expected, exit) in [
        (
            NativeSubscriptionError::CredentialUnavailable,
            codex,
            "no ready codex credential (signed out, quarantined or revoked); run `plakd login codex`",
            3,
        ),
        (
            NativeSubscriptionError::AcceptanceRequired,
            Some(NativeProvider::Xai),
            "the grok compatibility notice is not accepted; run `plakd models grok` to review and accept it (the login is kept)",
            3,
        ),
        (
            NativeSubscriptionError::CatalogRefreshRejected,
            Some(NativeProvider::Claude),
            "native catalog credential refresh was rejected; run `plakd login claude` to authorize again",
            3,
        ),
        (
            NativeSubscriptionError::AuthenticationProviderRejected,
            codex,
            "native subscription provider rejected authentication; run `plakd login codex` to authorize again",
            3,
        ),
        (
            NativeSubscriptionError::LoginTokenRejected,
            None,
            "provider rejected the authorization-code token exchange; start a fresh login with `plakd login <account>`",
            3,
        ),
        (
            NativeSubscriptionError::LoginTokenTransport,
            codex,
            "native subscription token request did not complete; check the network, then start a fresh login with `plakd login codex`",
            3,
        ),
        (
            NativeSubscriptionError::CredentialProfileChanged,
            codex,
            "the stored codex login was written by an older build or another profile; run `plakd login codex`, which replaces it",
            3,
        ),
        (
            NativeSubscriptionError::ModelUnavailable,
            codex,
            "requested native subscription model is unavailable; run `plakd models codex` and choose a listed model, effort and composition",
            1,
        ),
        (
            NativeSubscriptionError::ProviderConfigurationPolicyMismatch(
                "synthetic-private-policy-detail",
            ),
            codex,
            "explicit native provider behavior did not match provider policy; run `plakd models codex` and choose a listed model, effort and composition",
            1,
        ),
        (
            NativeSubscriptionError::CatalogDocumentRejected,
            codex,
            "native catalog document was rejected; the provider's catalog does not match this build's pinned contract; retry later, and if it persists use a newer plakd",
            1,
        ),
        (
            NativeSubscriptionError::Transport(None),
            codex,
            "codex sign-in refresh failed: network exchange failed; retry when online",
            1,
        ),
        (
            NativeSubscriptionError::Transport(Some(plakous::model::ModelNetworkFailure {
                phase: plakous::model::ModelRetryPhase::Resolution,
                sent: plakous::model::ModelNetworkTransmission::NotSent,
                timed_out: true,
            })),
            codex,
            "codex sign-in refresh failed: DNS lookup timed out before anything was sent; credential unchanged; retry when online",
            1,
        ),
    ] {
        let failure = native_subscription_failure(error, account, state);
        assert_eq!(failure.message(), expected);
        assert_eq!(failure.exit_code(), exit, "{expected}");
    }
    let inactive =
        native_subscription_failure(NativeSubscriptionError::ProfileInactive, codex, state);
    assert_eq!(inactive.exit_code(), 3);
    assert!(inactive.message().contains("build a newer plakd"));
    assert!(inactive.message().contains("UTC"));
}

#[cfg(unix)]
#[test]
fn unsafe_storage_names_the_offending_path_and_rule() {
    use std::os::unix::fs::PermissionsExt as _;
    let home = tempfile::tempdir().unwrap();
    let state = std::fs::canonicalize(home.path()).unwrap().join("state");
    std::fs::create_dir_all(state.join("plak/subscriptions")).unwrap();
    for directory in [state.clone(), state.join("plak")] {
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let subscriptions = state.join("plak/subscriptions");
    std::fs::set_permissions(&subscriptions, std::fs::Permissions::from_mode(0o750)).unwrap();
    let failure = native_subscription_failure(
        NativeSubscriptionError::UnsafeStorage,
        Some(NativeProvider::Codex),
        &state,
    );
    assert_eq!(failure.exit_code(), 3);
    assert_eq!(
        failure.message(),
        format!(
            "native subscription storage is unsafe: {0} has mode 0750 but private directories must be 0700; run `chmod 700 '{0}'`",
            subscriptions.display()
        )
    );

    // A refusal no inspected path explains names the root without
    // claiming the store changed under the operation.
    std::fs::set_permissions(&subscriptions, std::fs::Permissions::from_mode(0o700)).unwrap();
    let failure = native_subscription_failure(
        NativeSubscriptionError::UnsafeStorage,
        Some(NativeProvider::Codex),
        &state,
    );
    assert_eq!(failure.exit_code(), 3);
    assert_eq!(
        failure.message(),
        format!(
            "the subscription store refused {}; check that it and its parents are directories you own and can write",
            state.display()
        )
    );
}

#[test]
fn compatibility_prompt_states_its_dates_and_a_closed_stdin_refuses_with_the_remedy() {
    let mut backend = FakeBackend::new(CompatibilityState::Required, CredentialState::Ready);
    backend.credential = CredentialState::Ready;
    let (result, stdout, stderr) =
        invoke(Command::Models("codex".parse().unwrap()), &mut backend, b"");
    let failure = result.unwrap_err();
    assert_eq!(failure.exit_code(), 3);
    assert_eq!(
        failure.message(),
        "the codex compatibility notice needs an answer on stdin and none came; accept it once with `plakd models codex` from a terminal, or pipe `y` to this command"
    );
    assert_eq!(stdout, [] as [u8; 0]);
    assert_eq!(
        String::from_utf8(stderr).unwrap(),
        "codex uses an experimental revision-pinned compatibility integration.\n\
             profile: fixture-profile\n\
             reviewed: 2026-07-31 UTC\n\
             expires: 2026-10-29T00:00:00Z; after that this build refuses codex until a newer build replaces the profile\n\
             Continue? [y/N] \n"
    );
    assert_eq!(backend.calls, vec![Call::Status(NativeProvider::Codex)]);

    let mut declined = FakeBackend::new(CompatibilityState::Required, CredentialState::Ready);
    declined.credential = CredentialState::Ready;
    let (result, _, _) = invoke(
        Command::Models("codex".parse().unwrap()),
        &mut declined,
        b"n\n",
    );
    let failure = result.unwrap_err();
    assert_eq!(failure.exit_code(), 3);
    assert!(failure.message().contains("run `plakd login codex`"));

    let mut inactive = FakeBackend::new(CompatibilityState::Inactive, CredentialState::Ready);
    inactive.credential = CredentialState::Ready;
    let (result, _, _) = invoke(
        Command::Models("codex".parse().unwrap()),
        &mut inactive,
        b"",
    );
    let failure = result.unwrap_err();
    assert_eq!(failure.exit_code(), 3);
    assert!(
        failure.message().contains("build a newer plakd"),
        "{}",
        failure.message()
    );
}

/// What `plakd login` leaves behind a credential this build cannot parse:
/// the acceptance receipt, and a cleared recovery marker with its epoch,
/// both pinned to that consent and to credential generation 1.
#[cfg(unix)]
#[cfg(unix)]
#[derive(Clone, Debug, Eq, PartialEq)]
enum Call {
    Status(NativeProvider),
    Usage(NativeProvider),
    Expiry(NativeProvider),
    Acknowledge(NativeProvider, String),
    Login(NativeProvider, NativeLoginMode),
    Refresh(NativeProvider),
    RefreshNow(NativeProvider),
    Revoke(NativeProvider),
    LogoutLocal(NativeProvider),
    ReplaceNewerBuild(NativeProvider),
    Models(NativeProvider),
    OpenRouterModels,
}

struct FakePendingLogin(NativeLoginMode);

struct FakeBackend {
    selected_accounts: Vec<plakous_hosted::account::AccountRef>,
    same_upstream: Option<plakous_hosted::account::AccountRef>,
    login_completion: Option<plakous::subscriptions::NativeLoginCompletion>,
    login_outcome: Option<commands::application::LoginOutcome>,
    login_failure: Option<&'static str>,
    login_begin_failure: Option<&'static str>,
    login_input: Option<String>,
    login_timeout: Option<Duration>,
    completed_login_modes: Vec<NativeLoginMode>,
    compatibility: CompatibilityState,
    credential: CredentialState,
    terminal: CredentialState,
    calls: Vec<Call>,
    model_summaries: Vec<CliModelSummary>,
    models_failure: Option<CliFailure>,
    openrouter_key_usage: Option<product_usage::VerticalObservation>,
    revoke_refused: bool,
    revoke_failed: bool,
    refresh_failure: Option<&'static str>,
    access_token_expired: bool,
    host_identity_unavailable: bool,
}

impl FakeBackend {
    fn new(compatibility: CompatibilityState, terminal: CredentialState) -> Self {
        Self {
            selected_accounts: Vec::new(),
            same_upstream: None,
            login_completion: None,
            login_outcome: None,
            login_failure: None,
            login_begin_failure: None,
            login_input: None,
            login_timeout: None,
            completed_login_modes: Vec::new(),
            compatibility,
            credential: CredentialState::SignedOut,
            terminal,
            calls: Vec::new(),
            model_summaries: Vec::new(),
            models_failure: None,
            openrouter_key_usage: None,
            revoke_refused: false,
            revoke_failed: false,
            refresh_failure: None,
            access_token_expired: false,
            host_identity_unavailable: false,
        }
    }
}

#[cfg(feature = "web")]
#[test]
fn host_login_preparation_requires_exact_notice_and_explicit_newer_build_consent() {
    let account = "codex/work".parse().unwrap();
    let mut backend = FakeBackend::new(CompatibilityState::Current, CredentialState::Ready);
    assert!(
        commands::application::host::prepare_host_login(
            &mut backend,
            &account,
            "wrong-review",
            false
        )
        .is_err()
    );
    assert!(backend.calls.is_empty() && backend.selected_accounts.is_empty());
    backend.compatibility = CompatibilityState::Inactive;
    assert!(
        commands::application::host::prepare_host_login(
            &mut backend,
            &account,
            "exact-review",
            false
        )
        .is_err()
    );
    assert!(
        !backend
            .calls
            .iter()
            .any(|call| matches!(call, Call::Login(..) | Call::Acknowledge(..)))
    );
    backend.compatibility = CompatibilityState::Current;
    backend.credential = CredentialState::WrittenByNewerBuild;
    backend.calls.clear();
    assert!(
        commands::application::host::prepare_host_login(
            &mut backend,
            &account,
            "exact-review",
            false
        )
        .is_err()
    );
    assert!(
        !backend
            .calls
            .iter()
            .any(|call| matches!(call, Call::ReplaceNewerBuild(..) | Call::Acknowledge(..)))
    );
    backend.calls.clear();
    commands::application::host::prepare_host_login(&mut backend, &account, "exact-review", true)
        .unwrap();
    assert_eq!(
        backend.calls,
        [
            Call::Status(NativeProvider::Codex),
            Call::ReplaceNewerBuild(NativeProvider::Codex),
            Call::Acknowledge(NativeProvider::Codex, "exact-review".into())
        ]
    );
    assert!(
        backend
            .selected_accounts
            .iter()
            .all(|selected| selected == &account)
    );
}

#[cfg(feature = "web")]
#[test]
fn host_logout_never_falls_back_to_local_retirement_after_failed_remote_revoke() {
    let account = "claude/work".parse().unwrap();
    let mut backend = FakeBackend::new(CompatibilityState::Current, CredentialState::Revoked);
    backend.credential = CredentialState::Ready;
    backend.revoke_failed = true;
    assert!(matches!(
        commands::application::host::logout_host_account(&mut backend, &account, false),
        Err(commands::application::host::HostAccountError::OutcomeUnknown(_))
    ));
    assert!(
        backend
            .calls
            .contains(&Call::Revoke(NativeProvider::Claude))
    );
    assert!(
        !backend
            .calls
            .contains(&Call::LogoutLocal(NativeProvider::Claude))
    );
    backend.calls.clear();
    assert_eq!(
        commands::application::host::logout_host_account(&mut backend, &account, true).unwrap(),
        Some(true)
    );
    assert!(
        !backend
            .calls
            .contains(&Call::Revoke(NativeProvider::Claude))
    );
    assert!(
        backend
            .calls
            .contains(&Call::LogoutLocal(NativeProvider::Claude))
    );
}

#[cfg(feature = "web")]
#[test]
fn host_logout_notice_refusal_happens_before_any_credential_effect() {
    let account = "claude/work".parse().unwrap();
    let mut backend = FakeBackend::new(CompatibilityState::Required, CredentialState::Revoked);
    backend.credential = CredentialState::Ready;
    assert!(matches!(
        commands::application::host::logout_host_account(&mut backend, &account, false),
        Err(commands::application::host::HostAccountError::Refused(_))
    ));
    assert_eq!(backend.calls, vec![Call::Status(NativeProvider::Claude)]);
}

impl AccountOps for FakeBackend {
    fn prepare_login(&mut self, account: &plakous_hosted::account::AccountRef) -> CliResult<()> {
        self.selected_accounts.push(account.clone());
        Ok(())
    }
    fn same_upstream_account(
        &mut self,
        account: &plakous_hosted::account::AccountRef,
    ) -> Option<plakous_hosted::account::AccountRef> {
        self.selected_accounts.push(account.clone());
        account
            .is_named()
            .then(|| self.same_upstream.clone())
            .flatten()
    }
    fn remove_account(&mut self, account: &plakous_hosted::account::AccountRef) -> CliResult<()> {
        self.selected_accounts.push(account.clone());
        Ok(())
    }
    fn notice(&self, _provider: &plakous_hosted::account::AccountRef) -> CliResult<CliNotice> {
        Ok(CliNotice {
            profile_id: "fixture-profile",
            notice_revision: "exact-review",
            reviewed_at: 1_785_456_000,
            expires_at: 1_793_232_000,
        })
    }

    fn offline_status(
        &mut self,
        account: &plakous_hosted::account::AccountRef,
    ) -> CliResult<CliStatus> {
        let provider = commands::application::native_provider(account)?;
        self.calls.push(Call::Status(provider));
        Ok(CliStatus {
            compatibility: self.compatibility,
            credential: self.credential,
        })
    }

    fn stored_credential(
        &mut self,
        account: &plakous_hosted::account::AccountRef,
    ) -> CliResult<commands::application::StoredCredential> {
        let provider = commands::application::native_provider(account)?;
        self.calls.push(Call::Status(provider));
        Ok(
            commands::application::CredentialVerdict::from_status(CliStatus {
                compatibility: self.compatibility,
                credential: self.credential,
            })
            .stored(),
        )
    }

    fn subscription_usage(
        &mut self,
        account: &plakous_hosted::account::AccountRef,
    ) -> CliResult<SubscriptionUsageRead> {
        let provider = commands::application::native_provider(account)?;
        self.calls.push(Call::Usage(provider));
        Ok(SubscriptionUsageRead::Unavailable {
            observed_at: plakous::model::Timestamp::new(0, 0)
                .unwrap_or_else(|_| unreachable!("epoch is valid")),
            reason: plakous::model::SubscriptionUsageUnavailable::Unqualified,
        })
    }

    fn access_token_expired(
        &mut self,
        account: &plakous_hosted::account::AccountRef,
    ) -> CliResult<bool> {
        let provider = commands::application::native_provider(account)?;
        self.calls.push(Call::Expiry(provider));
        Ok(self.access_token_expired)
    }

    fn openrouter_key_usage(
        &mut self,
        _account: &plakous_hosted::account::AccountRef,
    ) -> CliResult<product_usage::VerticalObservation> {
        self.openrouter_key_usage
            .clone()
            .map_or(Ok(product_usage::VerticalObservation::NotConnected), Ok)
    }

    fn acknowledge_exact(
        &mut self,
        account: &plakous_hosted::account::AccountRef,
        operator_response: &str,
    ) -> CliResult<()> {
        let provider = commands::application::native_provider(account)?;
        self.calls
            .push(Call::Acknowledge(provider, operator_response.to_owned()));
        if operator_response == "exact-review" {
            Ok(())
        } else {
            Err(CliFailure::new("exact acknowledgement rejected"))
        }
    }

    type PendingLogin = FakePendingLogin;

    fn begin_login(
        &mut self,
        account: &plakous_hosted::account::AccountRef,
        mode: NativeLoginMode,
    ) -> CliResult<commands::application::LoginChallenge<Self::PendingLogin>> {
        self.selected_accounts.push(account.clone());
        let provider = commands::application::native_provider(account)?;
        self.calls.push(Call::Login(provider, mode));
        if let Some(error) = self.login_begin_failure {
            return Err(CliFailure::new(error));
        }
        let completion = self.login_completion.unwrap_or(match (provider, mode) {
            (_, NativeLoginMode::Browser) => {
                plakous::subscriptions::NativeLoginCompletion::Automatic
            }
            (NativeProvider::Codex, NativeLoginMode::Remote) => {
                plakous::subscriptions::NativeLoginCompletion::DeviceCode
            }
            (_, NativeLoginMode::Remote) => {
                plakous::subscriptions::NativeLoginCompletion::AuthorizationCodeOrRedirectUrl
            }
        });
        let device_code = (completion == plakous::subscriptions::NativeLoginCompletion::DeviceCode)
            .then(|| commands::application::DeviceCodePrompt {
                user_code: zeroize::Zeroizing::new("FAKE-CODE".to_owned()),
                expires_in: Duration::from_secs(900),
            });
        Ok(commands::application::LoginChallenge {
            pending: FakePendingLogin(mode),
            authorize_url: if device_code.is_some() {
                "https://auth.invalid/codex/device"
            } else {
                "https://auth.invalid/authorize?fixture=1"
            }
            .to_owned(),
            completion,
            device_code,
            replaced_unusable: matches!(
                self.credential,
                CredentialState::LoginRequired | CredentialState::ProfileChanged
            ),
        })
    }

    fn finish_login(
        &mut self,
        pending: Self::PendingLogin,
        response: commands::application::LoginResponse<'_>,
    ) -> CliResult<commands::application::LoginOutcome> {
        self.completed_login_modes.push(pending.0);
        match response {
            commands::application::LoginResponse::Pasted(input) => {
                self.login_input = Some(input.to_owned());
            }
            commands::application::LoginResponse::Automatic { timeout } => {
                self.login_timeout = Some(timeout);
                self.login_input = None;
            }
        }
        if let Some(error) = self.login_failure {
            return Err(CliFailure::new(error));
        }
        Ok(self
            .login_outcome
            .unwrap_or(commands::application::LoginOutcome::Completed(
                self.terminal,
            )))
    }

    fn record_login_host(&mut self, _provider: &plakous_hosted::account::AccountRef) -> bool {
        !self.host_identity_unavailable
    }

    fn refresh(
        &mut self,
        account: &plakous_hosted::account::AccountRef,
    ) -> CliResult<CredentialState> {
        let provider = commands::application::native_provider(account)?;
        self.calls.push(Call::Refresh(provider));
        Ok(self.terminal)
    }

    fn refresh_now(
        &mut self,
        account: &plakous_hosted::account::AccountRef,
    ) -> CliResult<(CredentialState, u64)> {
        let provider = commands::application::native_provider(account)?;
        self.calls.push(Call::RefreshNow(provider));
        if let Some(error) = self.refresh_failure {
            return Err(CliFailure::new(error));
        }
        Ok((self.terminal, 1_720_000_000))
    }

    fn revoke(
        &mut self,
        account: &plakous_hosted::account::AccountRef,
    ) -> CliResult<commands::application::Revocation> {
        let provider = commands::application::native_provider(account)?;
        self.calls.push(Call::Revoke(provider));
        if self.revoke_failed {
            return Err(CliFailure::new("synthetic revocation transport failure"));
        }
        Ok(if self.revoke_refused {
            commands::application::Revocation::Unusable
        } else {
            commands::application::Revocation::Revoked(self.terminal)
        })
    }

    fn logout_local(
        &mut self,
        account: &plakous_hosted::account::AccountRef,
    ) -> CliResult<CredentialState> {
        let provider = commands::application::native_provider(account)?;
        self.calls.push(Call::LogoutLocal(provider));
        Ok(self.terminal)
    }

    fn replace_newer_build(
        &mut self,
        account: &plakous_hosted::account::AccountRef,
    ) -> CliResult<CredentialState> {
        let provider = commands::application::native_provider(account)?;
        self.calls.push(Call::ReplaceNewerBuild(provider));
        self.credential = CredentialState::Revoked;
        Ok(self.credential)
    }

    fn openrouter_models(
        &mut self,
        _account: &plakous_hosted::account::AccountRef,
    ) -> CliResult<Vec<String>> {
        self.calls.push(Call::OpenRouterModels);
        if let Some(failure) = self.models_failure.clone() {
            return Err(failure);
        }
        Ok(vec!["author/model".to_owned()])
    }

    fn platform_catalog(
        &mut self,
        account: &plakous_hosted::account::AccountRef,
    ) -> CliResult<plakous_hosted::platform::ApiModelCatalog> {
        Ok(plakous_hosted::platform::bundled_catalog(
            account.vertical().capability().platform_provider().unwrap(),
            "offline test backend",
        ))
    }

    fn models(
        &mut self,
        account: &plakous_hosted::account::AccountRef,
    ) -> CliResult<commands::application::LiveModelList> {
        let provider = commands::application::native_provider(account)?;
        self.calls.push(Call::Models(provider));
        if let Some(failure) = self.models_failure.clone() {
            return Err(failure);
        }
        Ok(commands::application::LiveModelList {
            models: self.model_summaries.clone(),
            publication_skipped: None,
        })
    }
}

#[test]
fn new_codex_session_seeds_priority_as_durable_target_authority() {
    let mut config = root_config("codex", "gpt-5.6-sol", Some("medium"));
    config.root_template_mut().extra = Some("priority".into());
    let mut session = product_sessions::SessionRecord::new(PathBuf::from("/tmp/workspace"));
    seed_new_session_binding(&mut session, &config).expect("seed new session");
    assert_eq!(
        session.target_selection,
        product_config::ProductTargetSelection::NativePriority
    );
}

#[test]
fn new_openrouter_session_does_not_inherit_default_effort() {
    let mut config = root_config("openrouter", "vendor/model", Some("medium"));
    config.root_template_mut().composition =
        Some(product_config::ProductCompositionSelection::Direct);
    let mut session = product_sessions::SessionRecord::new(PathBuf::from("/tmp/workspace"));
    seed_new_session_binding(&mut session, &config).expect("seed new session");
    assert_eq!(session.account.as_deref(), Some("openrouter"));
    assert_eq!(session.effort, None);
    assert_eq!(
        session.target_selection,
        product_config::ProductTargetSelection::OpenRouter {
            wire: product_config::ProductWireBinding::resolve(
                product_config::ProductProtocolSelection::Auto
            ),
            routing: product_config::ProductOpenRouterRoutingSelection::GatewayManaged,
        }
    );
}

#[test]
fn new_openrouter_session_resolves_follow_catalog_to_direct() {
    let config = root_config("openrouter", "vendor/model", None);
    let mut session = product_sessions::SessionRecord::new(PathBuf::from("/tmp/workspace"));
    seed_new_session_binding(&mut session, &config).expect("seed new session");
    assert_eq!(
        session.composition,
        product_config::ProductCompositionSelection::Direct
    );
}

#[test]
fn unbound_new_session_never_inherits_non_native_composition() {
    // No `[root] agent`: an unbound session cannot inherit a composition,
    // because a composition only exists on a complete template.
    let config = product_config::ProductConfig::default_product();
    let mut session = product_sessions::SessionRecord::new(PathBuf::from("/tmp/workspace"));
    seed_new_session_binding(&mut session, &config).expect("seed new session");
    assert_eq!(session.account, None);
    assert_eq!(session.model, None);
    assert_eq!(
        session.composition,
        product_config::ProductCompositionSelection::FollowCatalog
    );
}

#[test]
fn run_composition_is_target_scoped_and_preserves_every_explicit_choice() {
    let mut root = root_config("grok", "grok-4.6", None);
    root.root_template_mut().composition =
        Some(product_config::ProductCompositionSelection::Direct);
    let configured = root.default_binding();
    assert_eq!(
        run_composition(None, &configured, &"codex".parse().unwrap(), "gpt-5.6-sol"),
        product_config::ProductCompositionSelection::FollowCatalog
    );
    assert_eq!(
        run_composition(None, &configured, &"grok".parse().unwrap(), "grok-4.6"),
        product_config::ProductCompositionSelection::Direct
    );
    assert_eq!(
        run_composition(None, &configured, &"grok".parse().unwrap(), "grok-4.5"),
        product_config::ProductCompositionSelection::FollowCatalog
    );
    for composition in [
        product_config::ProductCompositionSelection::Direct,
        product_config::ProductCompositionSelection::FollowCatalog,
        product_config::ProductCompositionSelection::JsPtc,
        product_config::ProductCompositionSelection::PyPtc,
    ] {
        assert_eq!(
            run_composition(
                Some(composition),
                &configured,
                &"codex".parse().unwrap(),
                "gpt-5.6-sol"
            ),
            composition
        );
    }
}

#[test]
fn invalid_new_session_default_is_refused_without_rewriting_config() {
    let root = tempfile::tempdir().expect("temporary product home");
    let paths = product_paths::PlakPaths::explicit(root.path().join("product"));
    let config = root_config("grok", "gpt-5.6-sol", Some("high"));
    config.save(&paths).expect("save invalid defaults");
    let before = std::fs::read(paths.config_file()).unwrap();
    let mut session = product_sessions::SessionRecord::new(PathBuf::from("/tmp/workspace"));
    let error = seed_new_session_binding(&mut session, &config).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("plakd config set root.agent <template>")
    );
    assert_eq!(std::fs::read(paths.config_file()).unwrap(), before);
    assert_eq!(config.default_model(), Some("gpt-5.6-sol"));
    assert_eq!(session.model.as_deref(), Some("gpt-5.6-sol"));
}

#[test]
fn new_sessions_reuse_each_persisted_openrouter_protocol_default() {
    let root = tempfile::tempdir().expect("temporary product home");
    for (index, protocol) in [
        product_config::ProductProtocolSelection::Auto,
        product_config::ProductProtocolSelection::Chat,
        product_config::ProductProtocolSelection::Responses,
        product_config::ProductProtocolSelection::Messages,
    ]
    .into_iter()
    .enumerate()
    {
        let mut config = root_config("openrouter", "vendor/model", None);
        {
            let root = config.root_template_mut();
            root.composition = Some(product_config::ProductCompositionSelection::Direct);
            root.protocol = Some(protocol);
        }
        // Templates persist as files beside config.toml, so
        // the round trip goes through a home, not a rendered string.
        let paths = product_paths::PlakPaths::explicit(root.path().join(format!("home-{index}")));
        paths.ensure_layout().expect("home layout");
        config.save(&paths).expect("persisted defaults save");
        let reopened =
            product_config::ProductConfig::load(&paths).expect("persisted defaults reopen");
        let mut session = product_sessions::SessionRecord::new(root.path().to_path_buf());
        seed_new_session_binding(&mut session, &reopened).expect("seed new session");
        let product_config::ProductTargetSelection::OpenRouter { wire, .. } =
            session.target_selection
        else {
            panic!("OpenRouter default must seed an OpenRouter target");
        };
        assert_eq!(wire.selection, protocol);
        wire.validate().expect("seeded wire is exact");
    }
}

/// Builds a config whose `[root] agent` names one minted template.
fn root_config(account: &str, model: &str, effort: Option<&str>) -> product_config::ProductConfig {
    let mut config = product_config::ProductConfig::default_product();
    config.bind_root_tuple(account, model, effort);
    config
}

fn ready_attach_catalog(
    account: &str,
    model: &str,
    effort_required: bool,
) -> plakous_host_protocol::views::ReadyCatalog {
    vec![plakous_host_protocol::views::ProviderSection {
        account: account.into(),
        label: account.into(),
        ready: true,
        entries: vec![plakous_host_protocol::views::ModelEntry {
            id: model.into(),
            tag: String::new(),
            reasoning: effort_required,
            effort_options: effort_required
                .then(|| "medium".to_owned())
                .into_iter()
                .collect(),
            extras: Vec::new(),
            unavailable_reason: None,
        }],
    }]
    .into()
}

#[test]
fn deferred_native_model_without_effort_requirement_attaches_after_discovery() {
    let catalog = ready_attach_catalog("claude", "claude-sonnet-5", false);
    assert_eq!(
        attach_eligibility(
            &product_config::ProductTargetSelection::Native,
            &catalog,
            Some("claude"),
            Some("claude-sonnet-5"),
            false,
        ),
        AttachEligibility::Ready
    );
}

#[test]
fn deferred_reasoning_model_with_explicit_effort_attaches_after_discovery() {
    let catalog = ready_attach_catalog("codex", "gpt-reasoning", true);
    assert_eq!(
        attach_eligibility(
            &product_config::ProductTargetSelection::Native,
            &catalog,
            Some("codex"),
            Some("gpt-reasoning"),
            true,
        ),
        AttachEligibility::Ready
    );
    assert_eq!(
        attach_eligibility(
            &product_config::ProductTargetSelection::Native,
            &catalog,
            Some("codex"),
            Some("gpt-reasoning"),
            false,
        ),
        AttachEligibility::EffortRequired
    );
}

#[test]
fn deferred_unknown_native_model_is_refused_even_with_explicit_effort() {
    let catalog = ready_attach_catalog("codex", "gpt-known", true);
    let eligibility = attach_eligibility(
        &product_config::ProductTargetSelection::Native,
        &catalog,
        Some("codex"),
        Some("gpt-unknown"),
        true,
    );
    assert_eq!(eligibility, AttachEligibility::Unavailable);
}

#[test]
fn deferred_openrouter_target_attaches_without_catalog_membership_or_effort() {
    let target = product_config::ProductTargetSelection::OpenRouter {
        wire: product_config::ProductWireBinding::resolve(
            product_config::ProductProtocolSelection::Chat,
        ),
        routing: product_config::ProductOpenRouterRoutingSelection::GatewayManaged,
    };
    assert_eq!(
        attach_eligibility(
            &target,
            &plakous_host_protocol::views::ReadyCatalog::default(),
            Some("openrouter"),
            Some("vendor/dynamic-model"),
            false,
        ),
        AttachEligibility::Ready
    );
}

#[test]
fn host_default_binding_rolls_back_before_publication_and_remains_usable() {
    let root = tempfile::tempdir().expect("temporary product home");
    let paths = product_paths::PlakPaths::explicit(root.path().join("product"));
    let mut record = product_sessions::SessionRecord::new(root.path().to_path_buf());
    record.account = Some("codex".to_owned());
    product_sessions::save_session(&paths, &mut record).expect("persist source");
    let id = record.id.clone();
    let mut candidate = record.clone();
    candidate.account = Some("grok".to_owned());

    let failure = commit_host_default_candidate(&paths, &mut record, candidate, || {
        Err(product_config::ConfigSaveError::BeforePublication(
            "injected refusal".to_owned(),
        ))
    })
    .expect_err("config refusal");

    assert!(failure.message().contains("session change rolled back"));
    assert_eq!(record.account.as_deref(), Some("codex"));
    assert_eq!(
        product_sessions::load_session(&paths, &id)
            .unwrap()
            .account
            .as_deref(),
        Some("codex")
    );
    let mut retry = record.clone();
    retry.account = Some("grok".to_owned());
    commit_host_default_candidate(&paths, &mut record, retry, || Ok(()))
        .expect("retry after rollback");
    assert_eq!(record.account.as_deref(), Some("grok"));
}

#[test]
fn host_default_binding_fences_post_publication_durability_uncertainty() {
    let root = tempfile::tempdir().expect("temporary product home");
    let paths = product_paths::PlakPaths::explicit(root.path().join("product"));
    let mut record = product_sessions::SessionRecord::new(root.path().to_path_buf());
    record.account = Some("codex".to_owned());
    product_sessions::save_session(&paths, &mut record).expect("persist source");
    let id = record.id.clone();
    let mut candidate = record.clone();
    candidate.account = Some("grok".to_owned());

    let failure = commit_host_default_candidate(&paths, &mut record, candidate, || {
        Err(
            product_config::ConfigSaveError::PublishedDurabilityUncertain(
                "injected directory sync failure".to_owned(),
            ),
        )
    })
    .expect_err("durability uncertainty");

    assert!(failure.message().contains("recovery required"));
    assert_eq!(record.account.as_deref(), Some("grok"));
    assert_eq!(
        product_sessions::load_session(&paths, &id)
            .unwrap()
            .account
            .as_deref(),
        Some("grok")
    );
}

fn invoke(
    command: Command,
    backend: &mut FakeBackend,
    input: &[u8],
) -> (CliResult<ProcessOutcome>, Vec<u8>, Vec<u8>) {
    let mut stdin = Cursor::new(input);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let services = Services::from_env(shell::home::launch_directory().as_deref());
    let result = execute_command(
        command,
        &services,
        Some(backend),
        &mut stdin,
        &mut stdout,
        &mut stderr,
        &mut commands::login_flow::TestLoginUi::default(),
    );
    (result, stdout, stderr)
}

#[test]
fn child_wake_config_set_is_immediate_even_with_a_non_quiescent_tree() {
    use plakous_hosted::runtime::SurfaceHandler;
    let home = tempfile::tempdir().unwrap();
    let paths = product_paths::PlakPaths::explicit(home.path().join("product"));
    paths.ensure_layout().unwrap();
    let mut tree = plakous_hosted::agent_tree::ProductAgentTree::bootstrap_root_for_test(
        "wake-config-tree",
        "root",
        "owner",
        home.path().to_owned(),
        "selected",
        plakous_hosted::agent_tree_protocol::WriterMode::SingleLease,
    )
    .unwrap();
    tree.spawn_agent(
        "root",
        &plakous_hosted::agent_tree_protocol::SpawnAgentRequest::test_review(),
        None,
    )
    .unwrap();
    store::tree::save_tree(&paths, &mut tree).unwrap();
    assert!(!config_key_changes_orchestration("turn.auto_continue"));
    let services = Services::for_home(paths.clone());
    let mut backend = FakeBackend::new(CompatibilityState::Current, CredentialState::Ready);
    let mut output = Vec::new();
    execute_command(
        Command::ConfigSet {
            key: "turn.auto_continue".into(),
            value: "false".into(),
            scope: product_config::ConfigWriteScope::User,
        },
        &services,
        Some(&mut backend),
        &mut Cursor::new([]),
        &mut output,
        &mut Vec::new(),
        &mut commands::login_flow::TestLoginUi::default(),
    )
    .unwrap();
    assert!(
        !product_config::ProductConfig::load_user(&paths)
            .unwrap()
            .auto_continue
    );
    assert!(
        !String::from_utf8(output)
            .unwrap()
            .contains("next session genesis")
    );
    let surface = shell::settings::ProductSurface::new(paths.clone(), gateway::HostCustody::Local);
    assert_eq!(surface.child_wake_enabled(), Some(false));
    let mut config = product_config::ProductConfig::load_user(&paths).unwrap();
    config.set_key("turn.auto_continue", "true").unwrap();
    config.save(&paths).unwrap();
    assert_eq!(surface.child_wake_enabled(), Some(true));
}

#[test]
fn command_and_attach_workers_share_one_lazy_product_store() {
    let home = tempfile::tempdir().unwrap();
    let paths = product_paths::PlakPaths::explicit(home.path().join("product"))
        .for_workspace(&env::current_dir().unwrap());
    let services = Services::for_home(paths.clone());
    let first_roots = services.state_roots().unwrap();
    let second_roots = services.state_roots().unwrap();
    assert!(!paths.product_database().exists());
    assert_eq!(services.product_store_opens(), 0);

    let list = || {
        let mut backend = FakeBackend::new(CompatibilityState::Current, CredentialState::Ready);
        let mut output = Vec::new();
        execute_command(
            Command::SessionList {
                include_archived: false,
            },
            &services,
            Some(&mut backend),
            &mut Cursor::new([]),
            &mut output,
            &mut Vec::new(),
            &mut commands::login_flow::TestLoginUi::default(),
        )
        .unwrap();
        String::from_utf8(output).unwrap()
    };
    assert!(list().contains("No sessions"));
    let workspace = env::current_dir().unwrap();
    let mut session = product_sessions::SessionRecord::new(workspace.clone());
    session.account = Some("openrouter".into());
    session.model = Some("offline/ownership-fixture".into());
    session.composition = product_config::ProductCompositionSelection::Direct;
    session.target_selection = product_config::ProductTargetSelection::OpenRouter {
        wire: product_config::ProductWireBinding::resolve(
            product_config::ProductProtocolSelection::Auto,
        ),
        routing: product_config::ProductOpenRouterRoutingSelection::GatewayManaged,
    };
    let id = session.id.clone();
    // Exercise the same configuration constructor native/deferred attach uses.
    let config = build_host_config(
        &paths,
        &second_roots,
        session,
        Some("openrouter"),
        Some("offline/ownership-fixture"),
        ReasoningEffort::Medium,
        false,
        &workspace,
        &plakous_host_protocol::views::ReadyCatalog::default(),
        None,
        &[],
        &gateway::HostCustody::Local,
    )
    .unwrap();
    assert!(Arc::ptr_eq(&first_roots.session_records(), &config.store));
    thread::spawn(move || {
        let mut record = config.session;
        config.store.save(&mut record).unwrap();
        record.append_transcript(product_sessions::TranscriptRow::user("worker publication"));
        config.store.save(&mut record).unwrap();
    })
    .join()
    .unwrap();
    assert!(list().contains(&id));
    assert_eq!(
        services
            .session_records()
            .unwrap()
            .load(&id)
            .unwrap()
            .store_revision,
        2
    );
    assert_eq!(
        services.product_store_opens(),
        1,
        "command reads and attach-thread writes must retain the same connection"
    );
}

/// One process holds sessions of two workspaces, each with its own trusted
/// project overlay, root template, MCP server and skill: each session's
/// host binds only its own, and both are under the process's one job bound
/// and one custody host id. Nothing workspace-derived is cached for the
/// process.
#[test]
fn two_workspaces_in_one_process_keep_their_own_contexts() {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    let home = product_paths::PlakPaths::explicit(root.join("product"));
    home.ensure_layout().unwrap();
    let services = Services::for_home(home.clone());
    let open = |name: &str| {
        let workspace = root.join(name);
        std::fs::create_dir_all(workspace.join(".plak/agents")).unwrap();
        std::fs::create_dir_all(workspace.join(format!(".agents/skills/{name}-skill"))).unwrap();
        std::fs::write(
            workspace.join(format!(".agents/skills/{name}-skill/SKILL.md")),
            format!("---\nname: {name}-skill\ndescription: {name}\n---\n"),
        )
        .unwrap();
        std::fs::write(
            workspace.join(".plak/config.toml"),
            format!(
                "[root]\nagent = \"{name}\"\n[mcp_servers.{name}mcp]\ntransport = \"stdio\"\ncommand = \"{name}-bin\"\n"
            ),
        )
        .unwrap();
        std::fs::write(
            workspace.join(format!(".plak/agents/{name}.toml")),
            format!(
                "account = \"openrouter\"\nmodel = \"offline/{name}\"\ndescription = \"{name}\"\ntools = [\"mcp:{name}mcp\"]\n"
            ),
        )
        .unwrap();
        product_config::project_trust::trust(&home, &workspace).unwrap();
        let context = services.paths().unwrap().for_workspace(&workspace);
        let mut session = product_sessions::SessionRecord::new(workspace.clone());
        session.account = Some("openrouter".into());
        session.model = Some(format!("offline/{name}"));
        session.composition = product_config::ProductCompositionSelection::Direct;
        session.target_selection = product_config::ProductTargetSelection::OpenRouter {
            wire: product_config::ProductWireBinding::resolve(
                product_config::ProductProtocolSelection::Auto,
            ),
            routing: product_config::ProductOpenRouterRoutingSelection::GatewayManaged,
        };
        let config = build_host_config(
            &context,
            &services.state_roots().unwrap(),
            session,
            Some("openrouter"),
            Some(&format!("offline/{name}")),
            ReasoningEffort::Medium,
            false,
            &workspace,
            &plakous_host_protocol::views::ReadyCatalog::default(),
            None,
            &[],
            &gateway::HostCustody::Local,
        )
        .unwrap();
        store::tree::open_tree_store(&context).unwrap();
        (workspace, config)
    };
    let (alpha_workspace, alpha) = open("alpha");
    let (beta_workspace, beta) = open("beta");
    for (workspace, config, own, other) in [
        (&alpha_workspace, &alpha, "alpha", "beta"),
        (&beta_workspace, &beta, "beta", "alpha"),
    ] {
        let template = config.root_template.as_ref().expect("root template");
        assert_eq!(template.name, own);
        assert_eq!(template.model, format!("offline/{own}"));
        assert!(
            template
                .tool_sections
                .contains_key(&format!("mcp_servers.{own}mcp"))
        );
        assert!(!template.tool_sections.keys().any(|key| key.contains(other)));
        // Skills and instruction sources are discovered from the session's
        // own workspace when its tools are built.
        assert_eq!(&config.cwd, workspace);
        assert_eq!(&config.session.workspace_root, workspace);
        assert!(config.state_roots.shares_jobs(services.process_jobs()));
    }
    assert_eq!(
        plakous_recursive_subagents::tree::custody_host().host_id,
        commands::runner::host_id(&home).unwrap(),
        "both sessions' trees are held under the home's one host id"
    );
}

#[test]
fn command_store_reopens_after_product_home_replacement() {
    let home = tempfile::tempdir().unwrap();
    let workspace = env::current_dir().unwrap();
    let paths =
        product_paths::PlakPaths::explicit(home.path().join("product")).for_workspace(&workspace);
    let services = Services::for_home(paths.clone());
    let records = services.session_records().unwrap();
    let mut original = product_sessions::SessionRecord::new(workspace.clone());
    records.save(&mut original).unwrap();
    std::fs::rename(paths.home(), home.path().join("retired-product")).unwrap();
    let mut replacement = product_sessions::SessionRecord::new(workspace);
    product_sessions::save_session(&paths, &mut replacement).unwrap();

    let mut backend = FakeBackend::new(CompatibilityState::Current, CredentialState::Ready);
    let mut output = Vec::new();
    execute_command(
        Command::SessionList {
            include_archived: false,
        },
        &services,
        Some(&mut backend),
        &mut Cursor::new([]),
        &mut output,
        &mut Vec::new(),
        &mut commands::login_flow::TestLoginUi::default(),
    )
    .unwrap();
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains(&replacement.id));
    assert!(!output.contains(&original.id));
    assert_eq!(records.load(&original.id).unwrap_err(), "session not found");
    assert_eq!(services.product_store_opens(), 2);
}

#[test]
fn session_usage_reads_only_the_selected_native_subscription() {
    for (account, provider) in [
        ("codex", NativeProvider::Codex),
        ("grok", NativeProvider::Xai),
        ("claude", NativeProvider::Claude),
    ] {
        let mut backend = FakeBackend::new(CompatibilityState::Current, CredentialState::Ready);
        backend.credential = CredentialState::Ready;
        let mut stdout = Vec::new();

        write_selected_subscription_usage(Some(account), &mut backend, None, &mut stdout).unwrap();

        // Only the selected vertical performs a network read; the block
        // itself comes out of the one host-owned Usage projection.
        assert_eq!(
            backend.calls,
            vec![Call::Status(provider), Call::Usage(provider)]
        );
        let text = String::from_utf8(stdout).unwrap();
        assert!(text.starts_with("\nLimits\n"), "{text}");
        assert_eq!(
            text.matches("\nCodex\n").count()
                + text.matches("\nSuperGrok\n").count()
                + text.matches("\nClaude\n").count()
                + text.matches("\nOpenRouter\n").count(),
            1,
            "a selected view contains only its selected vertical: {text}"
        );
    }

    // An unknown account names no vertical, so nothing is read or written.
    let mut backend = FakeBackend::new(CompatibilityState::Current, CredentialState::Ready);
    let mut stdout = Vec::new();
    write_selected_subscription_usage(Some("nope"), &mut backend, None, &mut stdout).unwrap();
    assert_eq!(backend.calls, [] as [Call; 0]);
    assert_eq!(stdout, [] as [u8; 0]);
}

/// The short id the exit line prints must resolve back to the one session
/// it names, and an ambiguous or unknown prefix must say so rather than
/// open the wrong session.
#[test]
fn session_id_prefix_resolves_uniquely_or_refuses() {
    let ids = [
        "1a3c1adc-22f2-4b89-afc6-784807a2f1a5",
        "1a3c9999-0000-4b89-afc6-784807a2f1a5",
        "f71403b3-6c92-4ce2-b01c-51e01e9c2de4",
    ];
    assert_eq!(
        commands::session::match_session_id_prefix("f71403b3", &ids).unwrap(),
        ids[2]
    );
    assert_eq!(
        commands::session::match_session_id_prefix("1a3c1adc", &ids).unwrap(),
        ids[0]
    );
    let ambiguous = commands::session::match_session_id_prefix("1a3c", &ids).unwrap_err();
    assert!(ambiguous.contains("matches 2 sessions"), "{ambiguous}");
    let unknown = commands::session::match_session_id_prefix("beef", &ids).unwrap_err();
    assert!(
        unknown.contains("no session in this workspace"),
        "{unknown}"
    );
    let short = commands::session::match_session_id_prefix("1a3", &ids).unwrap_err();
    assert!(short.contains("too short"), "{short}");
}

/// A quit that finishes under the notice delay prints nothing; one that
/// is still releasing after it says so exactly once.
#[test]
fn slow_host_join_prints_the_quitting_notice_once_and_a_quick_one_never() {
    let mut quick = Vec::new();
    join_host_worker_bounded_with_notice(
        plakous_hosted::runtime::HostWorkerHandle::spawn(|| Ok(())),
        Duration::from_secs(1),
        "quick",
        Some(&mut quick),
    )
    .unwrap();
    assert!(quick.is_empty(), "{}", String::from_utf8_lossy(&quick));

    let mut slow = Vec::new();
    join_host_worker_bounded_with_notice(
        plakous_hosted::runtime::HostWorkerHandle::spawn(|| {
            thread::sleep(QUIT_NOTICE_DELAY * 3);
            Ok(())
        }),
        Duration::from_secs(10),
        "slow",
        Some(&mut slow),
    )
    .unwrap();
    assert_eq!(String::from_utf8_lossy(&slow), format!("{QUIT_NOTICE}\n"));
}

#[test]
fn host_worker_join_preserves_the_terminal_failure() {
    let worker = plakous_hosted::runtime::HostWorkerHandle::spawn(|| {
        Err(CliFailure::new("owned root release refused"))
    });
    let error = join_host_worker_bounded(worker, Duration::from_secs(1), "typed").unwrap_err();
    assert_eq!(error.message(), "owned root release refused");
}

#[test]
fn host_worker_join_returns_within_its_deadline_and_says_it_detached() {
    join_host_worker_bounded(
        plakous_hosted::runtime::HostWorkerHandle::spawn(|| Ok(())),
        Duration::from_secs(1),
        "timely",
    )
    .unwrap();

    let (release, held) = std::sync::mpsc::channel();
    let (entered, running) = std::sync::mpsc::channel();
    let (finished, settled) = std::sync::mpsc::channel();
    let worker = plakous_hosted::runtime::HostWorkerHandle::spawn(move || {
        entered.send(()).unwrap();
        let _ = held.recv();
        finished.send(()).unwrap();
        Ok(())
    });
    running.recv_timeout(Duration::from_secs(30)).unwrap();
    let (completed, detached) = std::sync::mpsc::channel();
    let joiner = thread::spawn(move || {
        completed
            .send(join_host_worker_bounded(
                worker,
                Duration::from_millis(5),
                "stuck",
            ))
            .unwrap();
    });
    // An unbounded join cannot return while the original worker is held.
    let result = detached.recv_timeout(Duration::from_secs(30));
    release.send(()).unwrap();
    settled.recv_timeout(Duration::from_secs(30)).unwrap();
    joiner.join().unwrap();
    let error = result
        .expect("join waited on the unreleased worker")
        .unwrap_err();
    assert!(
        error
            .message()
            .contains("exceeded its 5ms shutdown deadline")
    );
    assert!(error.message().contains("detached"));
}

#[test]
fn ordinary_build_omits_maintainer_paid_commands() {
    let mut output = Vec::new();
    if let Err(error) = cli::render::global_help(&mut output) {
        panic!("usage must render: {error:?}");
    }
    let usage = match String::from_utf8(output) {
        Ok(usage) => usage,
        Err(error) => panic!("usage must be UTF-8: {error}"),
    };
    assert!(!usage.contains("plakd platform"));
    assert!(!usage.contains("smoke platform"));
    assert!(
        parse(args(&[
            "platform",
            "authority",
            "recover",
            "--provider",
            "openai",
            "--recovery-token",
            "authority.json",
        ]))
        .is_err()
    );
}

#[test]
fn parses_both_providers_and_every_credential_verb_and_mode() {
    for provider in [
        NativeProvider::Codex,
        NativeProvider::Xai,
        NativeProvider::Claude,
    ] {
        let name = cli_provider_name(provider);
        assert_eq!(
            parse(args(&["status", name])),
            Ok(Command::AuthStatus(
                cli_provider_name(provider).parse().unwrap()
            ))
        );
        assert_eq!(
            parse(args(&["login", name])),
            Ok(Command::AuthLogin {
                provider: cli_provider_name(provider).parse().unwrap(),
                mode: NativeLoginMode::Browser,
            })
        );
        assert_eq!(
            parse(args(&["login", name, "--remote"])),
            Ok(Command::AuthLogin {
                provider: cli_provider_name(provider).parse().unwrap(),
                mode: NativeLoginMode::Remote,
            })
        );
        assert!(parse(args(&["login", name, "--device"])).is_err());
        assert_eq!(
            parse(args(&["refresh", name])),
            Ok(Command::AuthRefresh(
                cli_provider_name(provider).parse().unwrap()
            ))
        );
        assert_eq!(
            parse(args(&["logout", name])),
            Ok(Command::AuthLogout {
                provider: cli_provider_name(provider).parse().unwrap(),
                local_only: false,
            })
        );
        assert_eq!(
            parse(args(&["logout", name, "--local"])),
            Ok(Command::AuthLogout {
                provider: cli_provider_name(provider).parse().unwrap(),
                local_only: true,
            })
        );
    }
    assert_eq!(
        parse(args(&["login", "grok", "--remote"])),
        Ok(Command::AuthLogin {
            provider: "grok".parse().unwrap(),
            mode: NativeLoginMode::Remote,
        })
    );
}

#[test]
fn parses_exact_run_route_for_both_providers() {
    assert_eq!(
        parse(args(&[
            "run",
            "--model",
            "exact-model",
            "--provider",
            "grok",
            "--",
            "hello",
            "world",
        ])),
        Ok(Command::Run {
            limits: plakous_hosted::runtime::RunBounds::default(),
            provider: "grok".parse().unwrap(),
            model: "exact-model".to_owned(),
            effort: None,
            composition: None,
            print_session_id: false,
            prompt: "hello world".into(),

            accounts: Vec::new(),
        })
    );
    assert_eq!(
        parse(args(&[
            "run",
            "--model",
            "exact-model",
            "--provider",
            "codex",
            "--",
            "hello",
            "world",
        ])),
        Ok(Command::Run {
            limits: plakous_hosted::runtime::RunBounds::default(),
            provider: "codex".parse().unwrap(),
            model: "exact-model".to_owned(),
            effort: None,
            composition: None,
            print_session_id: false,
            prompt: "hello world".into(),

            accounts: Vec::new(),
        })
    );
    assert_eq!(
        parse(args(&[
            "run",
            "--provider",
            "grok",
            "--model",
            "exact-model",
            "--effort",
            "high",
            "--",
            "research me",
        ])),
        Ok(Command::Run {
            limits: plakous_hosted::runtime::RunBounds::default(),
            provider: "grok".parse().unwrap(),
            model: "exact-model".to_owned(),
            effort: Some(ReasoningEffort::High),
            composition: None,
            print_session_id: false,
            prompt: "research me".into(),

            accounts: Vec::new(),
        })
    );
    assert_eq!(
        parse(args(&[
            "run",
            "--provider",
            "codex",
            "--model",
            "gpt-5.3-codex-spark",
            "--effort",
            "xhigh",
            "--",
            "spark me",
        ])),
        Ok(Command::Run {
            limits: plakous_hosted::runtime::RunBounds::default(),
            provider: "codex".parse().unwrap(),
            model: "gpt-5.3-codex-spark".to_owned(),
            effort: Some(ReasoningEffort::XHigh),
            composition: None,
            print_session_id: false,
            prompt: "spark me".into(),

            accounts: Vec::new(),
        })
    );
    assert_eq!(
        parse(args(&[
            "run",
            "--provider",
            "grok",
            "--model",
            "exact-model",
            "--effort",
            "medium",
            "--",
            "patch me",
        ])),
        Ok(Command::Run {
            limits: plakous_hosted::runtime::RunBounds::default(),
            provider: "grok".parse().unwrap(),
            model: "exact-model".to_owned(),
            effort: Some(ReasoningEffort::Medium),
            composition: None,
            print_session_id: false,
            prompt: "patch me".into(),

            accounts: Vec::new(),
        })
    );
}

/// `plakd models` is read-only catalog discovery, present in every build.
#[test]
fn parses_exact_models_route_for_both_providers() {
    assert_eq!(
        parse(args(&["models", "openrouter"])),
        Ok(Command::ModelsOpenRouter("openrouter".parse().unwrap()))
    );
    assert!(parse(args(&["models", "unknown"])).is_err());

    for provider in [
        NativeProvider::Codex,
        NativeProvider::Xai,
        NativeProvider::Claude,
    ] {
        assert_eq!(
            parse(args(&["models", cli_provider_name(provider)])),
            Ok(Command::Models(
                cli_provider_name(provider).parse().unwrap()
            ))
        );
    }
    assert!(matches!(
        parse(args(&["models"])),
        Ok(Command::CommandHelp { command }) if command == "models"
    ));
}

#[cfg(feature = "maintainer")]
#[test]
fn parses_local_compatibility_notice_route_for_both_providers() {
    for provider in [
        NativeProvider::Codex,
        NativeProvider::Xai,
        NativeProvider::Claude,
    ] {
        assert_eq!(
            parse(args(&["compatibility", cli_provider_name(provider)])),
            Ok(Command::CompatibilityNotice(provider))
        );
    }
    assert!(matches!(
        parse(args(&["compatibility"])),
        Ok(Command::CommandHelp { command }) if command == "compatibility"
    ));
    assert!(parse(args(&["compatibility", "unknown"])).is_err());
    assert!(parse(args(&["compatibility", "codex", "extra"])).is_err());
}

#[test]
fn local_compatibility_notice_exposes_exact_compiled_facts_for_each_provider() {
    let cases = [
        (
            NativeProvider::Codex,
            concat!(
                "provider=codex\n",
                "profile_id=openai.codex.compat.2026-07-31.r1\n",
                "source_revision=c4f2746c43a3ab87c2978657d7f2c274546e97fe\n",
                "secondary_source_revision=none\n",
                "notice_revision=openai.codex.compat.notice.2026-07-31.r1\n",
                "provenance=pinned_compatibility\n",
                "reviewed_at={reviewed_at}\n",
                "expires_at={expires_at}\n",
                "kill_switch_revision=openai.codex.compat.kill.2026-07-31.r1\n",
                "killed=false\n",
            ),
        ),
        (
            NativeProvider::Xai,
            concat!(
                "provider=xai\n",
                "profile_id=xai.grok.compat.2026-07-31.r1\n",
                "source_revision=19231fce4b70aa5f7894a0a0eb20ff29bd417db5\n",
                "secondary_source_revision=afbc0fb710320c7add294c2106d447ecc3e3af2e\n",
                "notice_revision=xai.grok.compat.notice.2026-07-31.r1\n",
                "provenance=pinned_compatibility\n",
                "reviewed_at={reviewed_at}\n",
                "expires_at={expires_at}\n",
                "kill_switch_revision=xai.grok.compat.kill.2026-07-31.r1\n",
                "killed=false\n",
            ),
        ),
    ];
    for (provider, expected) in cases {
        let notice = plakous::subscriptions::NativeSubscriptions::notice(provider).unwrap();
        let expected = expected
            .replace("{reviewed_at}", &notice.reviewed_at().to_string())
            .replace("{expires_at}", &notice.expires_at().to_string());
        let mut output = Vec::new();
        assert_eq!(
            print_compatibility_notice(&cli_provider_name(provider).parse().unwrap(), &mut output),
            Ok(())
        );
        assert_eq!(output, expected.as_bytes());
    }
}

#[test]
fn rejects_missing_duplicate_unknown_and_credential_or_origin_style_arguments() {
    let rejected = [
        args(&["auth", "status", "codex"]),
        args(&["auth", "login", "codex"]),
        args(&["login", "unknown"]),
        args(&["login", "codex", "--browser"]),
        args(&["login", "codex", "--remote", "--remote"]),
        args(&["status", "codex", "--api-key", "sensitive"]),
        args(&["models", "unknown"]),
        args(&["models", "codex", "extra"]),
        args(&["models", "codex", "--api-key", "sensitive"]),
        args(&["run", "--provider", "codex", "--model", "m"]),
        args(&["run", "--", "prompt"]),
        args(&["run", "--provider", "codex", "--", "prompt"]),
        args(&["run", "--model", "m", "--", "prompt"]),
        args(&["run", "--provider", "", "--model", "m", "--", "prompt"]),
        args(&["run", "--provider", "codex", "--model", "", "--", "prompt"]),
        args(&[
            "run",
            "--provider",
            "codex",
            "--provider",
            "xai",
            "--model",
            "m",
            "--",
            "prompt",
        ]),
        args(&[
            "run",
            "--model",
            "m1",
            "--model",
            "m2",
            "--provider",
            "xai",
            "--",
            "prompt",
        ]),
        args(&[
            "run",
            "--api-key",
            "sensitive",
            "--provider",
            "codex",
            "--model",
            "m",
            "--",
            "prompt",
        ]),
        args(&[
            "run",
            "--base-url",
            "https://untrusted.invalid",
            "--provider",
            "xai",
            "--model",
            "m",
            "--",
            "prompt",
        ]),
        args(&[
            "run",
            "--origin",
            "https://untrusted.invalid",
            "--provider",
            "xai",
            "--model",
            "m",
            "--",
            "prompt",
        ]),
    ];
    for values in rejected {
        assert!(parse(values).is_err());
    }
}

#[test]
fn rejects_empty_prompt_forms() {
    for values in [
        args(&["run", "--provider", "codex", "--model", "m", "--"]),
        args(&["run", "--provider", "codex", "--model", "m", "--", ""]),
        args(&["run", "--provider", "grok", "--model", "m", "--", " ", "\t"]),
    ] {
        assert_eq!(
            parse(values),
            Err("run requires a non-empty prompt after `--`".to_owned())
        );
    }
}

#[cfg(unix)]
#[test]
fn rejects_invalid_utf8_arguments_without_reflecting_bytes() {
    use std::os::unix::ffi::OsStringExt as _;

    let error = parse(vec![OsString::from_vec(vec![b'r', b'u', b'n', 0xff])]);
    assert_eq!(
        error,
        Err("command arguments must be valid UTF-8".to_owned())
    );
}

#[test]
fn parser_errors_do_not_reflect_secret_or_origin_values() {
    for sensitive in ["top-secret-api-key", "https://private-origin.invalid/path"] {
        let Err(error) = parse(args(&[
            "run",
            "--api-key",
            sensitive,
            "--provider",
            "codex",
            "--model",
            "m",
            "--",
            "prompt",
        ])) else {
            panic!("credential-style argument must be rejected");
        };
        assert!(!error.contains(sensitive));
    }
}

#[test]
fn models_reports_unusable_credential_before_compatibility_prompt() {
    for credential in [
        CredentialState::SignedOut,
        CredentialState::Quarantined,
        CredentialState::Revoked,
        CredentialState::Unsupported,
    ] {
        let mut backend = FakeBackend::new(CompatibilityState::Required, credential);
        backend.credential = credential;
        let (result, stdout, stderr) = invoke(
            Command::Models("codex".parse().unwrap()),
            &mut backend,
            b"yes\n",
        );
        assert!(result.is_err());
        assert_eq!(stdout, [] as [u8; 0]);
        assert!(!String::from_utf8_lossy(&stderr).contains("Continue?"));
        assert_eq!(backend.calls, vec![Call::Status(NativeProvider::Codex)]);
    }
}

#[test]
fn models_run_status_then_exact_acknowledgement_then_discovery() {
    for provider in [
        NativeProvider::Codex,
        NativeProvider::Xai,
        NativeProvider::Claude,
    ] {
        let mut backend = FakeBackend::new(CompatibilityState::Required, CredentialState::Ready);
        backend.credential = CredentialState::Ready;
        backend.model_summaries = vec![
            CliModelSummary {
                unavailable_reason: None,
                exact_id: "exact-model-a".to_owned(),
                visibility: NativeModelVisibility::List,
                native_tool_mode: None,
                streaming: true,
                tools: true,
                reasoning_efforts: Vec::new(),
                service_tiers: Vec::new(),
                reported_context_length: Some(256_000),
            },
            CliModelSummary {
                unavailable_reason: None,
                exact_id: "exact-model-b".to_owned(),
                visibility: NativeModelVisibility::Hide,
                native_tool_mode: Some(NativeToolMode::Direct),
                streaming: true,
                tools: false,
                reasoning_efforts: vec![ReasoningEffort::Low, ReasoningEffort::High],
                service_tiers: Vec::new(),
                reported_context_length: None,
            },
        ];

        let (result, stdout, stderr) = invoke(
            Command::Models(cli_provider_name(provider).parse().unwrap()),
            &mut backend,
            b"yes\r\n",
        );

        assert_eq!(result, Ok(ProcessOutcome::Success));
        assert_eq!(
            backend.calls,
            vec![
                Call::Status(provider),
                Call::Acknowledge(provider, "exact-review".to_owned()),
                Call::Refresh(provider),
                Call::Models(provider),
            ]
        );
        assert_eq!(
            stdout,
            b"exact-model-a\tvisibility=list\ttool_mode=n/a\tstreaming=true\ttools=true\treasoning=false\tefforts=none\tcontext_length=256000\n\
exact-model-b\tvisibility=hide\ttool_mode=direct\tstreaming=true\ttools=false\treasoning=true\tefforts=low,high\tcontext_length=unknown\n"
        );
        let stderr = String::from_utf8(stderr).unwrap_or_default();
        assert!(stderr.contains("Continue? [y/N]"));
        assert!(!stderr.contains("exact-model-a"));
        assert!(!stderr.contains("exact-model-b"));
    }
}

#[test]
fn models_do_not_discover_or_write_listings_when_the_gate_fails() {
    let mut inactive = FakeBackend::new(CompatibilityState::Inactive, CredentialState::Ready);
    inactive.credential = CredentialState::Ready;
    let (result, stdout, _) = invoke(
        Command::Models("codex".parse().unwrap()),
        &mut inactive,
        b"exact-review\n",
    );
    assert!(result.is_err());
    assert_eq!(stdout, [] as [u8; 0]);
    assert_eq!(inactive.calls, vec![Call::Status(NativeProvider::Codex)]);

    let mut rejected = FakeBackend::new(CompatibilityState::Required, CredentialState::Ready);
    rejected.credential = CredentialState::Ready;
    let (result, stdout, stderr) = invoke(
        Command::Models("grok".parse().unwrap()),
        &mut rejected,
        b"not-exact\n",
    );
    assert!(result.is_err());
    assert_eq!(stdout, [] as [u8; 0]);
    assert_eq!(rejected.calls, vec![Call::Status(NativeProvider::Xai)]);
    let stderr = String::from_utf8(stderr).unwrap_or_default();
    assert!(stderr.contains("Continue? [y/N]"));
    assert!(!stderr.contains("not-exact"));
}

#[test]
fn openrouter_models_dispatches_only_catalog_and_failure_emits_no_listing() {
    let mut backend = FakeBackend::new(CompatibilityState::Required, CredentialState::SignedOut);
    let (result, stdout, stderr) = invoke(
        Command::ModelsOpenRouter("openrouter".parse().unwrap()),
        &mut backend,
        b"",
    );
    assert_eq!(result, Ok(ProcessOutcome::Success));
    assert_eq!(stdout, b"author/model\n");
    assert_eq!(stderr, [] as [u8; 0]);
    assert_eq!(backend.calls, vec![Call::OpenRouterModels]);
    backend.calls.clear();
    backend.models_failure = Some(CliFailure::new("catalog failed"));
    let (result, stdout, stderr) = invoke(
        Command::ModelsOpenRouter("openrouter".parse().unwrap()),
        &mut backend,
        b"",
    );
    assert!(result.is_err());
    assert_eq!(stdout, [] as [u8; 0]);
    assert_eq!(stderr, [] as [u8; 0]);
    assert_eq!(backend.calls, vec![Call::OpenRouterModels]);
}

#[test]
fn models_backend_failure_cannot_emit_a_partial_listing() {
    let mut backend = FakeBackend::new(CompatibilityState::Current, CredentialState::Ready);
    backend.credential = CredentialState::Ready;
    backend.model_summaries.push(CliModelSummary {
        unavailable_reason: None,
        exact_id: "must-not-print".to_owned(),
        visibility: NativeModelVisibility::List,
        native_tool_mode: Some(NativeToolMode::Direct),
        streaming: true,
        tools: true,
        reasoning_efforts: vec![ReasoningEffort::Medium],
        service_tiers: Vec::new(),
        reported_context_length: None,
    });
    backend.models_failure = Some(CliFailure::new("authenticated catalog failed"));

    let (result, stdout, stderr) =
        invoke(Command::Models("codex".parse().unwrap()), &mut backend, b"");

    assert_eq!(
        result.err().map(|error| error.message().to_owned()),
        Some("authenticated catalog failed".to_owned())
    );
    assert_eq!(stdout, [] as [u8; 0]);
    assert_eq!(stderr, [] as [u8; 0]);
    assert_eq!(
        backend.calls,
        vec![
            Call::Status(NativeProvider::Codex),
            Call::Refresh(NativeProvider::Codex),
            Call::Models(NativeProvider::Codex),
        ]
    );
}

#[test]
fn concrete_run_declined_compatibility_cannot_open_a_product_session() {
    let home = tempfile::tempdir().unwrap();
    let paths = product_paths::PlakPaths::explicit(home.path().join("product"));
    let services = Services::for_home(paths.clone());
    let mut app = PlakousApplication::for_test_accounts(home.path().join("state")).unwrap();
    let mut output = Vec::new();
    let result = execute_application_command(
        Command::Run {
            limits: plakous_hosted::runtime::RunBounds::default(),
            provider: "codex".parse().unwrap(),
            model: "gpt-5.6-sol".into(),
            effort: None,
            composition: None,
            prompt: "must not dispatch".into(),
            print_session_id: false,

            accounts: Vec::new(),
        },
        &services,
        &mut app,
        &mut Cursor::new(b"no\n"),
        &mut output,
        &mut Vec::new(),
    );
    assert_eq!(result.unwrap_err().exit_code(), 3);
    assert_eq!(output, [] as [u8; 0]);
    assert!(!paths.product_database().exists());
    assert!(!paths.runtime_database().exists());
}

#[test]
fn device_login_unavailable_names_the_browser_and_port_forward_remedies() {
    let message = native_subscription_failure(
        NativeSubscriptionError::DeviceLoginUnavailable,
        Some(NativeProvider::Codex),
        Path::new("/nonexistent-plakd-state"),
    )
    .message()
    .to_owned();
    assert_eq!(
        message,
        "device-code login is not available for this account/server; remedy: run `plakd login codex` on a machine with a browser, or forward port 1455 (ssh -L 1455:localhost:1455 <host>) and run `plakd login codex` there"
    );
}

#[test]
fn remote_authorization_interactive_accepts_paste_after_cooked_recopy() {
    // Both copy commands use the injected clipboard double.
    let mut ui = commands::login_flow::TestLoginUi::default();
    let mut stdin = b"c\ncopy\nhttp://localhost:1455/callback?code=abc\n".as_slice();
    let mut stderr = Vec::new();
    let got = read_remote_authorization_interactive(
        &mut stdin,
        &mut stderr,
        "https://auth.example/start",
        64 * 1024,
        &mut ui,
    )
    .expect("paste accepted");
    assert_eq!(got.as_str(), "http://localhost:1455/callback?code=abc");
    assert_eq!(ui.copied_urls, vec!["https://auth.example/start"; 2]);
    let err = String::from_utf8_lossy(&stderr);
    assert!(err.contains("> "));
    assert_eq!(err.matches("clipboard unavailable").count(), 2);
    ui.clipboard_available = true;
    let secret = read_remote_authorization_interactive(
        &mut b"copy\nsynthetic-code\n".as_slice(),
        &mut stderr,
        "https://auth.example/start",
        64 * 1024,
        &mut ui,
    )
    .expect("available clipboard and subsequent paste");
    assert_eq!(secret.as_str(), "synthetic-code");
    assert_eq!(ui.copied_urls.len(), 3);
    assert!(String::from_utf8_lossy(&stderr).contains("re-copied"));
    assert!(!String::from_utf8_lossy(&stderr).contains("synthetic-code"));
}

#[test]
fn remote_authorization_interactive_empty_line_cancels() {
    let mut ui = commands::login_flow::TestLoginUi::default();
    let mut stdin = b"\n".as_slice();
    let mut stderr = Vec::new();
    let err = read_remote_authorization_interactive(
        &mut stdin,
        &mut stderr,
        "https://auth.example/start",
        64 * 1024,
        &mut ui,
    )
    .expect_err("empty cancels");
    assert!(err.message().contains("required") || err.message().contains("cancel"));
}

#[test]
fn bare_login_parses_as_command_help() {
    assert_eq!(
        parse(args(&["login"])),
        Ok(Command::CommandHelp {
            command: "login".into()
        })
    );
}

fn commit_host_default_candidate(
    paths: &product_paths::PlakPaths,
    record: &mut product_sessions::SessionRecord,
    candidate: product_sessions::SessionRecord,
    save_config: impl FnOnce() -> Result<(), product_config::ConfigSaveError>,
) -> CliResult<()> {
    let mut prior = record.clone();
    let mut candidate = candidate;
    product_sessions::save_session(paths, &mut candidate).map_err(CliFailure::from)?;
    match save_config() {
        Ok(()) => {
            *record = candidate;
            Ok(())
        }
        Err(product_config::ConfigSaveError::BeforePublication(error)) => {
            prior.store_revision = candidate.store_revision;
            match product_sessions::save_session(paths, &mut prior) {
                Ok(()) => {
                    *record = prior;
                    Err(CliFailure::new(format!(
                        "host default config write failed: {error}; session change rolled back"
                    )))
                }
                Err(rollback) => Err(CliFailure::new(format!(
                    "host default config write failed: {error}; session rollback failed: {rollback}; recovery required"
                ))),
            }
        }
        Err(product_config::ConfigSaveError::PublishedDurabilityUncertain(error)) => {
            *record = candidate;
            Err(CliFailure::new(format!(
                "host default config was published but directory sync failed: {error}; durability uncertain; recovery required"
            )))
        }
    }
}

#[test]
fn product_gate_allows_inference() {
    let limits = product_subscription_request_limits()
        .unwrap_or_else(|()| panic!("product subscription request limits must be valid"));
    assert!(
        limits.allowance(plakous::subscriptions::SubscriptionRequestClass::Inference) > 0,
        "product turns must not use catalog-only inference=0 gates"
    );
}

#[test]
fn usage_refresh_is_offered_only_when_a_refresh_is_needed() {
    use commands::usage::{refresh_usage_credential, usage_refresh_accounts};
    for (credential, expired, offered) in [
        (CredentialState::Ready, false, &[][..]),
        (
            CredentialState::Ready,
            true,
            &["codex", "grok", "claude"][..],
        ),
        (
            CredentialState::RefreshRequired,
            false,
            &["codex", "grok", "claude"][..],
        ),
        (CredentialState::Quarantined, false, &["grok"][..]),
        (CredentialState::Refreshing, true, &[][..]),
        (CredentialState::SignedOut, true, &[][..]),
        (CredentialState::LoginRequired, true, &[][..]),
        (CredentialState::Revoked, true, &[][..]),
        (CredentialState::ProfileChanged, true, &[][..]),
        (CredentialState::WrittenByNewerBuild, true, &[][..]),
    ] {
        let mut backend = FakeBackend::new(CompatibilityState::Current, CredentialState::Ready);
        backend.credential = credential;
        backend.access_token_expired = expired;
        assert_eq!(
            usage_refresh_accounts(&mut backend),
            offered,
            "{credential:?} expired={expired}"
        );
        // The offered set is decided offline: only a Ready posture checks
        // the stored expiry, and nothing reads usage to decide.
        assert!(
            !backend
                .calls
                .iter()
                .any(|call| matches!(call, Call::Usage(_)))
        );
        assert_eq!(
            backend
                .calls
                .iter()
                .any(|call| matches!(call, Call::Expiry(_))),
            credential == CredentialState::Ready
        );
    }

    for provider in [
        NativeProvider::Codex,
        NativeProvider::Claude,
        NativeProvider::Xai,
    ] {
        for credential in [
            CredentialState::Ready,
            CredentialState::SignedOut,
            CredentialState::LoginRequired,
            CredentialState::Revoked,
            CredentialState::ProfileChanged,
            CredentialState::WrittenByNewerBuild,
            CredentialState::RefreshRequired,
            CredentialState::Refreshing,
            CredentialState::Quarantined,
        ] {
            // A button may outlive its snapshot: activation revalidates the
            // posture, refreshes only what still needs it and otherwise lets
            // the caller re-read.
            let admitted = matches!(
                credential,
                CredentialState::Ready
                    | CredentialState::RefreshRequired
                    | CredentialState::Refreshing
            ) || (provider == NativeProvider::Xai
                && credential == CredentialState::Quarantined);
            let refreshes = credential == CredentialState::RefreshRequired
                || (provider == NativeProvider::Xai && credential == CredentialState::Quarantined);
            let mut backend = FakeBackend::new(CompatibilityState::Current, CredentialState::Ready);
            backend.credential = credential;
            assert_eq!(
                refresh_usage_credential(
                    &mut backend,
                    &cli_provider_name(provider).parse().unwrap()
                )
                .is_ok(),
                admitted,
                "{provider:?} {credential:?}"
            );
            assert_eq!(
                backend.calls.contains(&Call::RefreshNow(provider)),
                refreshes
            );
            assert_eq!(
                backend.calls.contains(&Call::Expiry(provider)),
                credential == CredentialState::Ready
            );
            assert!(!backend.calls.contains(&Call::Usage(provider)));
            assert!(
                !backend
                    .calls
                    .iter()
                    .any(|call| matches!(call, Call::Login(..) | Call::Refresh(_)))
            );
        }
    }

    // The common SuperGrok case: the offline posture stays Ready while the
    // stored access token expired. The action is offered and runs the claimed
    // refresh after the local expiry check, with no usage read first.
    for provider in [
        NativeProvider::Xai,
        NativeProvider::Codex,
        NativeProvider::Claude,
    ] {
        let mut expired = FakeBackend::new(CompatibilityState::Current, CredentialState::Ready);
        expired.credential = CredentialState::Ready;
        expired.access_token_expired = true;
        assert_eq!(
            usage_refresh_accounts(&mut expired),
            ["codex", "grok", "claude"]
        );
        expired.calls.clear();
        refresh_usage_credential(&mut expired, &cli_provider_name(provider).parse().unwrap())
            .unwrap();
        assert_eq!(
            expired.calls,
            vec![
                Call::Status(provider),
                Call::Expiry(provider),
                Call::RefreshNow(provider)
            ]
        );

        let mut valid = FakeBackend::new(CompatibilityState::Current, CredentialState::Ready);
        valid.credential = CredentialState::Ready;
        refresh_usage_credential(&mut valid, &cli_provider_name(provider).parse().unwrap())
            .unwrap();
        assert_eq!(
            valid.calls,
            vec![Call::Status(provider), Call::Expiry(provider)]
        );
    }

    let mut signed_out = FakeBackend::new(CompatibilityState::Current, CredentialState::Ready);
    assert_eq!(usage_refresh_accounts(&mut signed_out), [] as [String; 0]);
    let refusal = refresh_usage_credential(&mut signed_out, &"grok".parse().unwrap()).unwrap_err();
    assert!(
        refusal
            .message()
            .contains("can no longer be refreshed from Usage"),
        "{}",
        refusal.message()
    );

    let mut failure = FakeBackend::new(CompatibilityState::Current, CredentialState::Ready);
    failure.credential = CredentialState::RefreshRequired;
    failure.refresh_failure = Some("scripted credential refresh failed");
    assert!(
        refresh_usage_credential(&mut failure, &"claude".parse().unwrap())
            .unwrap_err()
            .message()
            .contains("scripted credential refresh failed")
    );
    assert_eq!(
        failure.calls,
        vec![
            Call::Status(NativeProvider::Claude),
            Call::RefreshNow(NativeProvider::Claude)
        ]
    );
    let mut unready = FakeBackend::new(CompatibilityState::Current, CredentialState::Quarantined);
    unready.credential = CredentialState::Quarantined;
    assert!(
        refresh_usage_credential(&mut unready, &"grok".parse().unwrap())
            .unwrap_err()
            .message()
            .contains("did not produce a ready credential")
    );
    let mut inactive = FakeBackend::new(CompatibilityState::Required, CredentialState::Ready);
    inactive.credential = CredentialState::Ready;
    assert_eq!(usage_refresh_accounts(&mut inactive), [] as [String; 0]);
    assert!(refresh_usage_credential(&mut inactive, &"claude".parse().unwrap()).is_err());
    assert!(
        !inactive
            .calls
            .iter()
            .any(|call| matches!(call, Call::RefreshNow(_) | Call::Usage(_) | Call::Expiry(_)))
    );
}
