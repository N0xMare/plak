#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]

use std::io::Cursor;

use super::*;
use crate::command::{RunEffort, run_effort_levels, run_effort_usage};
use crate::presentation::credential_locations_text;
use plakous::subscriptions::NativeLoginMode;

#[test]
fn import_resume_post_commit_failure_names_the_successor_and_recovery_command() {
    let failure = import_resume_failure(
        CliFailure::new("synthetic TUI failure"),
        "new-session",
        "old-session",
    );
    assert_eq!(
        failure.message(),
        "synthetic TUI failure; session new-session was carried forward from old-session; fix the error and run: plak --resume new-session"
    );
}

#[test]
fn reload_handoff_reuses_the_tui_probe_verdict_without_reprobing() {
    let root = tempfile::tempdir().unwrap();
    let paths = product_paths::PlakPaths::explicit(root.path().join("home"));
    let mut record = product_sessions::SessionRecord::new(root.path().to_path_buf());
    let key = home::state::state_key();
    let target = carry::ReloadTarget {
        executable: root.path().join("absent-plak"),
        state_key: Some(key.clone()),
        probe: Some(carry::ReloadProbe {
            state_key: key,
            export_versions: carry::IMPORTABLE_VERSIONS.to_vec(),
            config_error: Some("synthetic config refusal".to_owned()),
        }),
    };
    let note = reload_installed_plak(
        &paths,
        &store::session_records::SqliteSessionStore::new(paths.clone()),
        &mut record,
        Some(target),
    )
    .unwrap();
    assert!(note.contains("synthetic config refusal"), "{note}");
    assert!(!paths.runtime_database().exists());
}

/// A piped `plak` keeps exit 2 but must hand the reader a command, not a
/// refusal. Both forms of that command are pinned here.
#[test]
fn the_non_terminal_refusal_names_the_exact_command_to_run() {
    assert_eq!(headless_run_command_for(true), "plak run -- \"<prompt>\"");
    assert_eq!(
        headless_run_command_for(false),
        "plak run --account <codex|grok|claude|openrouter|openai|anthropic|xai-api> --model <model> -- \"<prompt>\""
    );
}

