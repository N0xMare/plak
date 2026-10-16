//! Shared composition helpers for the product command workflows.
//!
//! These are the small facade-mapping, state-root and process helpers the
//! `plak` binary used to own inline. They live here so the binary keeps only
//! clap dispatch, exit-code mapping and the TUI attach.

use std::env;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use plakous::application::ApplicationExecutor;
use plakous::subscriptions::{
    CompatibilityStatus as FacadeCompatibilityStatus, NativeAuthStatus as FacadeAuthStatus,
    NativeModelVisibility, NativeProvider, NativeSubscriptionError, NativeToolMode,
};

use crate::home::paths as product_paths;
use crate::presentation::{CompatibilityState, CredentialState};
use crate::product_config;
use plakous_hosted::agent as product_agent;
use plakous_hosted::error::{CliFailure, CliFailureKind, CliResult};
use plakous_hosted::subscription_usage as product_usage;

// Must exceed the runtime's bounded model cleanup before the product reports
// shutdown as overdue. An overdue worker is still joined: detaching it would
// let process teardown interrupt recovery/persistence work.
pub const HOST_WORKER_JOIN_TIMEOUT: Duration = Duration::from_secs(30);
pub const TUI_ERROR_JOIN_TIMEOUT: Duration = Duration::from_secs(1);

/// Join a TUI host worker after the terminal is already cooked, on a path
/// where the process will report the outcome. Returns what the worker
/// returned under the product's shutdown deadline; an overdue worker is a
/// failure, not a quiet detach.
pub fn join_tui_host_worker(
    worker: thread::JoinHandle<CliResult<()>>,
    label: &str,
) -> CliResult<()> {
    join_host_worker_bounded(worker, TUI_ERROR_JOIN_TIMEOUT, label)
}

/// How long a voluntary quit may wait for the host to record a cancel and
/// signal child processes. Session release is not joined past this: a release
/// blocked in `spawn_blocking` used to hold the cooked shell on
/// [`QUIT_NOTICE`] until the operator pressed Ctrl-C.
pub const VOLUNTARY_QUIT_GRACE: Duration = Duration::from_secs(1);

/// Session-owned process-group stops are supplied at this boundary.
fn stop_background_processes_for_interactive_exit(
    _host_mailbox: &crate::surface_protocol::UserActionSender,
) {
}

/// Voluntary TUI quit after the terminal is cooked and the resume hint is
/// flushed. The host is asked to interrupt and shut down, then this process
/// exits when that work finishes or when [`VOLUNTARY_QUIT_GRACE`] ends,
/// whichever comes first. It does not print [`QUIT_NOTICE`] and it does not
/// wait out session release.
///
/// The mailbox is forgotten rather than dropped when the grace wins: dropping
/// the last sender is `Disconnected`, which the host also treats as Quit.
pub fn exit_restored_tui(
    worker: thread::JoinHandle<CliResult<()>>,
    host_mailbox: crate::surface_protocol::UserActionSender,
) -> ! {
    stop_background_processes_for_interactive_exit(&host_mailbox);
    host_mailbox.request_abandon_and_release();
    let deadline = Instant::now() + crate::tui_timing::remaining_exit_budget();
    while !worker.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    if worker.is_finished() {
        let _ = worker.join();
    } else {
        std::mem::forget(worker);
    }
    std::mem::forget(host_mailbox);
    crate::tui_timing::exiting();
    // A quit the signal watcher requested reports the signalled status, the
    // same one a CLI turn stopped by `SIGTERM` or `SIGHUP` reports.
    let code = if crate::signals::stop_reason().is_some() {
        crate::signals::SIGNALLED_EXIT_CODE
    } else {
        0
    };
    std::process::exit(code);
}