/// `plak status` on an emptied home still says Ready. The line that
/// explains why is text a reader depends on, so it is pinned.
#[test]
fn status_says_where_each_kind_of_credential_lives() {
    assert_eq!(
        credential_locations_text("/s/plak/subscriptions", "/h/secrets"),
        "OAuth credentials live in /s/plak/subscriptions, outside the product home, so emptying it does not log you out. API keys live in /h/secrets."
    );
}

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
    assert_eq!(
        persist_miss_native_tool_mode("grok").unwrap(),
        (None, false)
    );
    assert_eq!(
        persist_miss_native_tool_mode("claude").unwrap(),
        (None, false)
    );
    assert_eq!(
        persist_miss_native_tool_mode("openrouter").unwrap(),
        (None, false)
    );
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
fn native_subscription_failures_name_their_repair_and_class() {
    let state = Path::new("/nonexistent-plak-state");
    let codex = Some(NativeProvider::Codex);
    for (error, account, expected, exit) in [
        (
            NativeSubscriptionError::CredentialUnavailable,
            codex,
            "no ready codex credential (signed out, quarantined or revoked); run `plak login codex`",
            3,
        ),
        (
            NativeSubscriptionError::AcceptanceRequired,
            Some(NativeProvider::Xai),
            "the grok compatibility notice is not accepted; run `plak login grok` to review and accept it",
            3,
        ),
        (
            NativeSubscriptionError::CatalogRefreshRejected,
            Some(NativeProvider::Claude),
            "native catalog credential refresh was rejected; run `plak login claude` to authorize again",
            3,
        ),
        (
            NativeSubscriptionError::AuthenticationProviderRejected,
            codex,
            "native subscription provider rejected authentication; run `plak login codex` to authorize again",
            3,
        ),
        (
            NativeSubscriptionError::LoginTokenRejected,
            None,
            "provider rejected the authorization-code token exchange; start a fresh login with `plak login <account>`",
            3,
        ),
        (
            NativeSubscriptionError::LoginTokenTransport,
            codex,
            "native subscription token request did not complete; check the network, then start a fresh login with `plak login codex`",
            3,
        ),
        (
            NativeSubscriptionError::CredentialProfileChanged,
            codex,
            "the stored codex login was written by an older build or another profile; run `plak login codex`, which replaces it",
            3,
        ),
        (
            NativeSubscriptionError::ModelUnavailable,
            codex,
            "requested native subscription model is unavailable; run `plak models codex` and choose a listed model, effort and composition",
            1,
        ),
        (
            NativeSubscriptionError::ProviderConfigurationPolicyMismatch(
                "synthetic-private-policy-detail",
            ),
            codex,
            "explicit native provider behavior did not match provider policy; run `plak models codex` and choose a listed model, effort and composition",
            1,
        ),
        (
            NativeSubscriptionError::CatalogDocumentRejected,
            codex,
            "native catalog document was rejected; the provider's catalog does not match this build's pinned contract; retry later, and if it persists use a newer plak",
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
    assert!(inactive.message().contains("build a newer plak"));
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
        invoke(Command::Models(NativeProvider::Codex), &mut backend, b"");
    let failure = result.unwrap_err();
    assert_eq!(failure.exit_code(), 3);
    assert_eq!(
        failure.message(),
        "the codex compatibility notice needs an answer on stdin and none came; accept it once with `plak login codex`, or pipe `y` to this command"
    );
    assert_eq!(stdout, [] as [u8; 0]);
    assert_eq!(
        String::from_utf8(stderr).unwrap(),
        "codex uses an experimental revision-pinned compatibility integration.\n\
             profile: fixture-profile\n\
             reviewed: 2026-07-31 UTC\n\
             expires: 2026-10-31 UTC; after that this build refuses codex until a newer build replaces the profile\n\
             Continue? [y/N] \n"
    );
    assert_eq!(backend.calls, vec![Call::Status(NativeProvider::Codex)]);

    // `plak login` is the remedy elsewhere, so its own refusal must not
    // send the operator back to itself.
    let mut login = FakeBackend::new(CompatibilityState::Required, CredentialState::Ready);
    let (result, _, _) = invoke(
        Command::AuthLogin {
            provider: NativeProvider::Codex,
            mode: NativeLoginMode::Browser,
        },
        &mut login,
        b"",
    );
    let failure = result.unwrap_err();
    assert_eq!(failure.exit_code(), 3);
    assert_eq!(
        failure.message(),
        "the codex compatibility notice needs an answer on stdin and none came; answer `y` from a terminal, or pipe `y` on stdin"
    );

    let mut declined = FakeBackend::new(CompatibilityState::Required, CredentialState::Ready);
    declined.credential = CredentialState::Ready;
    let (result, _, _) = invoke(
        Command::Models(NativeProvider::Codex),
        &mut declined,
        b"n\n",
    );
    let failure = result.unwrap_err();
    assert_eq!(failure.exit_code(), 3);
    assert!(failure.message().contains("run `plak login codex`"));

    let mut inactive = FakeBackend::new(CompatibilityState::Inactive, CredentialState::Ready);
    inactive.credential = CredentialState::Ready;
    let (result, _, _) = invoke(Command::Models(NativeProvider::Codex), &mut inactive, b"");
    let failure = result.unwrap_err();
    assert_eq!(failure.exit_code(), 3);
    assert!(
        failure.message().contains("build a newer plak"),
        "{}",
        failure.message()
    );
}

#[test]
fn a_credential_that_is_not_ready_names_its_repair_and_exits_3() {
    for (state, expected) in [
        (
            CredentialState::SignedOut,
            "codex is signed out; run `plak login codex`",
        ),
        (
            CredentialState::RefreshRequired,
            "codex credential needs a refresh; run `plak refresh codex`, or `plak login codex` if that fails",
        ),
        (
            CredentialState::Quarantined,
            "codex credential is quarantined after an uncertain refresh; run `plak login codex`",
        ),
        (
            CredentialState::Revoked,
            "codex credential was revoked; run `plak login codex`",
        ),
    ] {
        let mut backend = FakeBackend::new(CompatibilityState::Current, state);
        let (result, _, _) = invoke(
            Command::AuthRefresh(NativeProvider::Codex),
            &mut backend,
            b"",
        );
        let failure = result.unwrap_err();
        assert_eq!(failure.message(), expected);
        assert_eq!(failure.exit_code(), 3);
    }
}

#[test]
fn status_failure_cells_name_the_same_repair_as_the_command_failure() {
    let state = Path::new("/nonexistent-plak-state");
    let cell = commands::support::oauth_status_failure_cell(
        NativeSubscriptionError::CredentialProfileChanged,
        NativeProvider::Claude,
        state,
    );
    assert_eq!(cell, "Profile changed · run plak login claude");
    let long = native_subscription_failure(
        NativeSubscriptionError::CredentialProfileChanged,
        Some(NativeProvider::Claude),
        state,
    );
    assert!(long.message().contains("which replaces it"));
    assert!(!long.message().contains("plak logout"));
    assert!(long.message().contains("plak login claude"));
    assert_eq!(
        commands::support::oauth_status_failure_cell(
            NativeSubscriptionError::CredentialUnavailable,
            NativeProvider::Codex,
            state,
        ),
        "Login required · plak login codex"
    );
    assert_eq!(
        status_credential_cell(NativeProvider::Codex, CredentialState::LoginRequired),
        "Login required · plak login codex"
    );
    assert_eq!(
        status_credential_cell(NativeProvider::Codex, CredentialState::Ready),
        "Ready"
    );
    assert_eq!(
        api_key_status_failure_cell("run `plak key set openai`"),
        "Unreadable · run plak key set openai"
    );
}

/// A record refused as another profile is a state of the one offline
/// classification, read beside its compatibility acceptance: status,
/// login and doctor see it alike instead of a read failure.
#[cfg(unix)]
#[test]
fn a_profile_changed_record_reads_as_a_state_beside_its_acceptance() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().unwrap();
    let state = std::fs::canonicalize(root.path()).unwrap().join("state");
    let subscriptions = state.join("plak/subscriptions");
    for name in ["codex", "xai", "claude", "catalog", "compatibility"] {
        std::fs::create_dir_all(subscriptions.join(name)).unwrap();
    }
    for dir in [state.clone(), state.join("plak"), subscriptions.clone()]
        .into_iter()
        .chain(
            ["codex", "xai", "claude", "catalog", "compatibility"]
                .map(|name| subscriptions.join(name)),
        )
    {
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let record = subscriptions.join("claude/credential.json");
    let bytes: &[u8] = br#"{"format_version":2,"profile_id":"anthropic.claude.oauth.compat.2026-01-01.r1","slot_id":"primary","account_binding":"old-claude-grant","generation":1,"status":"ready","payload":"e30="}"#;
    std::fs::write(&record, bytes).unwrap();
    std::fs::set_permissions(&record, std::fs::Permissions::from_mode(0o600)).unwrap();

    let mut app = PlakousApplication::for_accounts(state).unwrap();
    let status = AccountOps::offline_status(&mut app, NativeProvider::Claude).unwrap();
    assert_eq!(
        status,
        CliStatus {
            compatibility: CompatibilityState::Required,
            credential: CredentialState::ProfileChanged,
        }
    );
    assert_eq!(
        commands::application::CredentialVerdict::from_status(status),
        commands::application::CredentialVerdict::NeedsLogin(
            commands::application::LoginReason::ProfileChanged
        )
    );
    let mut stdout = Vec::new();
    auth_status(&mut app, NativeProvider::Claude, None, &mut stdout).unwrap();
    assert_eq!(
        String::from_utf8(stdout).unwrap(),
        "claude: compatibility=acceptance-required, credential=profile-changed; run plak login claude\n"
    );
    assert_eq!(
        std::fs::read(&record).unwrap(),
        bytes,
        "reads leave it untouched"
    );
}

/// One unreadable OAuth account is a Setup needed row. The other OAuth
/// accounts, including a login-required payload and an unsafe file, and
/// the API-key rows, still print. The table is the report, so the exit
/// is success.
#[cfg(unix)]
#[test]
fn status_prints_every_provider_when_one_credential_cannot_be_read() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().unwrap();
    let state = std::fs::canonicalize(root.path()).unwrap().join("state");
    let subscriptions = state.join("plak/subscriptions");
    for name in ["codex", "xai", "claude", "catalog", "compatibility"] {
        std::fs::create_dir_all(subscriptions.join(name)).unwrap();
    }
    for dir in [
        state.clone(),
        state.join("plak"),
        subscriptions.clone(),
        subscriptions.join("codex"),
        subscriptions.join("xai"),
        subscriptions.join("claude"),
        subscriptions.join("catalog"),
        subscriptions.join("compatibility"),
    ] {
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let codex_profile =
        plakous::subscriptions::NativeSubscriptions::notice(NativeProvider::Codex).profile_id();
    let codex_record = format!(
        r#"{{"format_version":2,"profile_id":"{codex_profile}","slot_id":"primary","account_binding":"legacy-account","generation":1,"status":"ready","payload":"e30="}}"#
    );
    let write = |path: &Path, bytes: &[u8], mode: u32| {
        std::fs::write(path, bytes).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    };
    write(
        &subscriptions.join("codex/credential.json"),
        codex_record.as_bytes(),
        0o600,
    );
    write(
        &subscriptions.join("claude/credential.json"),
        br#"{"format_version":2,"profile_id":"anthropic.claude.oauth.compat.2026-01-01.r1","slot_id":"primary","account_binding":"old-claude-grant","generation":1,"status":"ready","payload":"e30="}"#,
        0o600,
    );
    write(
        &subscriptions.join("xai/credential.json"),
        b"not-a-record",
        0o644,
    );

    let services = Services::for_home(product_paths::PlakPaths::explicit(
        root.path().join("product"),
    ));
    let mut app = PlakousApplication::for_accounts(state).unwrap();
    // An earlier-build login leaves the acceptance receipt plus a cleared
    // recovery marker and epoch. Without them a ready-capable record is
    // quarantined, which is the fail-closed rule, not this row.
    seed_cleared_codex_login_recovery(&mut app, &subscriptions);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let result = execute_command(
        Command::StatusAll,
        &services,
        &mut app,
        &mut Cursor::new([]),
        &mut stdout,
        &mut stderr,
        &mut commands::login_flow::TestLoginUi::default(),
    );
    let stdout = String::from_utf8(stdout).unwrap();
    let stderr = String::from_utf8(stderr).unwrap();
    assert_eq!(
        result.unwrap(),
        ProcessOutcome::Success,
        "stderr={stderr}\nstdout={stdout}"
    );
    assert!(stderr.is_empty(), "{stderr}");
    let row = |label: &str| -> String {
        stdout
            .lines()
            .find(|line| line.contains(label))
            .unwrap_or("")
            .to_owned()
    };
    let claude = row("Claude Subscription");
    assert!(
        claude.contains("Profile changed · run plak login claude"),
        "{claude}\n{stdout}"
    );
    let codex = row("Codex Subscription");
    assert!(
        codex.contains("Login required · plak login codex"),
        "{codex}\n{stdout}"
    );
    let grok = row("Grok Subscription");
    assert!(grok.contains("Unsafe storage"), "{grok}\n{stdout}");
    assert!(grok.contains("chmod 600"), "{grok}\n{stdout}");
    for provider in ["OpenAI API", "Anthropic API", "OpenRouter API", "xAI API"] {
        assert!(
            stdout.contains(provider),
            "{provider} missing from\n{stdout}"
        );
    }
    assert!(stdout.contains("Setup needed"));
    assert!(stdout.contains("Local credential state only; no provider request was made."));
}

/// What `plak login` leaves behind a credential this build cannot parse:
/// the acceptance receipt, and a cleared recovery marker with its epoch,
/// both pinned to that consent and to credential generation 1.
#[cfg(unix)]
fn seed_cleared_codex_login_recovery(app: &mut PlakousApplication, subscriptions: &Path) {
    use std::os::unix::fs::PermissionsExt as _;

    let notice = plakous::subscriptions::NativeSubscriptions::notice(NativeProvider::Codex);
    app.acknowledge_exact(NativeProvider::Codex, notice.notice_revision())
        .expect("codex acceptance");
    let receipt = std::fs::read_to_string(subscriptions.join("compatibility/codex.json")).unwrap();
    let key = "\"generation\":\"";
    let start = receipt
        .find(key)
        .expect("consent generation")
        .saturating_add(key.len());
    let generation = receipt[start..]
        .split('"')
        .next()
        .expect("consent generation end");
    assert_eq!(generation.len(), 64, "{generation}");
    let nonce = "0123456789abcdef".repeat(4);
    let marker = format!(
        "{{\"schema\":\"plak.native-login-recovery\",\"version\":2,\"state\":\"cleared\",\"operation_nonce\":\"{nonce}\",\"consent_generation\":\"{generation}\",\"credential_generation\":1}}\n"
    );
    for name in ["codex.login-recovery", "codex.login-recovery-epoch"] {
        let path = subscriptions.join("compatibility").join(name);
        std::fs::write(&path, marker.as_bytes()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

#[cfg(unix)]
fn mode_700_subscription_tree(state: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;

    let subscriptions = state.join("plak/subscriptions");
    for name in ["codex", "xai", "claude", "catalog", "compatibility"] {
        std::fs::create_dir_all(subscriptions.join(name)).unwrap();
    }
    for dir in [
        state,
        &state.join("plak"),
        &subscriptions,
        &subscriptions.join("codex"),
        &subscriptions.join("xai"),
        &subscriptions.join("claude"),
        &subscriptions.join("catalog"),
        &subscriptions.join("compatibility"),
    ] {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    subscriptions
}

#[cfg(unix)]
fn run_offline_status(
    command: Command,
    state: PathBuf,
    product: PathBuf,
) -> (CliResult<ProcessOutcome>, String, String) {
    let services = Services::for_home(product_paths::PlakPaths::explicit(product));
    let mut app = PlakousApplication::for_accounts(state).unwrap();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let result = execute_command(
        command,
        &services,
        &mut app,
        &mut Cursor::new([]),
        &mut stdout,
        &mut stderr,
        &mut commands::login_flow::TestLoginUi::default(),
    );
    (
        result,
        String::from_utf8(stdout).unwrap(),
        String::from_utf8(stderr).unwrap(),
    )
}

/// A ready-capable record with no recovery marker is quarantined. The
/// table paints that as Unavailable, not as login-required.
#[cfg(unix)]
#[test]
fn status_quarantines_a_ready_capable_codex_login_with_no_recovery_marker() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().unwrap();
    let state = std::fs::canonicalize(root.path()).unwrap().join("state");
    let subscriptions = mode_700_subscription_tree(&state);
    let profile =
        plakous::subscriptions::NativeSubscriptions::notice(NativeProvider::Codex).profile_id();
    let record = format!(
        r#"{{"format_version":2,"profile_id":"{profile}","slot_id":"primary","account_binding":"legacy-account","generation":1,"status":"ready","payload":"e30="}}"#
    );
    let path = subscriptions.join("codex/credential.json");
    std::fs::write(&path, record).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

    let (result, stdout, stderr) =
        run_offline_status(Command::StatusAll, state, root.path().join("product"));
    assert_eq!(
        result.unwrap(),
        ProcessOutcome::Success,
        "stderr={stderr}\nstdout={stdout}"
    );
    let codex = stdout
        .lines()
        .find(|line| line.contains("Codex Subscription"))
        .unwrap_or("");
    assert!(
        codex.contains("Unavailable") && !codex.contains("Login required"),
        "a record with no marker must read Quarantined: {codex}\n{stdout}"
    );
}

/// A repair longer than the cell cap is not cut mid-path. The row names
/// `plak status <account>`, which prints the chmod and the whole path.
#[cfg(unix)]
#[test]
fn status_long_state_path_names_the_account_command() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().unwrap();
    let mut state = std::fs::canonicalize(root.path()).unwrap();
    for index in 0..4 {
        state.push(format!("{index}{}", "d".repeat(70)));
    }
    let subscriptions = mode_700_subscription_tree(&state);
    let credential = subscriptions.join("xai/credential.json");
    std::fs::write(&credential, b"not-a-record").unwrap();
    std::fs::set_permissions(&credential, std::fs::Permissions::from_mode(0o644)).unwrap();
    let product = root.path().join("product");

    let (result, stdout, stderr) =
        run_offline_status(Command::StatusAll, state.clone(), product.clone());
    assert_eq!(
        result.unwrap(),
        ProcessOutcome::Success,
        "stderr={stderr}\nstdout={stdout}"
    );
    let grok = stdout
        .lines()
        .find(|line| line.contains("Grok Subscription"))
        .unwrap_or("");
    assert!(
        grok.contains("Unsafe storage · run plak status grok for the fix"),
        "{grok}\n{stdout}"
    );
    assert!(!grok.contains('…') && !grok.contains("chmod"), "{grok}");

    let (result, stdout, _) =
        run_offline_status(Command::AuthStatus(NativeProvider::Xai), state, product);
    let failure = result.expect_err("status grok must fail");
    assert_eq!(failure.exit_code(), 3, "{stdout}");
    let repair = failure.message();
    assert!(
        repair.contains("chmod 600") && repair.contains(&credential.display().to_string()),
        "{repair}"
    );
    assert!(repair.chars().count() > 512, "{}", repair.chars().count());
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Call {
    Status(NativeProvider),
    Usage(NativeProvider),
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
}

impl FakeBackend {
    fn new(compatibility: CompatibilityState, terminal: CredentialState) -> Self {
        Self {
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
        }
    }
}

impl AccountOps for FakeBackend {
    fn notice(&self, _provider: NativeProvider) -> CliNotice {
        CliNotice {
            profile_id: "fixture-profile",
            notice_revision: "exact-review",
            reviewed_at: 1_785_456_000,
            expires_at: 1_793_491_199,
        }
    }

    fn offline_status(&mut self, provider: NativeProvider) -> CliResult<CliStatus> {
        self.calls.push(Call::Status(provider));
        Ok(CliStatus {
            compatibility: self.compatibility,
            credential: self.credential,
        })
    }

    fn stored_credential(
        &mut self,
        provider: NativeProvider,
    ) -> CliResult<commands::application::StoredCredential> {
        self.calls.push(Call::Status(provider));
        Ok(
            commands::application::CredentialVerdict::from_status(CliStatus {
                compatibility: self.compatibility,
                credential: self.credential,
            })
            .stored(),
        )
    }

    fn subscription_usage(&mut self, provider: NativeProvider) -> CliResult<SubscriptionUsageRead> {
        self.calls.push(Call::Usage(provider));
        Ok(SubscriptionUsageRead::Unavailable {
            observed_at: plakous::model::Timestamp::new(0, 0)
                .unwrap_or_else(|_| unreachable!("epoch is valid")),
            reason: plakous::model::SubscriptionUsageUnavailable::Unqualified,
        })
    }

    fn openrouter_key_usage(&mut self) -> CliResult<product_usage::VerticalObservation> {
        self.openrouter_key_usage
            .clone()
            .map_or(Ok(product_usage::VerticalObservation::NotConnected), Ok)
    }

    fn acknowledge_exact(
        &mut self,
        provider: NativeProvider,
        operator_response: &str,
    ) -> CliResult<()> {
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
        provider: NativeProvider,
        mode: NativeLoginMode,
    ) -> CliResult<commands::application::LoginChallenge<Self::PendingLogin>> {
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

    fn refresh(&mut self, provider: NativeProvider) -> CliResult<CredentialState> {
        self.calls.push(Call::Refresh(provider));
        Ok(self.terminal)
    }

    fn refresh_now(&mut self, provider: NativeProvider) -> CliResult<(CredentialState, u64)> {
        self.calls.push(Call::RefreshNow(provider));
        Ok((self.terminal, 1_720_000_000))
    }

    fn revoke(&mut self, provider: NativeProvider) -> CliResult<commands::application::Revocation> {
        self.calls.push(Call::Revoke(provider));
        Ok(if self.revoke_refused {
            commands::application::Revocation::Unusable
        } else {
            commands::application::Revocation::Revoked(self.terminal)
        })
    }

    fn logout_local(&mut self, provider: NativeProvider) -> CliResult<CredentialState> {
        self.calls.push(Call::LogoutLocal(provider));
        Ok(self.terminal)
    }

    fn replace_newer_build(&mut self, provider: NativeProvider) -> CliResult<CredentialState> {
        self.calls.push(Call::ReplaceNewerBuild(provider));
        self.credential = CredentialState::Revoked;
        Ok(self.credential)
    }

    fn openrouter_models(&mut self) -> CliResult<Vec<String>> {
        self.calls.push(Call::OpenRouterModels);
        if let Some(failure) = self.models_failure.clone() {
            return Err(failure);
        }
        Ok(vec!["author/model".to_owned()])
    }

    fn platform_catalog(
        &mut self,
        provider: plakous_hosted::binding::PlatformProvider,
    ) -> CliResult<plakous_hosted::platform::ApiModelCatalog> {
        Ok(plakous_hosted::platform::bundled_catalog(
            provider,
            "offline test backend",
        ))
    }

    fn models(&mut self, provider: NativeProvider) -> CliResult<Vec<CliModelSummary>> {
        self.calls.push(Call::Models(provider));
        if let Some(failure) = self.models_failure.clone() {
            return Err(failure);
        }
        Ok(self.model_summaries.clone())
    }
}

impl PresentationBackend for FakeBackend {
    fn offline_status(&mut self, provider: NativeProvider) -> CliResult<CliStatus> {
        AccountOps::offline_status(self, provider)
    }

    fn models(&mut self, provider: NativeProvider) -> CliResult<Vec<CliModelSummary>> {
        AccountOps::models(self, provider)
    }
}

fn native_sections_with_checked_api_rows<'a>(
    providers: &[plakous_host_protocol::views::ProviderDef],
    sections: &'a [plakous_host_protocol::views::ProviderSection],
) -> Vec<&'a plakous_host_protocol::views::ProviderSection> {
    use plakous_host_protocol::views::{AuthKind, ProviderState};
    let native = sections
        .iter()
        .filter(|section| matches!(section.account.as_str(), "codex" | "grok" | "claude"))
        .collect::<Vec<_>>();
    assert_eq!(sections.len(), native.len() + 3);
    for (account, models) in [
        (
            "openai",
            &[
                "gpt-6-luna",
                "gpt-6-sol",
                "gpt-5.6-luna",
                "gpt-5.6-sol",
                "gpt-5.6-terra",
            ][..],
        ),
        (
            "xai-api",
            &["grok-4.3", "grok-4.5", "grok-4.6", "grok-4.7"][..],
        ),
        (
            "anthropic",
            &["claude-sonnet-5", "claude-opus-5", "claude-opus-5-5"][..],
        ),
    ] {
        let provider = providers
            .iter()
            .find(|provider| provider.id == account)
            .expect("API provider status");
        let section = sections
            .iter()
            .find(|section| section.account == account)
            .expect("API section");
        assert_eq!(provider.kind, AuthKind::ApiKey);
        assert_eq!(section.ready, provider.state == ProviderState::Ready);
        if section.ready {
            assert_eq!(
                section
                    .entries
                    .iter()
                    .map(|entry| entry.id.as_str())
                    .collect::<Vec<_>>(),
                models
            );
            for entry in &section.entries {
                assert!(entry.reasoning);
                if account == "openai" {
                    assert_eq!(
                        entry.effort_options,
                        ["none", "low", "medium", "high", "xhigh", "max"]
                    );
                } else if account == "xai-api" && entry.id == "grok-4.3" {
                    assert_eq!(
                        entry.effort_options,
                        ["none", "low", "medium", "high", "xhigh"]
                    );
                } else if account == "xai-api" {
                    assert_eq!(entry.effort_options, ["low", "medium", "high", "xhigh"]);
                } else {
                    assert_eq!(entry.effort_options, ["low", "medium"]);
                }
                assert_eq!(
                    entry.extras,
                    [] as [plakous_host_protocol::views::ExtraState; 0]
                );
            }
        } else {
            assert!(matches!(
                provider.state,
                ProviderState::NoKey | ProviderState::Unavailable
            ));
            assert!(
                section.entries.is_empty(),
                "unready API account cannot publish model authority"
            );
        }
    }
    assert!(
        !providers
            .iter()
            .any(|provider| provider.id == "xai" && provider.kind == AuthKind::ApiKey)
    );
    native
}

#[test]
fn tui_catalog_discovery_requires_current_compatibility_and_ready_credentials() {
    let root = tempfile::tempdir().expect("temporary product home");
    let paths = product_paths::PlakPaths::explicit(root.path().to_path_buf());
    for (compatibility, credential, expected_state, expected_detail) in [
        (
            CompatibilityState::Required,
            CredentialState::Ready,
            plakous_host_protocol::views::ProviderState::Expired,
            "acceptance required",
        ),
        (
            CompatibilityState::Inactive,
            CredentialState::Ready,
            plakous_host_protocol::views::ProviderState::Expired,
            "compatibility inactive",
        ),
        (
            CompatibilityState::Current,
            CredentialState::SignedOut,
            plakous_host_protocol::views::ProviderState::NotLoggedIn,
            "",
        ),
        (
            CompatibilityState::Current,
            CredentialState::Quarantined,
            plakous_host_protocol::views::ProviderState::RefreshUnknown,
            "quarantined",
        ),
    ] {
        let mut backend = FakeBackend::new(compatibility, credential);
        backend.credential = credential;
        let (providers, sections) = tui_presentation_values_with_backend(&paths, &mut backend);
        let sections = native_sections_with_checked_api_rows(&providers, &sections);
        assert_eq!(
            sections,
            [] as [&plakous_host_protocol::views::ProviderSection; 0]
        );
        assert_eq!(
            providers
                .iter()
                .map(|provider| provider.id.as_str())
                .collect::<Vec<_>>(),
            [
                "codex",
                "grok",
                "claude",
                "openai",
                "xai-api",
                "anthropic",
                "openrouter",
            ]
        );
        assert!(
            !backend
                .calls
                .iter()
                .any(|call| matches!(call, Call::Models(_)))
        );
        for provider in providers.iter().take(3) {
            assert_eq!(provider.state, expected_state);
            assert_eq!(provider.detail, expected_detail);
            assert!(!provider.state.ready(), "never painted as connected");
        }
    }
}

fn tui_test_locator(checkpoint: u8) -> plakous::sessions::SessionLocator {
    format!(
        "00000000-0000-0000-0000-000000000001/\
             00000000-0000-0000-0000-000000000002/\
             00000000-0000-0000-0000-{checkpoint:012}"
    )
    .parse()
    .expect("valid exact session locator")
}

#[test]
fn exact_resume_launch_is_read_only_until_runtime_attachment() {
    let root = tempfile::tempdir().expect("temporary product home");
    let paths = product_paths::PlakPaths::explicit(root.path().join("product"));
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace directory");
    let mut session = product_sessions::SessionRecord::new(workspace.clone());
    let id = session.id.clone();
    product_sessions::save_session(&paths, &mut session).expect("persist resume source");
    let before = product_sessions::load_session(&paths, &id).expect("load resume source");
    let revision = before.store_revision;
    let updated_at = before.updated_at_unix;
    let mut candidate = before;

    assert!(begin_tui_session_launch(
        &paths,
        &mut candidate,
        true,
        &workspace
    ));

    let after = product_sessions::load_session(&paths, &id).expect("reload resume source");
    assert_eq!(after.store_revision, revision);
    assert_eq!(after.updated_at_unix, updated_at);
    assert!(
        product_session_log::read_tail(&paths.session_home(), &id, 8)
            .expect("read observation tail")
            .iter()
            .all(|line| !line.contains("session.resumed"))
    );
}

#[test]
fn fresh_tui_launch_remains_unpublished_until_user_intent() {
    let root = tempfile::tempdir().expect("temporary product home");
    let paths = product_paths::PlakPaths::explicit(root.path().join("product"));
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace directory");
    let mut candidate = product_sessions::SessionRecord::new(workspace.clone());
    let id = candidate.id.clone();

    assert!(!begin_tui_session_launch(
        &paths,
        &mut candidate,
        false,
        &workspace
    ));
    assert_eq!(candidate.store_revision, 0);
    assert!(product_sessions::load_session(&paths, &id).is_err());
    assert_eq!(
        product_session_log::read_tail(&paths.session_home(), &id, 8)
            .expect("missing observation log is empty"),
        [] as [String; 0]
    );
}

#[test]
fn new_launch_exact_selection_keeps_selected_binding_instead_of_defaults() {
    let root = tempfile::tempdir().expect("temporary product home");
    let paths = product_paths::PlakPaths::explicit(root.path().join("product"));
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace directory");
    let defaults = root_config("codex", "gpt-5.6-sol", Some("medium"));
    let mut active = product_sessions::SessionRecord::new(workspace.clone());
    seed_new_session_binding(&mut active, &defaults).expect("seed new session");
    let former_id = active.id.clone();
    let mut resume = false;
    let mut resume_pending = false;
    let mut former_attachment = None;

    let mut selected = product_sessions::SessionRecord::new(workspace);
    selected.account = Some("grok".to_owned());
    selected.model = Some("grok-4.6".to_owned());
    selected.effort = Some("high".to_owned());
    selected
        .bind_plakous_locator(tui_test_locator(7))
        .expect("bind selected locator");
    product_sessions::save_session(&paths, &mut selected).expect("persist selected session");

    apply_tui_handoff(
        &paths,
        &store::session_records::SqliteSessionStore::new(paths.clone()),
        &mut active,
        &mut resume,
        &mut resume_pending,
        &mut former_attachment,
        tui::TuiHandoff::ResumeSession {
            id: selected.id.clone(),
        },
        &mut None,
    )
    .expect("select exact session");

    assert_eq!(active.account.as_deref(), Some("grok"));
    assert_eq!(active.model.as_deref(), Some("grok-4.6"));
    assert_eq!(active.effort.as_deref(), Some("high"));
    assert_eq!(active.plakous_locator, Some(tui_test_locator(7)));
    assert!(resume, "resume state follows the exact selected record");
    assert!(resume_pending, "selected exact resume is not published yet");
    assert_eq!(
        former_attachment.as_ref().map(|(record, resume, pending)| (
            record.id.as_str(),
            *resume,
            *pending
        )),
        Some((former_id.as_str(), false, false))
    );
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
        run_composition(None, &configured, NativeProvider::Codex, "gpt-5.6-sol"),
        product_config::ProductCompositionSelection::FollowCatalog
    );
    assert_eq!(
        run_composition(None, &configured, NativeProvider::Xai, "grok-4.6"),
        product_config::ProductCompositionSelection::Direct
    );
    assert_eq!(
        run_composition(None, &configured, NativeProvider::Xai, "grok-4.5"),
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
                NativeProvider::Codex,
                "gpt-5.6-sol"
            ),
            composition
        );
    }
}

#[test]
fn invalid_new_session_default_repair_clears_the_dependent_tuple() {
    let root = tempfile::tempdir().expect("temporary product home");
    let paths = product_paths::PlakPaths::explicit(root.path().join("product"));
    let mut config = root_config("grok", "gpt-5.6-sol", Some("high"));
    config.root_template_mut().composition =
        Some(product_config::ProductCompositionSelection::PyPtc);
    let mut session = product_sessions::SessionRecord::new(PathBuf::from("/tmp/workspace"));
    seed_new_session_binding(&mut session, &config).expect("seed new session");

    assert!(repair_invalid_new_session_binding(
        &mut session,
        &mut config
    ));
    assert_eq!(session.model, None);
    assert_eq!(session.effort, None);
    assert_eq!(
        session.target_selection,
        product_config::ProductTargetSelection::Native
    );
    assert_eq!(
        session.composition,
        product_config::ProductCompositionSelection::FollowCatalog
    );
    assert_eq!(config.default_account(), None);
    assert_eq!(config.default_model(), None);
    assert_eq!(config.default_effort(), None);
    assert_eq!(config.default_extra(), None);
    assert_eq!(
        config.default_protocol(),
        product_config::ProductProtocolSelection::Auto
    );
    assert_eq!(
        config.default_composition(),
        product_config::ProductCompositionSelection::FollowCatalog
    );
    config.save(&paths).expect("save repaired defaults once");
    let mut reopened = product_config::ProductConfig::load(&paths).expect("reopen defaults");
    let mut next = product_sessions::SessionRecord::new(PathBuf::from("/tmp/workspace"));
    seed_new_session_binding(&mut next, &reopened).expect("seed new session");
    assert_eq!(next.model, None);
    assert_eq!(next.effort, None);
    assert!(!repair_invalid_new_session_binding(
        &mut next,
        &mut reopened
    ));
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
        tui_attach_eligibility(
            &product_config::ProductTargetSelection::Native,
            &catalog,
            Some("claude"),
            Some("claude-sonnet-5"),
            false,
        ),
        TuiAttachEligibility::Ready
    );
}

#[test]
fn deferred_reasoning_model_with_explicit_effort_attaches_after_discovery() {
    let catalog = ready_attach_catalog("codex", "gpt-reasoning", true);
    assert_eq!(
        tui_attach_eligibility(
            &product_config::ProductTargetSelection::Native,
            &catalog,
            Some("codex"),
            Some("gpt-reasoning"),
            true,
        ),
        TuiAttachEligibility::Ready
    );
    assert_eq!(
        tui_attach_eligibility(
            &product_config::ProductTargetSelection::Native,
            &catalog,
            Some("codex"),
            Some("gpt-reasoning"),
            false,
        ),
        TuiAttachEligibility::EffortRequired
    );
}

#[test]
fn deferred_unknown_native_model_is_refused_even_with_explicit_effort() {
    let catalog = ready_attach_catalog("codex", "gpt-known", true);
    let eligibility = tui_attach_eligibility(
        &product_config::ProductTargetSelection::Native,
        &catalog,
        Some("codex"),
        Some("gpt-unknown"),
        true,
    );
    assert_eq!(eligibility, TuiAttachEligibility::Unavailable);
    assert!(
        deferred_attach_refusal_reason(eligibility, false, None).starts_with("resolve_failed:")
    );
}

/// An unusable stored login is named in the CLI's words, not as a model
/// missing from the catalog, and the shell reason drops CLI backticks.
#[test]
fn unusable_stored_login_names_the_credential_not_a_missing_model() {
    let refusal = commands::support::credential_refusal(
        NativeProvider::Xai,
        Ok(CliStatus {
            compatibility: CompatibilityState::Current,
            credential: CredentialState::ProfileChanged,
        }),
    );
    let reason = deferred_attach_refusal_reason(TuiAttachEligibility::Unavailable, false, refusal);
    assert_eq!(
        reason,
        "credential_unusable: the stored grok login was written by an older build or another profile; run plak login grok, which replaces it"
    );
    for (credential, words) in [
        (
            CredentialState::LoginRequired,
            "made by an earlier plak build",
        ),
        (CredentialState::WrittenByNewerBuild, "newer plak build"),
        (CredentialState::SignedOut, "signed out"),
    ] {
        let refusal = commands::support::credential_refusal(
            NativeProvider::Claude,
            Ok(CliStatus {
                compatibility: CompatibilityState::Current,
                credential,
            }),
        )
        .unwrap_or_default();
        assert!(refusal.contains(words), "{refusal}");
    }
    assert_eq!(
        commands::support::credential_refusal(
            NativeProvider::Codex,
            Ok(CliStatus {
                compatibility: CompatibilityState::Current,
                credential: CredentialState::Ready,
            }),
        ),
        None
    );
    // Missing defaults still win: there is no selected account to name.
    assert_eq!(
        deferred_attach_refusal_reason(
            TuiAttachEligibility::Unavailable,
            true,
            Some("x".to_owned())
        ),
        "missing_defaults"
    );
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
        tui_attach_eligibility(
            &target,
            &plakous_host_protocol::views::ReadyCatalog::default(),
            Some("openrouter"),
            Some("vendor/dynamic-model"),
            false,
        ),
        TuiAttachEligibility::Ready
    );
}