/// Product state roots under `product`, with the caller's extra telemetry
/// sink attached.
///
/// The binary owns the OTLP bridge. The composition root builds the sink once and
/// passes it to every state root it opens. This construction also captures HOME
/// once to supply the conventional product-state protection paths.
pub fn product_state_roots_from(
    product: PathBuf,
    state_home: PathBuf,
    extra_telemetry_sink: Option<Arc<dyn plakous_hosted::agent::ExtraTelemetrySink>>,
    session_records: Arc<dyn crate::store::session_records::SessionRecordStore>,
) -> CliResult<product_agent::ProductStateRoots> {
    // One canonical product home for both: `ProductStateRoots` canonicalises
    // its own copy, and the telemetry writer registry is keyed by the path the
    // factory holds. With a symlinked home the raw path would register the
    // same file on disk under a second key.
    let product = std::fs::canonicalize(&product).unwrap_or(product);
    let paths = product_paths::PlakPaths::from_home(product.clone());
    let telemetry = crate::telemetry_sink::ProductTelemetryFactory::new(paths.clone());
    let paths_for_roster = paths.clone();
    let credentials = crate::openrouter_product::ProductOpenRouterCredentials::shared(&paths);
    let platform_credentials = crate::platform_product::ProductPlatformCredentials::shared(&paths)?;
    let runtime_database = paths.runtime_database();
    let job_outputs = paths.job_outputs();
    let tree_store = crate::store::tree::ProductTreeStoreFactory::new(paths);
    let exchange_observer = crate::commands::exchange_capture::from_environment(&product)
        .map_err(|error| CliFailure::classified(CliFailureKind::Usage, error))?;
    let roots = product_agent::ProductStateRoots::new(
        product,
        runtime_database,
        state_home,
        Arc::new(telemetry),
        Arc::new(tree_store),
        Arc::new(crate::roster_input::ProductRosterSource::new(Some(
            paths_for_roster,
        ))),
        Arc::new(crate::sidecars::ProductSidecarLocator),
        credentials,
        platform_credentials,
        session_records,
    )?;
    let roots = roots
        .with_exchange_observer(exchange_observer)
        .with_job_output_root(job_outputs)
        .with_api_models(
            plakous_hosted::platform::ApiModelLists::Live,
            plakous_hosted::platform::ApiWindow::Working,
        );
    if let Some(sink) = extra_telemetry_sink {
        roots.set_extra_telemetry_sink(sink);
    }
    Ok(roots)
}

/// Product (run + TUI host) subscription gate limits; the host owns the one
/// definition shared with rebind and child spawn.
#[allow(
    clippy::result_unit_err,
    reason = "callers classify the one invalid-limits failure themselves"
)]
pub fn product_subscription_request_limits()
-> Result<plakous::subscriptions::SubscriptionRequestLimits, ()> {
    product_agent::product_subscription_request_limits().map_err(|_| ())
}

/// An unknown `plak config set` key is an operator usage error (exit 2), and the
/// valid key list is one command away.
pub fn config_set_failure(message: String) -> CliFailure {
    if message.starts_with("unknown config key") {
        return CliFailure::classified(
            CliFailureKind::Usage,
            format!("{message}; run `plak config show` for the valid keys"),
        );
    }
    CliFailure::classified(CliFailureKind::Usage, message)
}

pub const fn cli_provider_name(provider: NativeProvider) -> &'static str {
    match provider {
        NativeProvider::Codex => "codex",
        NativeProvider::Xai => "grok",
        NativeProvider::Claude => "claude",
        _ => provider.as_str(),
    }
}

pub fn run_composition(
    explicit: Option<product_config::ProductCompositionSelection>,
    configured: &product_config::DefaultBinding,
    provider: NativeProvider,
    model: &str,
) -> product_config::ProductCompositionSelection {
    match explicit {
        Some(composition) => composition,
        None => configured.composition_for_tuple(Some(cli_provider_name(provider)), Some(model)),
    }
}

pub const fn native_visibility_label(visibility: NativeModelVisibility) -> &'static str {
    match visibility {
        NativeModelVisibility::List => "list",
        NativeModelVisibility::Hide => "hide",
        NativeModelVisibility::None => "none",
    }
}

pub const fn native_tool_mode_label(mode: NativeToolMode) -> &'static str {
    match mode {
        NativeToolMode::Direct => "direct",
        NativeToolMode::CodeMode => "code_mode",
        NativeToolMode::CodeModeOnly => "code_mode_only",
    }
}