#[test]
fn recoverable_selected_attach_failure_restores_former_exact_attachment() {
    let root = tempfile::tempdir().expect("temporary product home");
    let paths = product_paths::PlakPaths::explicit(root.path().join("product"));
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace directory");
    let mut former = product_sessions::SessionRecord::new(workspace.clone());
    former.account = Some("codex".to_owned());
    former.model = Some("gpt-5.6-sol".to_owned());
    former.effort = Some("medium".to_owned());
    former
        .bind_plakous_locator(tui_test_locator(3))
        .expect("bind former locator");
    let mut selected = product_sessions::SessionRecord::new(workspace);
    selected.account = Some("grok".to_owned());
    selected.model = Some("grok-4.6".to_owned());
    selected.effort = Some("high".to_owned());
    selected
        .bind_plakous_locator(tui_test_locator(9))
        .expect("bind selected locator");
    product_sessions::save_session(&paths, &mut selected).expect("persist selected session");
    let former_id = former.id.clone();
    let mut active = former;
    let mut resume = false;
    let mut resume_pending = false;
    let mut rollback = None;

    apply_tui_handoff(
        &paths,
        &store::session_records::SqliteSessionStore::new(paths.clone()),
        &mut active,
        &mut resume,
        &mut resume_pending,
        &mut rollback,
        tui::TuiHandoff::ResumeSession { id: selected.id },
        &mut None,
    )
    .expect("stage exact selected attachment");
    assert_eq!(active.plakous_locator, Some(tui_test_locator(9)));
    assert!(resume);
    assert!(resume_pending);

    assert!(restore_former_attachment(
        &mut active,
        &mut resume,
        &mut resume_pending,
        &mut rollback
    ));
    assert_eq!(active.id, former_id);
    assert_eq!(active.account.as_deref(), Some("codex"));
    assert_eq!(active.model.as_deref(), Some("gpt-5.6-sol"));
    assert_eq!(active.effort.as_deref(), Some("medium"));
    assert_eq!(active.plakous_locator, Some(tui_test_locator(3)));
    assert!(!resume, "the restored record keeps its former launch state");
    assert!(!resume_pending);
    assert!(rollback.is_none());
}

#[test]
fn failed_in_process_resume_switch_keeps_last_attached_rollback_authority() {
    let root = tempfile::tempdir().expect("temporary product home");
    let paths = product_paths::PlakPaths::explicit(root.path().join("product"));
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace directory");
    let mut former = product_sessions::SessionRecord::new(workspace.clone());
    former
        .bind_plakous_locator(tui_test_locator(1))
        .expect("bind former locator");
    let former_id = former.id.clone();
    let mut failed = product_sessions::SessionRecord::new(workspace.clone());
    failed
        .bind_plakous_locator(tui_test_locator(2))
        .expect("bind failed locator");
    let mut next = product_sessions::SessionRecord::new(workspace);
    next.bind_plakous_locator(tui_test_locator(3))
        .expect("bind next locator");
    product_sessions::save_session(&paths, &mut failed).expect("persist failed selection");
    product_sessions::save_session(&paths, &mut next).expect("persist next selection");

    let mut active = former;
    let mut resume = false;
    let mut resume_pending = false;
    let mut rollback = None;
    apply_tui_handoff(
        &paths,
        &store::session_records::SqliteSessionStore::new(paths.clone()),
        &mut active,
        &mut resume,
        &mut resume_pending,
        &mut rollback,
        tui::TuiHandoff::ResumeSession { id: failed.id },
        &mut None,
    )
    .expect("stage failed selection");

    apply_tui_handoff_after_host(
        &paths,
        &store::session_records::SqliteSessionStore::new(paths.clone()),
        &mut active,
        &mut resume,
        &mut resume_pending,
        &mut rollback,
        tui::TuiHandoff::ResumeSession {
            id: next.id.clone(),
        },
        &mut None,
    )
    .expect("stage next selection after failed host");

    assert_eq!(active.id, next.id);
    assert!(resume_pending);
    assert_eq!(
        rollback
            .as_ref()
            .map(|(record, resume, pending)| (record.id.as_str(), *resume, *pending)),
        Some((former_id.as_str(), false, false))
    );
}

#[test]
fn exact_session_selection_preserves_record_and_refuses_workspace_mismatch() {
    let root = tempfile::tempdir().expect("temporary product home");
    let paths = product_paths::PlakPaths::explicit(root.path().join("product"));
    let workspace = root.path().join("workspace");
    let other_workspace = root.path().join("other-workspace");
    std::fs::create_dir(&workspace).expect("workspace directory");
    std::fs::create_dir(&other_workspace).expect("other workspace directory");
    let mut active = product_sessions::SessionRecord::new(workspace.clone());
    let active_id = active.id.clone();
    let mut selected = product_sessions::SessionRecord::new(workspace);
    selected.account = Some("grok".to_owned());
    selected.model = Some("grok-4.6".to_owned());
    selected.record_notice(product_sessions::TranscriptRow::warning(
        plakous_hosted::sessions::NoticeScope::Tree,
        "selected transcript",
    ));
    selected
        .bind_plakous_locator(tui_test_locator(11))
        .expect("bind selected locator");
    product_sessions::save_session(&paths, &mut selected).expect("persist selected session");

    handle_tui_handoff(
        &paths,
        &store::session_records::SqliteSessionStore::new(paths.clone()),
        &mut active,
        tui::TuiHandoff::ResumeSession {
            id: selected.id.clone(),
        },
        &mut None,
    )
    .expect("select exact session");
    assert_eq!(active.id, selected.id);
    assert_eq!(
        active.transcript,
        vec![product_sessions::TranscriptRow::warning(
            plakous_hosted::sessions::NoticeScope::Tree,
            "selected transcript"
        )]
    );
    assert_eq!(active.plakous_locator, Some(tui_test_locator(11)));

    let mut mismatched = product_sessions::SessionRecord::new(other_workspace);
    product_sessions::save_session(&paths, &mut mismatched).expect("persist mismatched session");
    let selected_id = active.id.clone();
    let failure = handle_tui_handoff(
        &paths,
        &store::session_records::SqliteSessionStore::new(paths.clone()),
        &mut active,
        tui::TuiHandoff::ResumeSession { id: mismatched.id },
        &mut None,
    )
    .expect_err("workspace mismatch refused");
    assert!(failure.message().contains("different workspace"));
    assert_eq!(active.id, selected_id);
    assert_ne!(active.id, active_id);
}