pub fn native_processing_tier(
    selection: &product_config::ProductTargetSelection,
) -> CliResult<plakous::model::ProcessingTierDisposition> {
    match selection {
        product_config::ProductTargetSelection::Native => {
            Ok(plakous::model::ProcessingTierDisposition::Standard)
        }
        product_config::ProductTargetSelection::NativePriority => {
            Ok(plakous::model::ProcessingTierDisposition::ProviderNamed(
                plakous::model::ProviderServiceTierId::new("priority")
                    .map_err(|_| CliFailure::new("priority tier identity is invalid"))?,
            ))
        }
        product_config::ProductTargetSelection::OpenRouter { .. }
        | product_config::ProductTargetSelection::Platform { .. } => Err(CliFailure::new(
            "API target has no native subscription processing tier",
        )),
    }
}

pub const CODEX_CATALOG_SNAPSHOT_MISSING: &str =
    "Codex catalog snapshot is missing; run `plak models codex`";

pub fn persist_miss_native_tool_mode(
    account: &str,
) -> Result<(Option<NativeToolMode>, bool), String> {
    if account == "codex" {
        Err(CODEX_CATALOG_SNAPSHOT_MISSING.to_owned())
    } else {
        Ok((None, false))
    }
}

pub fn application_state_home() -> Result<PathBuf, String> {
    product_paths::application_state_home()
}

/// The product configuration a turn-running face starts from. An invalid
/// file refuses the command (spec 07 §2); warnings go to `stderr` through the
/// terminal sanitizer, because they embed workspace-chosen paths.
pub fn admit_product_config(
    paths: &product_paths::PlakPaths,
    stderr: &mut dyn io::Write,
) -> CliResult<product_config::ProductConfig> {
    let config = product_config::ProductConfig::load(paths)
        .map_err(|error| CliFailure::new(format!("invalid product config: {error}")))?;
    for warning in &config.warnings {
        let _ = writeln!(
            stderr,
            "warning: {}",
            crate::cli_output::safe_terminal_block(warning)
        );
    }
    Ok(config)
}

pub fn edit_config_file(paths: &product_paths::PlakPaths) -> CliResult<()> {
    let editor = ["VISUAL", "EDITOR"]
        .into_iter()
        .filter_map(|name| env::var(name).ok())
        .find(|value| !value.trim().is_empty())
        .or_else(|| {
            use std::io::IsTerminal as _;
            (io::stdin().is_terminal() && io::stdout().is_terminal()).then(|| "vi".to_owned())
        })
        .ok_or_else(|| {
            CliFailure::classified(
                CliFailureKind::Usage,
                "config edit needs a terminal or an explicit editor; set VISUAL or EDITOR",
            )
        })?;
    product_config::init_if_missing(paths).map_err(CliFailure::from)?;
    // The operator's editor is a shell command; the config path is a separate
    // positional argument, never shell source.
    let status = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("exec {editor} \"$1\""))
        .arg("plak-config-editor")
        .arg(paths.config_file())
        .status()
        .map_err(|_| CliFailure::new("could not launch EDITOR"))?;
    if status.success() {
        Ok(())
    } else {
        Err(CliFailure::new("editor exited with failure"))
    }
}

pub const fn compatibility_state(status: FacadeCompatibilityStatus) -> CompatibilityState {
    match status {
        FacadeCompatibilityStatus::Required => CompatibilityState::Required,
        FacadeCompatibilityStatus::Current => CompatibilityState::Current,
        FacadeCompatibilityStatus::Inactive => CompatibilityState::Inactive,
        _ => CompatibilityState::Unsupported,
    }
}

pub const fn credential_state(status: FacadeAuthStatus) -> CredentialState {
    match status {
        FacadeAuthStatus::SignedOut => CredentialState::SignedOut,
        FacadeAuthStatus::Ready => CredentialState::Ready,
        FacadeAuthStatus::RefreshRequired => CredentialState::RefreshRequired,
        FacadeAuthStatus::Quarantined => CredentialState::Quarantined,
        FacadeAuthStatus::Revoked => CredentialState::Revoked,
        FacadeAuthStatus::LoginRequired => CredentialState::LoginRequired,
        _ => CredentialState::Unsupported,
    }
}

pub const fn compatibility_status_name(status: CompatibilityState) -> &'static str {
    match status {
        CompatibilityState::Required => "acceptance-required",
        CompatibilityState::Current => "current",
        CompatibilityState::Inactive => "inactive",
        CompatibilityState::Unsupported => "unsupported",
    }
}

pub const fn profile_acceptance_label(status: CompatibilityState) -> &'static str {
    match status {
        CompatibilityState::Current => "Accepted",
        CompatibilityState::Required => "Action required",
        CompatibilityState::Inactive => "Inactive",
        CompatibilityState::Unsupported => "Unsupported",
    }
}

pub const fn credential_display_label(status: CredentialState) -> &'static str {
    match status {
        CredentialState::Ready => "Ready",
        CredentialState::SignedOut => "Missing",
        CredentialState::RefreshRequired => "Refresh required",
        CredentialState::Quarantined => "Unavailable",
        CredentialState::Revoked => "Revoked",
        CredentialState::LoginRequired => "Login required",
        CredentialState::Unsupported => "Unsupported",
    }
}

/// Credential cell for `plak status`. Login-required names the repair in the
/// same words `credential_not_ready` uses; the other states keep their labels.
pub(crate) fn status_credential_cell(provider: NativeProvider, status: CredentialState) -> String {
    match status {
        CredentialState::LoginRequired => {
            format!(
                "Login required · plak login {}",
                cli_provider_name(provider)
            )
        }
        other => credential_display_label(other).to_owned(),
    }
}

pub const fn subscription_provider_label(provider: NativeProvider) -> &'static str {
    product_usage::subscription_provider_label(provider)
}

pub fn api_provider_label(account: &str) -> &'static str {
    match account {
        "anthropic" => "Anthropic API",
        "openai" => "OpenAI API",
        "openrouter" => "OpenRouter API",
        "xai-api" => "xAI API",
        _ => "API",
    }
}

pub const fn credential_status_name(status: CredentialState) -> &'static str {
    match status {
        CredentialState::SignedOut => "signed-out",
        CredentialState::Ready => "ready",
        CredentialState::RefreshRequired => "refresh-required",
        CredentialState::Quarantined => "quarantined",
        CredentialState::Revoked => "revoked",
        CredentialState::LoginRequired => "login-required",
        CredentialState::Unsupported => "unsupported",
    }
}

/// The account a remedy names: its login spelling, or a placeholder when the
/// failure is not bound to one account.
fn remedy_account(account: Option<NativeProvider>) -> &'static str {
    account.map_or("<account>", cli_provider_name)
}

/// UTC calendar date of a compiled profile timestamp.
pub(crate) fn profile_date(seconds: u64) -> String {
    i64::try_from(seconds)
        .ok()
        .and_then(|seconds| time::OffsetDateTime::from_unix_timestamp(seconds).ok())
        .map_or_else(|| seconds.to_string(), |at| at.date().to_string())
}

/// The compiled compatibility profile is no longer active: say until when it
/// was reviewed and that only a newer build repairs it.
pub(crate) fn profile_inactive_failure(account: Option<NativeProvider>) -> CliFailure {
    let detail = account.map_or_else(String::new, |provider| {
        let notice = plakous::subscriptions::NativeSubscriptions::notice(provider);
        if notice.is_killed() {
            format!(" ({} profile withdrawn)", cli_provider_name(provider))
        } else {
            format!(
                " ({} profile review expired {} UTC)",
                cli_provider_name(provider),
                profile_date(notice.expires_at())
            )
        }
    });
    CliFailure::classified(
        CliFailureKind::Auth,
        format!(
            "native compatibility profile compiled into this build is inactive{detail}; build a newer plak whose profile was reviewed again (`plak status` lists the dates); logging in again cannot renew it"
        ),
    )
}