#[test]
fn tui_default_binding_rolls_back_before_publication_and_remains_usable() {
    let root = tempfile::tempdir().expect("temporary product home");
    let paths = product_paths::PlakPaths::explicit(root.path().join("product"));
    let mut record = product_sessions::SessionRecord::new(root.path().to_path_buf());
    record.account = Some("codex".to_owned());
    product_sessions::save_session(&paths, &mut record).expect("persist source");
    let id = record.id.clone();
    let mut candidate = record.clone();
    candidate.account = Some("grok".to_owned());

    let failure = commit_tui_default_candidate(&paths, &mut record, candidate, || {
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
    commit_tui_default_candidate(&paths, &mut record, retry, || Ok(()))
        .expect("retry after rollback");
    assert_eq!(record.account.as_deref(), Some("grok"));
}

#[test]
fn tui_default_binding_fences_post_publication_durability_uncertainty() {
    let root = tempfile::tempdir().expect("temporary product home");
    let paths = product_paths::PlakPaths::explicit(root.path().join("product"));
    let mut record = product_sessions::SessionRecord::new(root.path().to_path_buf());
    record.account = Some("codex".to_owned());
    product_sessions::save_session(&paths, &mut record).expect("persist source");
    let id = record.id.clone();
    let mut candidate = record.clone();
    candidate.account = Some("grok".to_owned());

    let failure = commit_tui_default_candidate(&paths, &mut record, candidate, || {
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

#[test]
fn tui_catalog_failure_is_visible_without_partial_model_authority() {
    let root = tempfile::tempdir().expect("temporary product home");
    let paths = product_paths::PlakPaths::explicit(root.path().to_path_buf());
    let mut backend = FakeBackend::new(CompatibilityState::Current, CredentialState::Ready);
    backend.credential = CredentialState::Ready;
    backend.models_failure = Some(CliFailure::new("catalog unavailable"));
    let (providers, sections) = tui_presentation_values_with_backend(&paths, &mut backend);
    let sections = native_sections_with_checked_api_rows(&providers, &sections);
    assert_eq!(
        sections,
        [] as [&plakous_host_protocol::views::ProviderSection; 0]
    );
    for provider in providers.iter().take(3) {
        assert_eq!(
            provider.state,
            plakous_host_protocol::views::ProviderState::Ready
        );
        assert_eq!(provider.detail, "catalog unavailable");
    }
    assert_eq!(
        backend
            .calls
            .iter()
            .filter(|call| matches!(call, Call::Models(_)))
            .count(),
        3
    );
}

#[test]
fn tui_catalog_preserves_exact_facade_efforts_without_provider_or_model_inference() {
    let root = tempfile::tempdir().expect("temporary product home");
    let paths = product_paths::PlakPaths::explicit(root.path().to_path_buf());
    let mut backend = FakeBackend::new(CompatibilityState::Current, CredentialState::Ready);
    backend.credential = CredentialState::Ready;
    backend.model_summaries = vec![
        CliModelSummary {
            exact_id: "gpt-5.6-luna".to_owned(),
            visibility: NativeModelVisibility::List,
            native_tool_mode: Some(NativeToolMode::CodeModeOnly),
            streaming: true,
            tools: true,
            reasoning_efforts: vec![
                ReasoningEffort::Ultra,
                ReasoningEffort::Low,
                ReasoningEffort::Max,
            ],
            service_tiers: Vec::new(),
            reported_context_length: None,
        },
        CliModelSummary {
            exact_id: "grok-4.6".to_owned(),
            visibility: NativeModelVisibility::List,
            native_tool_mode: None,
            streaming: true,
            tools: false,
            reasoning_efforts: Vec::new(),
            service_tiers: Vec::new(),
            reported_context_length: None,
        },
        CliModelSummary {
            exact_id: "claude-fable-5-1".to_owned(),
            visibility: NativeModelVisibility::List,
            native_tool_mode: None,
            streaming: true,
            tools: true,
            reasoning_efforts: vec![
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
                ReasoningEffort::XHigh,
                ReasoningEffort::Max,
            ],
            service_tiers: Vec::new(),
            reported_context_length: None,
        },
    ];

    let (providers, sections) = tui_presentation_values_with_backend(&paths, &mut backend);
    let sections = native_sections_with_checked_api_rows(&providers, &sections);

    assert_eq!(
        providers[0].state,
        plakous_host_protocol::views::ProviderState::Ready
    );
    assert_eq!(providers[0].detail, "");
    assert_eq!(
        providers[1].state,
        plakous_host_protocol::views::ProviderState::Ready
    );
    assert_eq!(sections.len(), 3);
    assert_eq!(sections[0].entries.len(), 1);
    assert_eq!(sections[0].entries[0].id, "gpt-5.6-luna");
    assert_eq!(
        sections[0].entries[0].effort_options,
        ["low", "max"].map(str::to_owned)
    );
    assert!(sections[0].entries[0].reasoning);
    assert_eq!(sections[1].entries.len(), 1);
    assert_eq!(sections[1].entries[0].id, "grok-4.6");
    assert_eq!(sections[1].entries[0].effort_options, [] as [String; 0]);
    assert!(!sections[1].entries[0].reasoning);
    assert!(sections[2].ready);
    assert_eq!(sections[2].entries.len(), 1);
    assert_eq!(sections[2].entries[0].id, "claude-fable-5-1");
    assert_eq!(
        sections[2].entries[0].effort_options,
        ["low", "medium", "high", "xhigh", "max"].map(str::to_owned)
    );
    assert!(sections[2].entries[0].reasoning);
    assert_eq!(
        providers[2].state,
        plakous_host_protocol::views::ProviderState::Ready
    );
}

#[test]
fn tui_catalog_fails_closed_for_non_displayable_facade_effort() {
    let root = tempfile::tempdir().expect("temporary product home");
    let paths = product_paths::PlakPaths::explicit(root.path().to_path_buf());
    let mut backend = FakeBackend::new(CompatibilityState::Current, CredentialState::Ready);
    backend.credential = CredentialState::Ready;
    for exact_id in ["gpt-5.6-sol", "grok-4.6"] {
        backend.model_summaries.push(CliModelSummary {
            exact_id: exact_id.to_owned(),
            visibility: NativeModelVisibility::List,
            native_tool_mode: (exact_id == "gpt-5.6-sol").then_some(NativeToolMode::CodeModeOnly),
            streaming: true,
            tools: true,
            reasoning_efforts: vec![ReasoningEffort::TokenBudget(
                std::num::NonZeroU32::new(1).expect("non-zero fixture"),
            )],
            service_tiers: Vec::new(),
            reported_context_length: None,
        });
    }

    let (providers, sections) = tui_presentation_values_with_backend(&paths, &mut backend);
    let sections = native_sections_with_checked_api_rows(&providers, &sections);

    assert_eq!(sections.len(), 3);
    assert!(sections.iter().all(|section| section.ready));
    assert!(sections.iter().all(|section| section.entries.is_empty()));
    assert_eq!(providers[0].detail, "no picker-visible supported models");
    assert_eq!(providers[1].detail, "no picker-visible supported models");
}

#[test]
fn tui_catalog_never_offers_models_without_an_exact_process_presentation_candidate() {
    let root = tempfile::tempdir().expect("temporary product home");
    let paths = product_paths::PlakPaths::explicit(root.path().to_path_buf());
    let mut backend = FakeBackend::new(CompatibilityState::Current, CredentialState::Ready);
    backend.credential = CredentialState::Ready;
    backend.model_summaries = vec![
        CliModelSummary {
            exact_id: "gpt-future".to_owned(),
            visibility: NativeModelVisibility::List,
            native_tool_mode: None,
            streaming: true,
            tools: true,
            reasoning_efforts: vec![ReasoningEffort::High],
            service_tiers: Vec::new(),
            reported_context_length: None,
        },
        CliModelSummary {
            exact_id: "grok-future".to_owned(),
            visibility: NativeModelVisibility::List,
            native_tool_mode: None,
            streaming: true,
            tools: true,
            reasoning_efforts: vec![ReasoningEffort::High],
            service_tiers: Vec::new(),
            reported_context_length: None,
        },
    ];

    let (providers, sections) = tui_presentation_values_with_backend(&paths, &mut backend);
    let sections = native_sections_with_checked_api_rows(&providers, &sections);

    assert_eq!(sections.len(), 3);
    assert!(sections.iter().all(|section| section.ready));
    assert!(sections.iter().all(|section| section.entries.is_empty()));
    assert_eq!(providers[0].detail, "no picker-visible supported models");
    assert_eq!(providers[1].detail, "no picker-visible supported models");
}

#[test]
fn tui_catalog_hides_provider_hidden_rows_and_disables_unqualified_code_mode_rows() {
    let root = tempfile::tempdir().expect("temporary product home");
    let paths = product_paths::PlakPaths::explicit(root.path().join("product"));
    let mut backend = FakeBackend::new(CompatibilityState::Current, CredentialState::Ready);
    backend.credential = CredentialState::Ready;
    backend.model_summaries = vec![
        CliModelSummary {
            exact_id: "codex-auto-review".to_owned(),
            visibility: NativeModelVisibility::Hide,
            native_tool_mode: Some(NativeToolMode::CodeModeOnly),
            streaming: true,
            tools: true,
            reasoning_efforts: vec![ReasoningEffort::Medium],
            service_tiers: Vec::new(),
            reported_context_length: None,
        },
        CliModelSummary {
            exact_id: "gpt-5.6-terra".to_owned(),
            visibility: NativeModelVisibility::List,
            native_tool_mode: Some(NativeToolMode::CodeModeOnly),
            streaming: true,
            tools: true,
            reasoning_efforts: vec![ReasoningEffort::Medium],
            service_tiers: Vec::new(),
            reported_context_length: None,
        },
        CliModelSummary {
            exact_id: "gpt-5.6-luna".to_owned(),
            visibility: NativeModelVisibility::List,
            native_tool_mode: Some(NativeToolMode::CodeModeOnly),
            streaming: true,
            tools: true,
            reasoning_efforts: vec![ReasoningEffort::Medium],
            service_tiers: Vec::new(),
            reported_context_length: None,
        },
    ];

    let (_, sections) = tui_presentation_values_with_backend(&paths, &mut backend);
    let codex = sections
        .iter()
        .find(|section| section.account == "codex")
        .expect("Codex section");
    assert_eq!(codex.entries.len(), 2);
    assert!(
        codex
            .entries
            .iter()
            .all(|entry| entry.id != "codex-auto-review")
    );
    let terra = codex
        .entries
        .iter()
        .find(|entry| entry.id == "gpt-5.6-terra")
        .expect("catalog-present Terra");
    assert!(terra.unavailable_reason.is_none());
    assert!(terra.tag.contains("reasoning"));
    assert!(
        codex
            .entries
            .iter()
            .find(|entry| entry.id == "gpt-5.6-luna")
            .is_some_and(|entry| entry.unavailable_reason.is_none())
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
    let services = Services::from_env(None);
    let result = execute_command(
        command,
        &services,
        backend,
        &mut stdin,
        &mut stdout,
        &mut stderr,
        &mut commands::login_flow::TestLoginUi::default(),
    );
    (result, stdout, stderr)
}

fn status_report(backend: &mut FakeBackend, paths: &product_paths::PlakPaths) -> String {
    let services = Services::for_home(paths.clone());
    let mut stdout = Vec::new();
    let result = execute_command(
        Command::StatusAll,
        &services,
        backend,
        &mut Cursor::new([]),
        &mut stdout,
        &mut Vec::new(),
        &mut commands::login_flow::TestLoginUi::default(),
    );
    assert_eq!(result.unwrap(), ProcessOutcome::Success);
    String::from_utf8(stdout).unwrap()
}

#[test]
fn cli_status_last_failed_refresh_fixture_is_shared_and_offline()
-> Result<(), Box<dyn std::error::Error>> {
    let (_guard, paths) = product_paths::PlakPaths::isolated_test_home();
    paths.ensure_layout()?;
    drop(telemetry_store::TelemetryStore::open(&paths)?);
    let db = rusqlite::Connection::open(paths.telemetry_database())?;
    db.execute_batch(
        "INSERT INTO credential_refreshes
            (at, provider, phase, failure_class, transport_phase, generation) VALUES
            (1720000100000000000, 'claude', 'restored', 'timeout', 'resolution', 2);",
    )?;
    let expected = product_doctor::last_failed_refresh(&paths, "claude", local_time_zone())
        .ok_or("missing failure")?;
    let mut backend = FakeBackend::new(CompatibilityState::Current, CredentialState::Ready);
    backend.credential = CredentialState::RefreshRequired;
    let report = status_report(&mut backend, &paths);
    assert!(report.contains(&expected), "{report}");
    assert_eq!(
        backend.calls,
        vec![
            Call::Status(NativeProvider::Codex),
            Call::Status(NativeProvider::Xai),
            Call::Status(NativeProvider::Claude)
        ]
    );

    backend.calls.clear();
    let services = Services::for_home(paths);
    let mut stdout = Vec::new();
    assert_eq!(
        execute_command(
            Command::AuthStatus(NativeProvider::Claude),
            &services,
            &mut backend,
            &mut Cursor::new([]),
            &mut stdout,
            &mut Vec::new(),
            &mut commands::login_flow::TestLoginUi::default()
        )?,
        ProcessOutcome::Success
    );
    let text = String::from_utf8(stdout)?;
    assert!(text.contains(&expected), "{text}");
    assert_eq!(backend.calls, vec![Call::Status(NativeProvider::Claude)]);
    Ok(())
}

/// A heading with no rows is noise. All-ready output has no Setup needed
/// section; all-unready output has no Ready section.
#[test]
fn status_omits_an_empty_ready_or_setup_section() {
    let home = tempfile::tempdir().unwrap();
    let paths = product_paths::PlakPaths::explicit(home.path().join("product"));
    for account in ["openai", "xai-api", "anthropic", "openrouter"] {
        let present = product_secrets::key_status(&paths, account)
            .is_ok_and(|status| status.source != product_secrets::KeySource::Missing);
        if !present {
            product_secrets::set_api_key(&paths, account, "synthetic-status-key").unwrap();
        }
    }
    let mut ready = FakeBackend::new(CompatibilityState::Current, CredentialState::Ready);
    ready.credential = CredentialState::Ready;
    let ready_out = status_report(&mut ready, &paths);
    assert!(ready_out.starts_with("Ready\n"), "{ready_out}");
    assert!(
        !ready_out.contains("Setup needed"),
        "empty Setup needed section was printed:\n{ready_out}"
    );
    assert!(ready_out.contains("Codex Subscription"));
    assert!(ready_out.contains("OpenAI API"));

    let mut signed_out = FakeBackend::new(CompatibilityState::Current, CredentialState::SignedOut);
    let bare = tempfile::tempdir().unwrap();
    let bare_paths = product_paths::PlakPaths::explicit(bare.path().join("product"));
    let unsigned = status_report(&mut signed_out, &bare_paths);
    let key_ready = ["openai", "xai-api", "anthropic", "openrouter"]
        .iter()
        .any(|account| {
            product_secrets::key_status(&bare_paths, account)
                .is_ok_and(|status| status.source != product_secrets::KeySource::Missing)
        });
    if key_ready {
        assert!(unsigned.contains("Ready\n"), "{unsigned}");
    } else {
        assert!(
            !unsigned.contains("\nReady\n") && !unsigned.starts_with("Ready\n"),
            "empty Ready section was printed:\n{unsigned}"
        );
    }
    assert!(unsigned.contains("Setup needed\n"), "{unsigned}");
    assert!(unsigned.contains("Missing"));
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
        &mut backend,
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
    let surface = shell::settings::ProductSurface::new(paths.clone());
    assert_eq!(surface.child_wake_enabled(), Some(false));
    let mut config = product_config::ProductConfig::load_user(&paths).unwrap();
    config.set_key("turn.auto_continue", "true").unwrap();
    config.save(&paths).unwrap();
    assert_eq!(surface.child_wake_enabled(), Some(true));
}

#[test]
fn command_and_attach_workers_share_one_lazy_product_store() {
    let home = tempfile::tempdir().unwrap();
    let paths = product_paths::PlakPaths::explicit(home.path().join("product"));
    let services = Services::for_home(paths.clone());
    let first_roots = services.state_roots().unwrap();
    let second_roots = services.state_roots().unwrap();
    assert!(!paths.product_database().exists());
    assert_eq!(services.product_store_opens(), 0);

    let list = || {
        let mut backend = FakeBackend::new(CompatibilityState::Current, CredentialState::Ready);
        let mut output = Vec::new();
        execute_command(
            Command::SessionList,
            &services,
            &mut backend,
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

#[test]
fn command_store_reopens_after_product_home_replacement() {
    let home = tempfile::tempdir().unwrap();
    let paths = product_paths::PlakPaths::explicit(home.path().join("product"));
    let services = Services::for_home(paths.clone());
    let records = services.session_records().unwrap();
    let workspace = env::current_dir().unwrap();
    let mut original = product_sessions::SessionRecord::new(workspace.clone());
    records.save(&mut original).unwrap();
    std::fs::rename(paths.home(), home.path().join("retired-product")).unwrap();
    let mut replacement = product_sessions::SessionRecord::new(workspace);
    product_sessions::save_session(&paths, &mut replacement).unwrap();

    let mut backend = FakeBackend::new(CompatibilityState::Current, CredentialState::Ready);
    let mut output = Vec::new();
    execute_command(
        Command::SessionList,
        &services,
        &mut backend,
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

/// "Edit raw config" returned from `$EDITOR`
/// bound to an earlier session, discarding a rebind, a `/clear`, and every
/// turn after it. `/clear` replaces the attached session inside the host;
/// the launcher's own record still names the session the process started
/// on, so every terminal re-entry adopts the host-authoritative identity
/// the presentation carried out with it.
#[test]
fn a_terminal_handoff_reenters_on_the_host_authoritative_session() {
    let root = tempfile::tempdir().unwrap();
    let paths = product_paths::PlakPaths::explicit(root.path().join("home"));
    paths.ensure_layout().unwrap();
    let cwd = root.path().join("workspace");
    std::fs::create_dir_all(&cwd).unwrap();

    let mut launched = product_sessions::SessionRecord::new(cwd.clone());
    product_sessions::save_session(&paths, &mut launched).unwrap();
    // What `/clear` produced inside the host while the TUI was attached.
    let mut cleared = product_sessions::SessionRecord::new(cwd.clone());
    product_sessions::save_session(&paths, &mut cleared).unwrap();
    assert_ne!(launched.id, cleared.id);

    let mut record = launched.clone();
    let mut resume = true;
    let mut resume_pending = true;
    let mut former = Some((launched.clone(), true, true));

    adopt_attached_session(
        &store::session_records::SqliteSessionStore::new(paths.clone()),
        &mut record,
        &mut resume,
        &mut resume_pending,
        &mut former,
        Some(cleared.id.as_str()),
    )
    .unwrap();

    assert_eq!(
        record.id, cleared.id,
        "the editor must return to /clear's session"
    );
    assert!(
        !resume,
        "the adopted attachment is live, not an exact resume"
    );
    assert!(!resume_pending);
    assert!(
        former.is_none(),
        "a pre-resume rollback source cannot outlive a different attachment"
    );
}

/// The adoption is inert when the host is still on the launcher's session,
/// so it never disturbs a pending exact resume that has not been published.
#[test]
fn adopting_the_same_session_leaves_a_pending_resume_intact() {
    let root = tempfile::tempdir().unwrap();
    let paths = product_paths::PlakPaths::explicit(root.path().join("home"));
    paths.ensure_layout().unwrap();
    let cwd = root.path().join("workspace");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut attached = product_sessions::SessionRecord::new(cwd);
    product_sessions::save_session(&paths, &mut attached).unwrap();

    let mut record = attached.clone();
    let mut resume = true;
    let mut resume_pending = true;
    let mut former = Some((attached.clone(), true, true));
    for reported in [Some(attached.id.as_str()), None, Some("")] {
        adopt_attached_session(
            &store::session_records::SqliteSessionStore::new(paths.clone()),
            &mut record,
            &mut resume,
            &mut resume_pending,
            &mut former,
            reported,
        )
        .unwrap();
        assert_eq!(record.id, attached.id);
        assert!(resume && resume_pending && former.is_some());
    }
}

/// The exit line names the picker's short id and the workspace, so the
/// operator can paste it from any directory and know where it lands.
#[test]
fn resume_exit_line_names_the_short_id_and_the_workspace() {
    let line = resume_exit_line(
        "1a3c1adc-22f2-4b89-afc6-784807a2f1a5",
        Path::new("/home/op/project"),
    );
    assert_eq!(
        line,
        "Resume this session with: plak resume 1a3c1adc  (workspace /home/op/project)"
    );
    assert!(
        !line.contains("1a3c1adc-22f2"),
        "the hint is the short id, not the full uuid"
    );
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
        thread::spawn(|| Ok(())),
        Duration::from_secs(1),
        "quick",
        Some(&mut quick),
    )
    .unwrap();
    assert!(quick.is_empty(), "{}", String::from_utf8_lossy(&quick));

    let mut slow = Vec::new();
    join_host_worker_bounded_with_notice(
        thread::spawn(|| {
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
    let worker = thread::spawn(|| Err(CliFailure::new("owned root release refused")));
    let error = join_host_worker_bounded(worker, Duration::from_secs(1), "typed").unwrap_err();
    assert_eq!(error.message(), "owned root release refused");
}

/// The TUI error path joins the host worker under the product deadline
/// and returns what it returned; a worker failure is not swallowed.
#[test]
fn tui_shutdown_deadlines_are_at_most_one_second() {
    assert!(commands::support::VOLUNTARY_QUIT_GRACE <= Duration::from_secs(1));
    let started = Instant::now();
    let worker = thread::spawn(|| {
        thread::sleep(Duration::from_millis(1500));
        Ok(())
    });
    assert!(join_tui_host_worker(worker, "test").is_err());
    assert!(started.elapsed() < Duration::from_millis(1100));
}

#[test]
fn tui_error_join_propagates_the_worker_failure() {
    let worker = thread::spawn(|| Err(CliFailure::new("release checkpoint refused")));
    let error = join_tui_host_worker(worker, "TUI session").unwrap_err();
    assert_eq!(error.message(), "release checkpoint refused");
}

#[test]
fn combine_cleanup_preserves_the_worker_failure() {
    let error =
        combine_cleanup(Err(CliFailure::new("release checkpoint refused")), Ok(())).unwrap_err();
    assert_eq!(error.message(), "release checkpoint refused");
}

#[test]
fn combine_cleanup_fails_a_successful_round_on_restoration_failure() {
    let error = combine_cleanup(Ok(()), Err(CliFailure::new("raw mode refused"))).unwrap_err();
    assert_eq!(error.message(), "raw mode refused");
}

#[test]
fn combine_cleanup_retains_both_failure_reasons() {
    let error = combine_cleanup(
        Err(CliFailure::new("release checkpoint refused")),
        Err(CliFailure::new("raw mode refused")),
    )
    .unwrap_err();
    assert!(
        error.message().contains("release checkpoint refused")
            && error.message().contains("raw mode refused"),
        "{}",
        error.message()
    );
}

#[test]
fn host_worker_join_returns_within_its_deadline_and_says_it_detached() {
    join_host_worker_bounded(thread::spawn(|| Ok(())), Duration::from_secs(1), "timely").unwrap();

    let worker = thread::spawn(move || {
        thread::sleep(Duration::from_secs(5));
        Ok(())
    });
    let started = Instant::now();
    let error = join_host_worker_bounded(worker, Duration::from_millis(5), "stuck").unwrap_err();
    assert!(
        error
            .message()
            .contains("exceeded its 5ms shutdown deadline")
    );
    assert!(error.message().contains("detached"));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the deadline is the bound, not a label: {:?}",
        started.elapsed()
    );
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
    assert!(!usage.contains("plak platform"));
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
            Ok(Command::AuthStatus(provider))
        );
        assert_eq!(
            parse(args(&["login", name])),
            Ok(Command::AuthLogin {
                provider,
                mode: NativeLoginMode::Browser,
            })
        );
        assert_eq!(
            parse(args(&["login", name, "--remote"])),
            Ok(Command::AuthLogin {
                provider,
                mode: NativeLoginMode::Remote,
            })
        );
        assert!(parse(args(&["login", name, "--device"])).is_err());
        assert_eq!(
            parse(args(&["refresh", name])),
            Ok(Command::AuthRefresh(provider))
        );
        assert_eq!(
            parse(args(&["logout", name])),
            Ok(Command::AuthLogout {
                provider,
                local_only: false,
            })
        );
        assert_eq!(
            parse(args(&["logout", name, "--local"])),
            Ok(Command::AuthLogout {
                provider,
                local_only: true,
            })
        );
    }
    assert_eq!(
        parse(args(&["login", "grok", "--remote"])),
        Ok(Command::AuthLogin {
            provider: NativeProvider::Xai,
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
            provider: NativeProvider::Xai,
            model: "exact-model".to_owned(),
            effort: None,
            composition: None,
            print_session_id: false,
            prompt: "hello world".into(),
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
            provider: NativeProvider::Codex,
            model: "exact-model".to_owned(),
            effort: None,
            composition: None,
            print_session_id: false,
            prompt: "hello world".into(),
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
            provider: NativeProvider::Xai,
            model: "exact-model".to_owned(),
            effort: Some(ReasoningEffort::High),
            composition: None,
            print_session_id: false,
            prompt: "research me".into(),
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
            provider: NativeProvider::Codex,
            model: "gpt-5.3-codex-spark".to_owned(),
            effort: Some(ReasoningEffort::XHigh),
            composition: None,
            print_session_id: false,
            prompt: "spark me".into(),
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
            provider: NativeProvider::Xai,
            model: "exact-model".to_owned(),
            effort: Some(ReasoningEffort::Medium),
            composition: None,
            print_session_id: false,
            prompt: "patch me".into(),
        })
    );
}

/// `plak models` is read-only catalog discovery, present in every build.
#[test]
fn parses_exact_models_route_for_both_providers() {
    assert_eq!(
        parse(args(&["models", "openrouter"])),
        Ok(Command::ModelsOpenRouter)
    );
    assert!(parse(args(&["models", "unknown"])).is_err());

    for provider in [
        NativeProvider::Codex,
        NativeProvider::Xai,
        NativeProvider::Claude,
    ] {
        assert_eq!(
            parse(args(&["models", cli_provider_name(provider)])),
            Ok(Command::Models(provider))
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
                "reviewed_at=1791936000\n",
                "expires_at=1794614399\n",
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
                "reviewed_at=1785456000\n",
                "expires_at=1801353600\n",
                "kill_switch_revision=xai.grok.compat.kill.2026-07-31.r1\n",
                "killed=false\n",
            ),
        ),
    ];
    for (provider, expected) in cases {
        let mut output = Vec::new();
        assert_eq!(print_compatibility_notice(provider, &mut output), Ok(()));
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
fn compatibility_prompt_precedes_representative_network_actions() {
    let cases = [
        (
            Command::AuthLogin {
                provider: NativeProvider::Codex,
                mode: NativeLoginMode::Browser,
            },
            Call::Login(NativeProvider::Codex, NativeLoginMode::Browser),
            CredentialState::Ready,
        ),
        (
            Command::AuthLogin {
                provider: NativeProvider::Xai,
                mode: NativeLoginMode::Remote,
            },
            Call::Login(NativeProvider::Xai, NativeLoginMode::Remote),
            CredentialState::Ready,
        ),
        (
            Command::AuthRefresh(NativeProvider::Codex),
            Call::RefreshNow(NativeProvider::Codex),
            CredentialState::Ready,
        ),
        (
            Command::AuthLogout {
                provider: NativeProvider::Xai,
                local_only: false,
            },
            Call::Revoke(NativeProvider::Xai),
            CredentialState::Revoked,
        ),
    ];
    for (command, action, terminal) in cases {
        let provider = match &action {
            Call::Login(provider, _)
            | Call::Refresh(provider)
            | Call::RefreshNow(provider)
            | Call::Revoke(provider) => *provider,
            Call::Status(_)
            | Call::Usage(_)
            | Call::Acknowledge(_, _)
            | Call::LogoutLocal(_)
            | Call::ReplaceNewerBuild(_)
            | Call::Models(_)
            | Call::OpenRouterModels => {
                panic!("test case must end in a network action")
            }
        };
        let mut backend = FakeBackend::new(CompatibilityState::Required, terminal);
        backend.credential = CredentialState::Ready;
        let (result, _, _) = invoke(command, &mut backend, b"exact-review\r\nsynthetic-code\r\n");
        assert_eq!(result, Ok(ProcessOutcome::Success));
        assert_eq!(
            backend.calls,
            vec![
                Call::Status(provider),
                Call::Acknowledge(provider, "exact-review".to_owned()),
                action,
            ]
        );
    }
}

#[test]
fn login_over_an_unusable_record_replaces_it_and_says_so_once() {
    for (provider, unusable) in [
        (NativeProvider::Xai, CredentialState::LoginRequired),
        (NativeProvider::Xai, CredentialState::ProfileChanged),
        (NativeProvider::Claude, CredentialState::ProfileChanged),
    ] {
        let mut backend = FakeBackend::new(CompatibilityState::Required, CredentialState::Ready);
        backend.credential = unusable;
        let (result, _, stderr) = invoke(
            Command::AuthLogin {
                provider,
                mode: NativeLoginMode::Remote,
            },
            &mut backend,
            b"exact-review\r\nsynthetic-code\r\n",
        );
        assert_eq!(result, Ok(ProcessOutcome::Success), "{unusable:?}");
        assert!(
            backend
                .calls
                .contains(&Call::Login(provider, NativeLoginMode::Remote)),
            "{unusable:?}: login must reach the provider flow"
        );
        let stderr = String::from_utf8_lossy(&stderr);
        assert_eq!(
            stderr
                .matches("replacing the stored login this build cannot use")
                .count(),
            1,
            "{stderr}"
        );
        assert!(!stderr.contains("plak logout"), "{stderr}");
    }

    let mut healthy = FakeBackend::new(CompatibilityState::Required, CredentialState::Ready);
    healthy.credential = CredentialState::Ready;
    let (_, _, stderr) = invoke(
        Command::AuthLogin {
            provider: NativeProvider::Xai,
            mode: NativeLoginMode::Remote,
        },
        &mut healthy,
        b"exact-review\r\nsynthetic-code\r\n",
    );
    assert!(!String::from_utf8_lossy(&stderr).contains("replacing the stored login"));
}

#[test]
fn refresh_forces_a_grant_and_reports_its_expiry() {
    let mut backend = FakeBackend::new(CompatibilityState::Current, CredentialState::Ready);
    backend.credential = CredentialState::Ready;
    let (result, _, stderr) = invoke(Command::AuthRefresh(NativeProvider::Xai), &mut backend, b"");
    assert_eq!(result, Ok(ProcessOutcome::Success));
    assert!(
        backend
            .calls
            .contains(&Call::RefreshNow(NativeProvider::Xai))
    );
    assert!(!backend.calls.contains(&Call::Refresh(NativeProvider::Xai)));
    let stderr = String::from_utf8_lossy(&stderr);
    assert!(stderr.contains("new refresh grant issued"), "{stderr}");
    assert!(
        stderr.contains("access token valid until 2024-07-03"),
        "{stderr}"
    );
    assert!(!stderr.contains("refresh succeeded"), "{stderr}");
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
            Command::Models(NativeProvider::Codex),
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

        let (result, stdout, stderr) = invoke(Command::Models(provider), &mut backend, b"yes\r\n");

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
        Command::Models(NativeProvider::Codex),
        &mut inactive,
        b"exact-review\n",
    );
    assert!(result.is_err());
    assert_eq!(stdout, [] as [u8; 0]);
    assert_eq!(inactive.calls, vec![Call::Status(NativeProvider::Codex)]);

    let mut rejected = FakeBackend::new(CompatibilityState::Required, CredentialState::Ready);
    rejected.credential = CredentialState::Ready;
    let (result, stdout, stderr) = invoke(
        Command::Models(NativeProvider::Xai),
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
    let (result, stdout, stderr) = invoke(Command::ModelsOpenRouter, &mut backend, b"");
    assert_eq!(result, Ok(ProcessOutcome::Success));
    assert_eq!(stdout, b"author/model\n");
    assert_eq!(stderr, [] as [u8; 0]);
    assert_eq!(backend.calls, vec![Call::OpenRouterModels]);
    backend.calls.clear();
    backend.models_failure = Some(CliFailure::new("catalog failed"));
    let (result, stdout, stderr) = invoke(Command::ModelsOpenRouter, &mut backend, b"");
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
        invoke(Command::Models(NativeProvider::Codex), &mut backend, b"");

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
fn status_and_local_logout_never_select_a_network_action() {
    let mut status_backend =
        FakeBackend::new(CompatibilityState::Required, CredentialState::Unsupported);
    status_backend.credential = CredentialState::RefreshRequired;
    let (result, stdout, stderr) = invoke(
        Command::AuthStatus(NativeProvider::Xai),
        &mut status_backend,
        b"",
    );
    assert_eq!(result, Ok(ProcessOutcome::Success));
    assert_eq!(
        status_backend.calls,
        vec![Call::Status(NativeProvider::Xai)]
    );
    assert_eq!(
        stdout,
        b"grok: compatibility=acceptance-required, credential=refresh-required; run plak refresh grok\n"
    );
    assert_eq!(stderr, [] as [u8; 0]);

    let mut logout_backend =
        FakeBackend::new(CompatibilityState::Required, CredentialState::Revoked);
    let (result, stdout, stderr) = invoke(
        Command::AuthLogout {
            provider: NativeProvider::Codex,
            local_only: true,
        },
        &mut logout_backend,
        b"",
    );
    assert_eq!(result, Ok(ProcessOutcome::Success));
    assert_eq!(
        logout_backend.calls,
        vec![Call::LogoutLocal(NativeProvider::Codex)]
    );
    assert_eq!(stdout, [] as [u8; 0]);
    assert_eq!(
        stderr,
        b"codex: local credential revoked; recovery state cleared; remote credential may remain active\n"
    );
}

#[test]
fn compatibility_failure_stops_before_any_network_action() {
    for compatibility in [
        CompatibilityState::Inactive,
        CompatibilityState::Unsupported,
    ] {
        let mut backend = FakeBackend::new(compatibility, CredentialState::Ready);
        let (result, stdout, _) = invoke(
            Command::AuthRefresh(NativeProvider::Codex),
            &mut backend,
            b"exact-review\n",
        );
        assert!(result.is_err());
        assert_eq!(stdout, [] as [u8; 0]);
        assert_eq!(backend.calls, vec![Call::Status(NativeProvider::Codex)]);
    }

    let mut backend = FakeBackend::new(CompatibilityState::Required, CredentialState::Ready);
    let (result, _, _) = invoke(
        Command::AuthRefresh(NativeProvider::Codex),
        &mut backend,
        b"wrong-review\n",
    );
    assert!(result.is_err());
    assert_eq!(backend.calls, vec![Call::Status(NativeProvider::Codex)]);

    let mut backend = FakeBackend::new(CompatibilityState::Required, CredentialState::Ready);
    let (result, _, _) = invoke(
        Command::AuthRefresh(NativeProvider::Codex),
        &mut backend,
        b" exact-review\n",
    );
    assert!(result.is_err());
    assert_eq!(backend.calls, vec![Call::Status(NativeProvider::Codex)]);
}

#[test]
fn concrete_run_declined_compatibility_cannot_open_a_product_session() {
    let home = tempfile::tempdir().unwrap();
    let paths = product_paths::PlakPaths::explicit(home.path().join("product"));
    let services = Services::for_home(paths.clone());
    let mut app = PlakousApplication::for_accounts(home.path().join("state")).unwrap();
    let mut output = Vec::new();
    let result = execute_application_command(
        Command::Run {
            limits: plakous_hosted::runtime::RunBounds::default(),
            provider: NativeProvider::Codex,
            model: "gpt-5.6-sol".into(),
            effort: None,
            composition: None,
            prompt: "must not dispatch".into(),
            print_session_id: false,
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
fn login_over_a_newer_build_record_replaces_it_only_after_a_yes() {
    let newer = || {
        let mut backend = FakeBackend::new(CompatibilityState::Current, CredentialState::Ready);
        backend.credential = CredentialState::WrittenByNewerBuild;
        backend
    };
    let replaced = |backend: &FakeBackend| {
        backend
            .calls
            .iter()
            .any(|call| matches!(call, Call::ReplaceNewerBuild(_)))
    };
    let prompt = "This login was written by a newer plak build; replacing it means that build must log in again. Continue? [y/N] ";
    for (input, yes) in [
        (&b"y\n"[..], true),
        (b"YES\n", true),
        (b"n\n", false),
        (b"\n", false),
        (b"", false),
        (b"\xff\n", false),
    ] {
        let mut backend = newer();
        let mut stderr = Vec::new();
        let result = commands::login::confirm_newer_build_replacement(
            &mut backend,
            NativeProvider::Xai,
            true,
            &mut Cursor::new(input),
            &mut stderr,
        );
        assert_eq!(result.is_ok(), yes, "{input:?}");
        assert_eq!(replaced(&backend), yes, "{input:?}");
        assert!(String::from_utf8(stderr).unwrap().starts_with(prompt));
        if let Err(failure) = result {
            assert_eq!(
                failure.message(),
                "the stored grok login was written by a newer plak build; it was kept and nothing was changed; use the newer build"
            );
        }
    }

    // Without a terminal nothing is asked, read or replaced.
    let mut backend = newer();
    let mut input = Cursor::new(b"y\n");
    let mut ui = commands::login_flow::TestLoginUi::default();
    let mut stderr = Vec::new();
    let failure = auth_login(
        &mut backend,
        NativeProvider::Codex,
        NativeLoginMode::Browser,
        &mut input,
        &mut stderr,
        &mut ui,
    )
    .unwrap_err();
    assert_eq!(
        failure.message(),
        "the stored codex login was written by a newer plak build; replacing it needs a confirmation, so nothing was changed; confirm it from a terminal, or use the newer build"
    );
    assert_eq!(input.position(), 0);
    assert_eq!(stderr, Vec::<u8>::new());
    assert!(!replaced(&backend));
    assert_eq!(backend.completed_login_modes, []);
}

#[test]
fn automatic_login_keeps_stdin_unread_and_preserves_browser_mode_after_open_failure() {
    let mut backend = FakeBackend::new(CompatibilityState::Current, CredentialState::Ready);
    let mut input = Cursor::new(b"must remain unread\n");
    let mut ui = commands::login_flow::TestLoginUi::default();
    let mut output = Vec::new();
    assert_eq!(
        auth_login(
            &mut backend,
            NativeProvider::Codex,
            NativeLoginMode::Browser,
            &mut input,
            &mut output,
            &mut ui
        ),
        Ok(ProcessOutcome::Success)
    );
    assert_eq!(input.position(), 0);
    assert_eq!(backend.completed_login_modes, [NativeLoginMode::Browser]);
    assert_eq!(backend.login_input, None);
    assert_eq!(backend.login_timeout, Some(oauth_ui::LOCAL_HARD_TIMEOUT));
    assert_eq!(ui.browser_urls, ui.presented_urls);
    assert_eq!(ui.browser_urls.len(), 1);
    assert!(
        !ui.waiting
            .unwrap()
            .load(std::sync::atomic::Ordering::Relaxed)
    );
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("Could not open a local browser"));
    assert!(!output.contains("must remain unread"));
}

#[test]
fn codex_remote_login_shows_the_device_code_and_never_reads_stdin() {
    let mut backend = FakeBackend::new(CompatibilityState::Current, CredentialState::Ready);
    let mut input = Cursor::new(b"must remain unread\n");
    // A device-code login is the SSH path; the SSH refusal is for local login.
    let mut ui = commands::login_flow::TestLoginUi {
        ssh: true,
        ..commands::login_flow::TestLoginUi::default()
    };
    let mut output = Vec::new();
    assert_eq!(
        auth_login(
            &mut backend,
            NativeProvider::Codex,
            NativeLoginMode::Remote,
            &mut input,
            &mut output,
            &mut ui
        ),
        Ok(ProcessOutcome::Success)
    );
    assert_eq!(input.position(), 0);
    assert_eq!(backend.completed_login_modes, [NativeLoginMode::Remote]);
    assert_eq!(backend.login_input, None);
    assert_eq!(backend.login_timeout, Some(Duration::from_secs(930)));
    assert_eq!(ui.browser_urls, [] as [String; 0]);
    assert_eq!(ui.copied_urls, [] as [String; 0]);
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("https://auth.invalid/codex/device"));
    assert!(output.contains("FAKE-CODE"));
    assert!(output.contains("expires in 15 minutes"));
    assert!(output.contains(
        "Continue only if you started this login in plak; if someone gave you this code, cancel."
    ));
    assert!(!output.contains("must remain unread"));
    assert!(!output.to_ascii_lowercase().contains("paste"));
}

#[test]
fn codex_device_login_cancel_expiry_and_refusal_do_not_retry_or_change_credentials() {
    let state = Path::new("/nonexistent-plak-state");
    for (outcome, failure, expected) in [
        (
            Some(commands::application::LoginOutcome::Cancelled),
            None,
            "login cancelled".to_owned(),
        ),
        (
            Some(commands::application::LoginOutcome::TimedOut),
            None,
            "device code expired before it was approved; run `plak login codex --remote`"
                .to_owned(),
        ),
        (
            None,
            Some(NativeSubscriptionError::DeviceCodeExpired),
            "device code expired before it was approved; run `plak login codex --remote`"
                .to_owned(),
        ),
    ] {
        let mut backend = FakeBackend::new(CompatibilityState::Current, CredentialState::Ready);
        backend.credential = CredentialState::Ready;
        backend.login_outcome = outcome;
        let failure_message = failure.map(|error| {
            native_subscription_failure(error, Some(NativeProvider::Codex), state)
                .message()
                .to_owned()
        });
        backend.login_failure = failure_message.map(|message| &*message.leak());
        let mut ui = commands::login_flow::TestLoginUi::default();
        let result = auth_login(
            &mut backend,
            NativeProvider::Codex,
            NativeLoginMode::Remote,
            &mut Cursor::new([]),
            &mut Vec::new(),
            &mut ui,
        );
        let message = result.unwrap_err().message().to_owned();
        assert!(message.contains(&expected), "{message}");
        assert_eq!(
            backend.calls,
            [
                Call::Status(NativeProvider::Codex),
                Call::Login(NativeProvider::Codex, NativeLoginMode::Remote)
            ]
        );
        assert_eq!(backend.completed_login_modes, [NativeLoginMode::Remote]);
        assert_eq!(backend.credential, CredentialState::Ready);
    }
}

#[test]
fn device_login_unavailable_names_the_browser_and_port_forward_remedies() {
    let message = native_subscription_failure(
        NativeSubscriptionError::DeviceLoginUnavailable,
        Some(NativeProvider::Codex),
        Path::new("/nonexistent-plak-state"),
    )
    .message()
    .to_owned();
    assert_eq!(
        message,
        "device-code login is not available for this account/server; remedy: run `plak login codex` on a machine with a browser, or forward port 1455 (ssh -L 1455:localhost:1455 <host>) and run `plak login codex` there"
    );
}

#[test]
fn pasted_login_uses_the_challenge_completion_and_never_echoes_the_response() {
    for (provider, mode, completion, browser_count) in [
        (
            NativeProvider::Xai,
            NativeLoginMode::Remote,
            plakous::subscriptions::NativeLoginCompletion::AuthorizationCodeOrRedirectUrl,
            0,
        ),
        (
            NativeProvider::Claude,
            NativeLoginMode::Browser,
            plakous::subscriptions::NativeLoginCompletion::AuthorizationCodeOrRedirectUrl,
            1,
        ),
    ] {
        let mut backend = FakeBackend::new(CompatibilityState::Current, CredentialState::Ready);
        backend.login_completion = Some(completion);
        let mut input = Cursor::new(b"synthetic-authorization-response\n");
        let mut ui = commands::login_flow::TestLoginUi::default();
        let mut output = Vec::new();
        assert_eq!(
            auth_login(
                &mut backend,
                provider,
                mode,
                &mut input,
                &mut output,
                &mut ui
            ),
            Ok(ProcessOutcome::Success)
        );
        assert_eq!(backend.completed_login_modes, [mode]);
        assert_eq!(
            backend.login_input.as_deref(),
            Some("synthetic-authorization-response")
        );
        assert_eq!(ui.browser_urls.len(), browser_count);
        assert!(ui.waiting.is_none());
        assert!(
            !String::from_utf8(output)
                .unwrap()
                .contains("synthetic-authorization-response")
        );
    }
}

#[test]
fn cancelled_timed_out_and_failed_logins_do_not_retry_or_change_credentials() {
    for (outcome, failure, expected) in [
        (
            Some(commands::application::LoginOutcome::Cancelled),
            None,
            "login cancelled",
        ),
        (
            Some(commands::application::LoginOutcome::TimedOut),
            None,
            "no browser callback within",
        ),
        (None, Some("callback rejected"), "callback rejected"),
    ] {
        let mut backend = FakeBackend::new(CompatibilityState::Current, CredentialState::Ready);
        backend.credential = CredentialState::Ready;
        backend.login_outcome = outcome;
        backend.login_failure = failure;
        let mut ui = commands::login_flow::TestLoginUi::default();
        let result = auth_login(
            &mut backend,
            NativeProvider::Codex,
            NativeLoginMode::Browser,
            &mut Cursor::new([]),
            &mut Vec::new(),
            &mut ui,
        );
        assert!(result.unwrap_err().message().contains(expected));
        assert_eq!(
            backend.calls,
            [
                Call::Status(NativeProvider::Codex),
                Call::Login(NativeProvider::Codex, NativeLoginMode::Browser)
            ]
        );
        assert_eq!(backend.completed_login_modes, [NativeLoginMode::Browser]);
        assert_eq!(backend.credential, CredentialState::Ready);
        assert!(
            !ui.waiting
                .unwrap()
                .load(std::sync::atomic::Ordering::Relaxed)
        );
    }
}

#[test]
fn login_admission_and_input_refusals_never_complete_or_switch_routes() {
    let mut backend = FakeBackend::new(CompatibilityState::Current, CredentialState::Ready);
    backend.login_begin_failure = Some("exact compatibility acceptance is required");
    let mut ui = commands::login_flow::TestLoginUi::default();
    assert!(
        auth_login(
            &mut backend,
            NativeProvider::Codex,
            NativeLoginMode::Browser,
            &mut Cursor::new([]),
            &mut Vec::new(),
            &mut ui
        )
        .is_err()
    );
    assert_eq!(ui.presented_urls, [] as [String; 0]);
    assert_eq!(backend.completed_login_modes, [] as [NativeLoginMode; 0]);

    let mut backend = FakeBackend::new(CompatibilityState::Current, CredentialState::Ready);
    let mut ui = commands::login_flow::TestLoginUi {
        ssh: true,
        ..commands::login_flow::TestLoginUi::default()
    };
    assert!(
        auth_login(
            &mut backend,
            NativeProvider::Codex,
            NativeLoginMode::Browser,
            &mut Cursor::new([]),
            &mut Vec::new(),
            &mut ui
        )
        .is_err()
    );
    assert_eq!(backend.calls, [Call::Status(NativeProvider::Codex)]);
    assert_eq!(backend.completed_login_modes, [] as [NativeLoginMode; 0]);

    let mut backend = FakeBackend::new(CompatibilityState::Current, CredentialState::Ready);
    let mut ui = commands::login_flow::TestLoginUi::default();
    assert!(
        auth_login(
            &mut backend,
            NativeProvider::Xai,
            NativeLoginMode::Remote,
            &mut Cursor::new(b"\n"),
            &mut Vec::new(),
            &mut ui
        )
        .is_err()
    );
    assert_eq!(
        backend.calls,
        [
            Call::Status(NativeProvider::Xai),
            Call::Login(NativeProvider::Xai, NativeLoginMode::Remote)
        ]
    );
    assert_eq!(backend.completed_login_modes, [] as [NativeLoginMode; 0]);
    assert_eq!(ui.browser_urls, [] as [String; 0]);
}

#[test]
fn login_and_refresh_accept_only_ready_terminal_state() {
    let non_ready = [
        CredentialState::SignedOut,
        CredentialState::RefreshRequired,
        CredentialState::Quarantined,
        CredentialState::Revoked,
        CredentialState::Unsupported,
    ];
    for terminal in non_ready {
        for command in [
            Command::AuthLogin {
                provider: NativeProvider::Codex,
                mode: NativeLoginMode::Browser,
            },
            Command::AuthRefresh(NativeProvider::Xai),
        ] {
            let mut backend = FakeBackend::new(CompatibilityState::Current, terminal);
            let (result, stdout, _) = invoke(command, &mut backend, b"");
            let failure = result.unwrap_err();
            assert_eq!(failure.exit_code(), 3);
            assert!(
                failure.message().contains("`plak login "),
                "{}",
                failure.message()
            );
            assert_eq!(stdout, [] as [u8; 0]);
        }
    }
}

#[test]
fn remote_logout_requires_revoked_and_local_logout_accepts_safe_retirement_states() {
    for terminal in [
        CredentialState::SignedOut,
        CredentialState::Ready,
        CredentialState::RefreshRequired,
        CredentialState::Quarantined,
        CredentialState::Unsupported,
    ] {
        let mut remote = FakeBackend::new(CompatibilityState::Current, terminal);
        remote.credential = CredentialState::Ready;
        let (result, _, _) = invoke(
            Command::AuthLogout {
                provider: NativeProvider::Codex,
                local_only: false,
            },
            &mut remote,
            b"",
        );
        assert_eq!(
            result.err().map(|error| error.message().to_owned()),
            Some("remote revocation did not reach a revoked credential state".to_owned())
        );
    }

    for terminal in [
        CredentialState::Ready,
        CredentialState::RefreshRequired,
        CredentialState::Quarantined,
        CredentialState::Unsupported,
    ] {
        let mut local = FakeBackend::new(CompatibilityState::Current, terminal);
        let (result, _, _) = invoke(
            Command::AuthLogout {
                provider: NativeProvider::Xai,
                local_only: true,
            },
            &mut local,
            b"",
        );
        assert_eq!(
            result.err().map(|error| error.message().to_owned()),
            Some("local logout did not reach a safely retired local credential state".to_owned())
        );
    }

    let mut remote = FakeBackend::new(CompatibilityState::Current, CredentialState::Revoked);
    remote.credential = CredentialState::RefreshRequired;
    let (_, _, stderr) = invoke(
        Command::AuthLogout {
            provider: NativeProvider::Xai,
            local_only: false,
        },
        &mut remote,
        b"",
    );
    assert_eq!(stderr, b"grok: remote credential revoked\n");

    for (terminal, expected) in [
        (
            CredentialState::SignedOut,
            b"codex: no local credential remains; recovery state cleared; remote credential may remain active\n"
                .as_slice(),
        ),
        (
            CredentialState::Revoked,
            b"codex: local credential revoked; recovery state cleared; remote credential may remain active\n"
                .as_slice(),
        ),
    ] {
        let mut local = FakeBackend::new(CompatibilityState::Current, terminal);
        let (result, _, stderr) = invoke(
            Command::AuthLogout {
                provider: NativeProvider::Codex,
                local_only: true,
            },
            &mut local,
            b"",
        );
        assert_eq!(result, Ok(ProcessOutcome::Success));
        assert_eq!(stderr, expected);
    }
}

#[test]
fn logout_without_a_flag_retires_unusable_or_absent_credentials_locally() {
    let unusable = b"grok: local credential retired; this build cannot use it, so it was not revoked at the provider and the remote grant may remain active\n";
    let absent = b"grok: not logged in; nothing to log out\n";
    for (compatibility, credential, terminal, expected) in [
        (
            CompatibilityState::Current,
            CredentialState::Quarantined,
            CredentialState::Revoked,
            unusable.as_slice(),
        ),
        (
            CompatibilityState::Required,
            CredentialState::LoginRequired,
            CredentialState::Revoked,
            unusable.as_slice(),
        ),
        (
            CompatibilityState::Inactive,
            CredentialState::Ready,
            CredentialState::Revoked,
            unusable.as_slice(),
        ),
        (
            CompatibilityState::Required,
            CredentialState::SignedOut,
            CredentialState::SignedOut,
            absent.as_slice(),
        ),
        (
            CompatibilityState::Current,
            CredentialState::Revoked,
            CredentialState::Revoked,
            absent.as_slice(),
        ),
    ] {
        let mut backend = FakeBackend::new(compatibility, terminal);
        backend.credential = credential;
        let (result, stdout, stderr) = invoke(
            Command::AuthLogout {
                provider: NativeProvider::Xai,
                local_only: false,
            },
            &mut backend,
            b"",
        );
        assert_eq!(result, Ok(ProcessOutcome::Success), "{credential:?}");
        assert_eq!(
            backend.calls,
            vec![
                Call::Status(NativeProvider::Xai),
                Call::LogoutLocal(NativeProvider::Xai),
            ],
            "{credential:?} must not prompt or reach the provider"
        );
        assert_eq!(stdout, [] as [u8; 0]);
        assert_eq!(stderr, expected, "{credential:?}");
        assert!(!String::from_utf8_lossy(&stderr).contains("plak login"));
    }
}

#[test]
fn logout_retires_locally_when_the_provider_path_refuses_the_record() {
    // Offline status read the record as usable, but the manager refused
    // it: logout must still finish locally instead of naming itself.
    let mut backend = FakeBackend::new(CompatibilityState::Current, CredentialState::Revoked);
    backend.credential = CredentialState::Ready;
    backend.revoke_refused = true;
    let (result, stdout, stderr) = invoke(
        Command::AuthLogout {
            provider: NativeProvider::Xai,
            local_only: false,
        },
        &mut backend,
        b"",
    );
    assert_eq!(result, Ok(ProcessOutcome::Success));
    assert_eq!(
        backend.calls,
        vec![
            Call::Status(NativeProvider::Xai),
            Call::Revoke(NativeProvider::Xai),
            Call::LogoutLocal(NativeProvider::Xai),
        ]
    );
    assert_eq!(stdout, [] as [u8; 0]);
    assert_eq!(
        String::from_utf8_lossy(&stderr),
        "grok: local credential retired; this build cannot use it, so it was not revoked at the provider and the remote grant may remain active\n"
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

fn commit_tui_default_candidate(
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
                        "TUI default config write failed: {error}; session change rolled back"
                    )))
                }
                Err(rollback) => Err(CliFailure::new(format!(
                    "TUI default config write failed: {error}; session rollback failed: {rollback}; recovery required"
                ))),
            }
        }
        Err(product_config::ConfigSaveError::PublishedDurabilityUncertain(error)) => {
            *record = candidate;
            Err(CliFailure::new(format!(
                "TUI default config was published but directory sync failed: {error}; durability uncertain; recovery required"
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