/// A credential state that is not `Ready`, with the command that repairs it.
fn credential_not_ready(provider: NativeProvider, status: CredentialState) -> CliFailure {
    let account = cli_provider_name(provider);
    let message = match status {
        CredentialState::Ready => return CliFailure::new("credential readiness invariant failed"),
        CredentialState::SignedOut => {
            format!("{account} is signed out; run `plak login {account}`")
        }
        CredentialState::RefreshRequired => format!(
            "{account} credential needs a refresh; run `plak refresh {account}`, or `plak login {account}` if that fails"
        ),
        CredentialState::Quarantined => format!(
            "{account} credential is quarantined after an uncertain refresh; run `plak login {account}`"
        ),
        CredentialState::Revoked => {
            format!("{account} credential was revoked; run `plak login {account}`")
        }
        CredentialState::LoginRequired => format!(
            "{account} login was made by an earlier plak build and cannot be used; run `plak login {account}`"
        ),
        CredentialState::Unsupported => format!(
            "{account} credential state is not supported by this build; run `plak login {account}`"
        ),
    };
    CliFailure::classified(CliFailureKind::Auth, message)
}

/// `CredentialUnavailable` from a credential operation, named by the state
/// `status` reads offline (signed out, quarantined, revoked) when it reads one
/// that is not ready, and by the generic remedy otherwise.
pub(crate) fn credential_unavailable_failure(
    provider: NativeProvider,
    status: Option<CredentialState>,
    state_home: &Path,
) -> CliFailure {
    match status {
        Some(status) if status != CredentialState::Ready => credential_not_ready(provider, status),
        _ => native_subscription_failure(
            NativeSubscriptionError::CredentialUnavailable,
            Some(provider),
            state_home,
        ),
    }
}

/// A redaction-safe facade failure with the repair the operator can run.
///
/// Credential, compatibility and storage failures are auth failures (exit 3);
/// catalog contract, transport and internal failures keep the internal class.
pub fn native_subscription_failure(
    error: NativeSubscriptionError,
    account: Option<NativeProvider>,
    state_home: &Path,
) -> CliFailure {
    use NativeSubscriptionError as E;
    let name = remedy_account(account);
    let login = format!("run `plak login {name}`");
    let auth = |message: String| CliFailure::classified(CliFailureKind::Auth, message);
    match error {
        E::UnsafeStorage => auth(
            super::subscription_storage::find_violation(state_home, account).map_or_else(
                || format!(
                    "the subscription store refused {}; check that it and its parents are directories you own and can write",
                    state_home.display()
                ),
                |violation| format!("native subscription storage is unsafe: {violation}"),
            ),
        ),
        E::UnsupportedPlatform => auth(
            "native subscription storage is unsupported on this platform; use an API-key account with `plak key set <account>`".to_owned(),
        ),
        E::AcceptanceRequired => auth(format!(
            "the {name} compatibility notice is not accepted; {login} to review and accept it"
        )),
        E::ProfileInactive => profile_inactive_failure(account),
        E::InvalidReceipt => auth(format!(
            "native compatibility receipt is invalid; {login} to accept the notice again"
        )),
        E::CredentialProfileChanged => auth(format!(
            "the stored {name} login was written by an older build or another profile; {login}, which replaces it"
        )),
        E::CredentialUnavailable => auth(format!(
            "no ready {name} credential (signed out, quarantined or revoked); {login}"
        )),
        E::Authentication
        | E::AuthenticationBinding
        | E::AuthenticationProviderRejected
        | E::AuthenticationValidation
        | E::AuthenticationAccountChanged
        | E::CatalogCredentialRejected
        | E::CatalogAuthorizationHeaderRejected
        | E::CatalogAccountHeaderRejected
        | E::CatalogRefreshRejected => auth(format!("{error}; {login} to authorize again")),
        E::LoginTokenExchange => auth(format!(
            "native subscription token exchange or token-envelope validation failed; start a fresh login with `plak login {name}`"
        )),
        E::LoginTokenTransport => auth(format!(
            "native subscription token request did not complete; check the network, then start a fresh login with `plak login {name}`"
        )),
        E::LoginTokenRejected => auth(format!(
            "provider rejected the authorization-code token exchange; start a fresh login with `plak login {name}`"
        )),
        E::LoginTokenResponseInvalid => auth(format!(
            "provider returned an invalid token response; start a fresh login with `plak login {name}`"
        )),
        E::LoginSigningKeys => auth(format!(
            "native subscription signing-key retrieval or validation failed; check the network, then start a fresh login with `plak login {name}`"
        )),
        E::LoginAccountVerification => auth(format!(
            "native subscription signed identity or account verification failed; start a fresh login with `plak login {name}`"
        )),
        E::LoginPersistence => auth(format!(
            "native subscription login was validated but credential persistence failed; run `plak doctor` to check the state directory, then `plak login {name}`"
        )),
        E::InvalidCallback => auth(format!(
            "browser authorization input rejected; paste the full callback URL (http://127.0.0.1:<port>/callback?code=...&state=...) or the bare code from that page; then start a fresh login with `plak login {name}`"
        )),
        E::DeviceLoginUnavailable => auth(format!(
            "device-code login is not available for this account/server; remedy: run `plak login {name}` on a machine with a browser, or forward port 1455 (ssh -L 1455:localhost:1455 <host>) and run `plak login {name}` there"
        )),
        E::DeviceCodeExpired => auth(format!(
            "the device code expired before it was approved; run `plak login {name} --remote` for a new code"
        )),
        E::ModelUnavailable
        | E::ProviderConfigurationRejected
        | E::ProviderConfigurationPolicyMismatch(_)
        | E::ProviderConfigurationCompileRejected(_)
        | E::ResolutionEvidenceRejected => CliFailure::new(format!(
            "{error}; run `plak models {name}` and choose a listed model, effort and composition"
        )),
        E::CatalogValidationRejected
        | E::CatalogMediaTypeRejected
        | E::CatalogDocumentRejected
        | E::CatalogIdentifierRejected => CliFailure::new(format!(
            "{error}; the provider's catalog does not match this build's pinned contract; retry later, and if it persists use a newer plak"
        )),
        E::Transport(cause) => {
            let detail = cause.map_or_else(|| "network exchange failed".to_owned(), |cause| cause.to_string());
            let unchanged = matches!(cause, Some(cause) if cause.nothing_sent());
            CliFailure::new(format!("{name} sign-in refresh failed: {detail}; {}retry when online", if unchanged { "credential unchanged; " } else { "" }))
        },
        _ => CliFailure::new(format!(
            "{error}; retry, and run `plak doctor` if it persists"
        )),
    }
}

/// Display-only refresh evidence, without changing the credential state.
pub(crate) fn oauth_refresh_status_detail(
    paths: Option<&product_paths::PlakPaths>,
    provider: NativeProvider,
    credential: CredentialState,
) -> Option<String> {
    if !matches!(
        credential,
        CredentialState::Ready | CredentialState::RefreshRequired | CredentialState::Quarantined
    ) {
        return None;
    }
    let failure = crate::product_doctor::last_failed_refresh(
        paths?,
        provider.as_str(),
        crate::local_time_zone(),
    )?;
    Some(if credential == CredentialState::Ready {
        format!("ready; {failure}")
    } else {
        failure
    })
}

/// Credential cell when `plak status` cannot read one account. The repair is
/// the same classification as [`native_subscription_failure`]; the cell stays
/// one column so the rest of the table still lines up.
pub(crate) fn oauth_status_failure_cell(
    error: NativeSubscriptionError,
    provider: NativeProvider,
    state_home: &Path,
) -> String {
    use NativeSubscriptionError as E;
    let name = cli_provider_name(provider);
    match error {
        E::CredentialProfileChanged => format!(
            "Profile changed · {}",
            super::application::CredentialVerdict::NeedsLogin(
                super::application::LoginReason::ProfileChanged
            )
            .remedy(provider, super::application::AccountCommand::Status)
            .unwrap_or_default()
        ),
        E::CredentialUnavailable => {
            status_credential_cell(provider, CredentialState::LoginRequired)
        }
        E::UnsafeStorage => {
            unsafe_storage_status_cell(&detail_for_unsafe_storage(state_home, provider), name)
        }
        other => native_subscription_failure(other, Some(provider), state_home)
            .message()
            .replace('`', ""),
    }
}

fn detail_for_unsafe_storage(state_home: &Path, provider: NativeProvider) -> String {
    super::subscription_storage::find_violation(state_home, Some(provider)).map_or_else(
        || {
            format!(
                "the subscription store refused {}; check that it and its parents are directories you own and can write",
                state_home.display()
            )
        },
        |violation| violation.to_string(),
    )
}

/// The table cuts a cell at [`crate::cli_output::TABLE_CELL_MAX_CHARS`]. A long
/// state path would lose the chmod in the middle of the cell, so the row names
/// `plak status <account>` instead; that command prints the repair in full.
fn unsafe_storage_status_cell(detail: &str, account: &str) -> String {
    let cell = format!("Unsafe storage · {}", detail.replace('`', ""));
    if cell.chars().count() > crate::cli_output::TABLE_CELL_MAX_CHARS {
        format!("Unsafe storage · run plak status {account} for the fix")
    } else {
        cell
    }
}

/// API-key failure cell. Same backtick-free column style as the OAuth cells.
pub(crate) fn api_key_status_failure_cell(detail: &str) -> String {
    format!("Unreadable · {}", detail.replace('`', ""))
}

pub fn resume_opened_continue_tree(
    executor: &ApplicationExecutor,
    paths: &product_paths::PlakPaths,
    resume_tree: bool,
    tree_id: &str,
) -> CliResult<()> {
    if resume_tree {
        crate::topology_product::resume_topology_tree_for_cli_prompt(paths, tree_id)?;
        executor
            .block_on(product_agent::rejoin_inflight_after_resume(tree_id))
            .map_err(application_execution_failure)?;
        return Ok(());
    }
    if product_agent::topology_tree_is_restored_paused(tree_id) {
        return Err(CliFailure::new(product_agent::RESTORED_PAUSED_RESUME_HINT));
    }
    Ok(())
}

pub fn application_execution_failure<T>(_: T) -> CliFailure {
    CliFailure::new("application execution failed")
}

pub fn require_ready(provider: NativeProvider, status: CredentialState) -> CliResult<()> {
    if status == CredentialState::Ready {
        Ok(())
    } else {
        Err(credential_not_ready(provider, status))
    }
}

pub fn require_revoked(status: CredentialState, failure: &'static str) -> CliResult<()> {
    if status == CredentialState::Revoked {
        Ok(())
    } else {
        Err(CliFailure::new(failure))
    }
}

pub fn require_locally_retired(status: CredentialState, failure: &'static str) -> CliResult<()> {
    if matches!(
        status,
        CredentialState::SignedOut | CredentialState::Revoked
    ) {
        Ok(())
    } else {
        Err(CliFailure::new(failure))
    }
}

pub fn join_host_worker_bounded(
    worker: thread::JoinHandle<CliResult<()>>,
    timeout: Duration,
    label: &str,
) -> CliResult<()> {
    join_host_worker_bounded_with_notice(worker, timeout, label, None)
}

/// A quit still releasing the session after this long says so on the cooked
/// terminal; a quicker exit prints only the resume hint.
pub const QUIT_NOTICE_DELAY: Duration = Duration::from_millis(200);
pub const QUIT_NOTICE: &str = "quitting… releasing the session";

pub fn join_host_worker_bounded_with_notice(
    worker: thread::JoinHandle<CliResult<()>>,
    timeout: Duration,
    label: &str,
    mut notice: Option<&mut dyn io::Write>,
) -> CliResult<()> {
    let started = Instant::now();
    let deadline = started + timeout;
    while !worker.is_finished() {
        if started.elapsed() >= QUIT_NOTICE_DELAY
            && let Some(out) = notice.take()
        {
            let _ = writeln!(out, "{QUIT_NOTICE}");
            let _ = out.flush();
        }
        if Instant::now() >= deadline {
            // The deadline is the bound, not a label on an unbounded wait:
            // joining here waited for the worker anyway and only changed the
            // return value, so Ctrl-Q could still block for as long as the
            // worker took. Dropping the handle detaches the
            // worker and the process exits.
            drop(worker);
            return Err(CliFailure::new(format!(
                "{label} worker exceeded its {}ms shutdown deadline; cleanup is incomplete, the worker was detached, and recovery may be required",
                timeout.as_millis()
            )));
        }
        thread::sleep(Duration::from_millis(5));
    }
    worker
        .join()
        .map_err(|_| CliFailure::new(format!("{label} worker failed")))?
}
