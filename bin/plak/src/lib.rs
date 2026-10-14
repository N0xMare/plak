//! Plak product application library (CLI workflows over the public `plakous` facade).
//!
//! The `plak` binary is a thin entrypoint; product logic and clap-backed parsing live here.

#![allow(
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::needless_pass_by_value,
    clippy::match_same_arms,
    clippy::if_same_then_else,
    clippy::map_unwrap_or,
    clippy::format_push_string,
    clippy::uninlined_format_args,
    reason = "product CLI names SuperGrok/Codex; clap handlers; pedantic CLI surface"
)]
#![allow(
    clippy::must_use_candidate,
    clippy::assigning_clones,
    clippy::unnested_or_patterns,
    clippy::cast_possible_truncation,
    clippy::collapsible_if,
    clippy::struct_excessive_bools,
    clippy::format_collect,
    reason = "allowed crate-wide since this code left `plakous-hosted`; \
              tightening them is its own change"
)]
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::unwrap_used,
        clippy::unwrap_in_result,
        reason = "unit tests"
    )
)]

#[cfg(test)]
mod agent_composition_tests;
#[doc(hidden)]
pub mod agent_jsonl;
#[doc(hidden)]
pub mod agent_stdio;
#[doc(hidden)]
pub mod agents_projection;
mod build_identity;
mod carry;
mod cli;
#[path = "product_cli_output.rs"]
pub(crate) mod cli_output;
#[doc(hidden)]
pub mod commands;
#[cfg(test)]
mod composition_tests;
mod config;
mod diagnostics;
pub(crate) mod surface_protocol;
/// Terminal-only application adapters.
pub(crate) mod terminal_application;
/// Plak's own storage home: where the XDG layout puts each file, and the
/// credentials kept under it.
#[doc(hidden)]
pub mod home {
    pub mod paths;
    pub mod secrets;
    pub mod state;
    pub(crate) mod state_artifacts;
}
#[doc(hidden)]
pub mod oauth_ui;
#[cfg(test)]
mod observation_parity_tests;
#[doc(hidden)]
pub mod openrouter_product;
#[doc(hidden)]
pub mod presentation;
#[doc(hidden)]
pub mod product_config;
mod profiles;
#[doc(hidden)]
pub mod prompts;
#[doc(hidden)]
#[doc(hidden)]
pub mod roster_input;
#[doc(hidden)]
pub mod sidecars;
pub mod signals;
#[doc(hidden)]
pub mod store;
/// Product telemetry: where it is written, what emits it, and the topology
/// projection over it.
#[doc(hidden)]
pub mod telemetry {
    pub mod topology;
}
#[cfg(feature = "otel")]
mod otel_bridge;
#[cfg(all(test, unix))]
mod product_compaction_terminal_e2e_tests;
#[cfg(test)]
mod product_crash_tests;
#[doc(hidden)]
pub mod product_doctor;
mod product_onboarding;
mod product_subscription_usage;
mod product_telemetry;
#[cfg(all(test, unix))]
mod product_terminal_e2e_tests;
#[doc(hidden)]
pub mod shell;
#[doc(hidden)]
pub mod telemetry_sink;
#[doc(hidden)]
pub mod telemetry_store;
#[cfg(feature = "test-hooks")]
mod test_hooks;
#[cfg(test)]
mod test_support;
#[doc(hidden)]
pub mod topology_product;
#[cfg(test)]
mod topology_resources_projection;
mod tui;
mod tui_timing;

/// Runs the repository-only TUI rendering benchmark.
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub fn benchmark_tui_render() -> String {
    tui::benchmark::run()
}

/// Runs the repository-only product-store persistence benchmark.
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub fn benchmark_product_store(directory: Option<&Path>) -> String {
    store::benchmark::run(directory)
}

#[cfg(test)]
pub(crate) use crate::commands::host_config::HostedAgentChannels;
#[cfg(test)]
pub(crate) use crate::commands::support::product_subscription_request_limits;
pub(crate) use crate::home::paths as product_paths;
pub(crate) use crate::home::secrets as product_secrets;
#[cfg(test)]
pub(crate) use crate::presentation::PresentationBackend;
pub(crate) use crate::presentation::{CompatibilityState, CredentialState, TuiPresentationBundle};
pub(crate) use crate::store::sessions as product_sessions;
pub(crate) use plakous_hosted::agent as product_agent;
pub(crate) use plakous_hosted::error::{CliFailure, CliFailureKind, CliResult, ProcessOutcome};
pub(crate) use plakous_hosted::runtime as product_host;
pub(crate) use plakous_hosted::session_log as product_session_log;
#[cfg(test)]
pub(crate) use plakous_hosted::subscription_usage as product_usage;
pub(crate) use plakous_hosted::workspace as product_workspace;

use crate::product_config::ConfigScope;
use std::env;
use std::ffi::OsString;
use std::io::{self, IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use surface_protocol::SurfaceIntent;

use plakous::agent::ReasoningEffort;
use plakous::subscriptions::{NativeLoginMode, NativeProvider};

use crate::commands::agent::run_agent;
use crate::commands::application::{AccountOps, PlakousApplication};
use crate::commands::catalog::list_models;
use crate::commands::host_config::{
    NativeTuiAttacher, TuiAttachEligibility, TuiAttacher, build_host_config,
    deferred_attach_refusal_reason, tui_attach_eligibility,
};
use crate::commands::login::{
    auth_login, auth_logout_local, auth_logout_remote, auth_refresh, auth_status, key_clear,
    key_set, key_status, print_compatibility_notice,
};
use crate::commands::session::{
    confirm_session_mutation, load_exact_product_session, repair_invalid_new_session_binding,
    run_attached_action, seed_new_session_binding, show_child_recovery_status,
    show_recovery_status,
};
use crate::commands::support::{
    HOST_WORKER_JOIN_TIMEOUT, api_key_status_failure_cell, api_provider_label, config_set_failure,
    edit_config_file, exit_restored_tui, join_host_worker_bounded, join_tui_host_worker,
    profile_acceptance_label, status_credential_cell, subscription_provider_label,
};
use crate::commands::usage::{collect_usage_limits, usage_vertical_for_account};
use crate::shell::services::Services;

pub(crate) const NATIVE_RUN_MAX_PROMPT_BYTES: usize = 64 * 1024;
#[allow(
    dead_code,
    reason = "documented input ceiling reserved for future budget checks"
)]
const NATIVE_RUN_INPUT_UPPER_BOUND_TOTAL: u64 = 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
#[allow(
    clippy::enum_variant_names,
    reason = "CommandHelp is a recursive help subcommand name"
)]
#[cfg_attr(
    not(feature = "maintainer"),
    allow(
        dead_code,
        reason = "maintainer verbs stay on Command for execute/tests; parse cannot emit them"
    )
)]
pub(crate) enum Command {
    Help,
    CommandHelp {
        command: String,
    },
    Version,
    Tui {
        session_id: Option<String>,
        resume_picker: bool,
    },
    Resume {
        session_id: Option<String>,
    },
    Fork {
        session_id: String,
        confirmed: bool,
    },
    Compact {
        session_id: String,
        confirmed: bool,
    },
    Doctor,
    StatusAll,
    ConfigShow,
    ConfigSet {
        key: String,
        value: String,
        scope: product_config::ConfigWriteScope,
    },
    ConfigEdit,
    ConfigInit,
    /// Record the discovered project `.plak` as trusted configuration.
    ConfigTrust,
    /// Remove the discovered project `.plak` from the trust registry.
    ConfigUntrust,
    KeySet {
        account: String,
    },
    KeyClear {
        account: String,
    },
    KeyStatus {
        account: Option<String>,
    },
    Usage {
        account: Option<String>,
        session_id: Option<String>,
    },
    SessionList,
    SessionShow {
        id: String,
    },
    SessionRecover {
        id: String,
    },
    #[cfg(feature = "maintainer")]
    SessionRehearse {
        id: String,
    },
    SessionExport {
        id: String,
        out: Option<PathBuf>,
    },
    SessionImport {
        path: PathBuf,
    },
    SessionReference {
        path: PathBuf,
        search: Option<String>,
        tail: usize,
    },
    SessionDelete {
        id: String,
        yes: bool,
        abandon_children: bool,
    },
    /// `plak session gc <id>` or, with `None`, `plak session gc --all`.
    SessionGc {
        id: Option<String>,
    },
    /// Hidden `plak --import-resume <file>`: import, delete the file, resume.
    ImportResume {
        path: PathBuf,
    },
    /// Hidden `plak version --state-key`.
    StateKey,
    ReloadProbe,
    ExportVersion,
    StatePruneLegacy {
        yes: bool,
        forget_missing_launches: bool,
        prune_orphaned_references: bool,
    },
    SessionLog {
        id: String,
        kind: Option<String>,
        tail: usize,
        json: bool,
    },
    TelemetryStatus {
        views: bool,
    },
    TelemetrySql {
        query: String,
        json: bool,
    },
    TelemetryPrune {
        max_age_days: Option<u64>,
        max_bytes: Option<u64>,
    },
    CompatibilityNotice(NativeProvider),
    AuthStatus(NativeProvider),
    AuthLogin {
        provider: NativeProvider,
        mode: NativeLoginMode,
    },
    AuthRefresh(NativeProvider),
    AuthLogout {
        provider: NativeProvider,
        local_only: bool,
    },
    Models(NativeProvider),
    /// Authenticated OpenRouter catalog identifier listing.
    ModelsOpenRouter,
    ModelsPlatform(plakous_hosted::binding::PlatformProvider),
    Run {
        provider: NativeProvider,
        model: String,
        /// Reasoning effort, exactly as the operator typed `--effort` (SuperGrok/Codex
        /// portable: low|medium|high; the CLI also accepts xhigh|max|ultra and adapters
        /// fail closed when unsupported). `None` means the flag was absent; there is no
        /// silent default.
        effort: Option<ReasoningEffort>,
        composition: Option<product_config::ProductCompositionSelection>,
        prompt: String,
        print_session_id: bool,
        limits: plakous_hosted::runtime::RunBounds,
    },
    RunOpenRouter {
        model: String,
        effort: Option<ReasoningEffort>,
        composition: product_config::ProductCompositionSelection,
        wire: product_config::ProductWireBinding,
        prompt: String,
        print_session_id: bool,
        limits: plakous_hosted::runtime::RunBounds,
    },
    RunPlatform {
        provider: plakous_hosted::binding::PlatformProvider,
        model: String,
        effort: ReasoningEffort,
        composition: product_config::ProductCompositionSelection,
        prompt: String,
        print_session_id: bool,
        limits: plakous_hosted::runtime::RunBounds,
    },
    ContinueSession {
        session_id: String,
        prompt: String,
        print_session_id: bool,
        limits: plakous_hosted::runtime::RunBounds,
        resume_tree: bool,
    },
    Agent {
        session_id: Option<String>,
        provider: Option<NativeProvider>,
        platform: Option<plakous_hosted::binding::PlatformProvider>,
        wire: Option<product_config::ProductWireBinding>,
        model: Option<String>,
        effort: Option<ReasoningEffort>,
        effort_explicit: bool,
        composition: Option<product_config::ProductCompositionSelection>,
        resume_tree: bool,
    },
}

/// Operator-selected reasoning effort for subscription runs.
///
/// Dialects fail closed when a model does not admit a level (e.g. SuperGrok
/// rejects xhigh/max/ultra). Preferred capability keeps runs usable if omitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RunEffort {
    None,
    Low,
    Medium,
    High,
    XHigh,
    Max,
    Ultra,
}

/// The effort words `--effort` accepts, spelled from the one shared ladder
/// (`plakous_hosted::effort`) that templates and this flag both read. No surface
/// carries its own list.
pub(crate) fn run_effort_levels() -> String {
    plakous_hosted::effort::levels_pipe_list()
}

/// The one `--effort` usage message, in the ladder's own words.
pub(crate) fn run_effort_usage() -> String {
    format!("effort must be {}", run_effort_levels())
}

impl RunEffort {
    pub(crate) fn as_reasoning_effort(self) -> ReasoningEffort {
        match self {
            Self::Low => ReasoningEffort::Low,
            Self::Medium => ReasoningEffort::Medium,
            Self::High => ReasoningEffort::High,
            Self::XHigh => ReasoningEffort::XHigh,
            Self::Max => ReasoningEffort::Max,
            Self::Ultra => ReasoningEffort::Ultra,
            Self::None => ReasoningEffort::None,
        }
    }

    /// Named levels come from the shared ladder, so a level cannot exist in a
    /// template and be missing from `--effort`.
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match plakous_hosted::effort::parse(value)? {
            "none" => Some(Self::None),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::XHigh),
            "max" => Some(Self::Max),
            "ultra" => Some(Self::Ultra),
            _ => None,
        }
    }
}

impl Command {
    const fn diagnostic_verb(&self) -> &'static str {
        match self {
            Self::Help | Self::CommandHelp { .. } => "help",
            Self::Version | Self::StateKey | Self::ReloadProbe | Self::ExportVersion => "version",
            Self::Tui { .. } => "tui",
            Self::Resume { .. } | Self::ImportResume { .. } => "resume",
            Self::Fork { .. } => "fork",
            Self::Compact { .. } => "compact",
            Self::Doctor => "doctor",
            Self::StatusAll | Self::AuthStatus(_) => "status",
            Self::ConfigShow
            | Self::ConfigSet { .. }
            | Self::ConfigEdit
            | Self::ConfigInit
            | Self::ConfigTrust
            | Self::ConfigUntrust => "config",
            Self::KeySet { .. } | Self::KeyClear { .. } | Self::KeyStatus { .. } => "key",
            Self::Usage { .. } => "usage",
            Self::SessionList
            | Self::SessionShow { .. }
            | Self::SessionRecover { .. }
            | Self::SessionExport { .. }
            | Self::SessionImport { .. }
            | Self::SessionReference { .. }
            | Self::SessionDelete { .. }
            | Self::SessionGc { .. }
            | Self::SessionLog { .. } => "session",
            #[cfg(feature = "maintainer")]
            Self::SessionRehearse { .. } => "session",
            Self::StatePruneLegacy { .. } => "state",
            Self::TelemetryStatus { .. }
            | Self::TelemetrySql { .. }
            | Self::TelemetryPrune { .. } => "telemetry",
            Self::CompatibilityNotice(_) => "compatibility",
            Self::AuthLogin { .. } => "login",
            Self::AuthRefresh(_) => "refresh",
            Self::AuthLogout { .. } => "logout",
            Self::Models(_) | Self::ModelsOpenRouter | Self::ModelsPlatform(_) => "models",
            Self::Run { .. }
            | Self::RunOpenRouter { .. }
            | Self::RunPlatform { .. }
            | Self::ContinueSession { .. } => "run",
            Self::Agent { .. } => "agent",
        }
    }

    const fn default_failure_kind(&self) -> CliFailureKind {
        match self {
            Self::AuthStatus(_)
            | Self::AuthLogin { .. }
            | Self::AuthRefresh(_)
            | Self::AuthLogout { .. }
            | Self::Models(_)
            | Self::ModelsOpenRouter => CliFailureKind::Auth,
            _ => CliFailureKind::Internal,
        }
    }
}

/// Run the Plak CLI using process arguments from the environment.
#[must_use]
pub fn main_from_env() -> ExitCode {
    tui_timing::process_started();
    // Read the system zone once, before anything spawns a thread. Resume
    // dates and prompt send times resolve each instant's own offset in it.
    let _ = local_time_zone();
    let mut arguments = env::args_os();
    build_identity::record_launch(arguments.next().as_deref());
    run_from_args(arguments)
}

/// This process's zone, read once and reused; UTC when the platform cannot
/// say, never a wrong local time.
fn local_time_zone() -> &'static jiff::tz::TimeZone {
    static ZONE: std::sync::OnceLock<jiff::tz::TimeZone> = std::sync::OnceLock::new();
    ZONE.get_or_init(jiff::tz::TimeZone::system)
}

/// Run the Plak CLI with an explicit argument list (excluding argv0).
///
/// Exit codes (spec 02 §3.3): 0 ok, 2 usage/limit, 3 auth, 4 cancel, 5 agent, else 1.
#[must_use]
pub fn run_from_args(arguments: impl IntoIterator<Item = OsString>) -> ExitCode {
    match run(arguments) {
        Ok(ProcessOutcome::Success) => ExitCode::SUCCESS,
        Ok(ProcessOutcome::Interrupted) => ExitCode::from(4),
        Ok(ProcessOutcome::Bounded) => ExitCode::from(6),
        Err(failure) => {
            eprintln!(
                "plak: {}",
                cli_output::safe_terminal_block(failure.message())
            );
            ExitCode::from(failure.exit_code())
        }
    }
}

/// The OTLP bridge the `otel` build attaches to every product state root.
///
/// The bridge lives in this binary, so the host is told how to build one and
/// asks for it at each [`crate::commands::support::product_state_roots_from`].
///
/// `Services` holds this as a factory and calls it at the first state-root
/// open, so `plak --version` — which opens none — never spawns the exporter
/// thread and never prints `otel bridge unavailable` for a bad endpoint.
#[cfg(feature = "otel")]
fn otel_extra_telemetry_sink() -> Option<Arc<dyn plakous_hosted::agent::ExtraTelemetrySink>> {
    let endpoint = diagnostics::otlp_endpoint()?;
    match otel_bridge::OtelBridge::from_endpoint(&endpoint) {
        Ok(bridge) => Some(Arc::new(bridge)),
        Err(error) => {
            eprintln!("plak: otel bridge unavailable: {error}");
            None
        }
    }
}

fn log_start(binary: &str, command: &str) {
    if binary != "unavailable" {
        tracing::info!(
            target: "plak.start",
            binary = %binary,
            state_key = %home::state::state_key(),
            command = %command,
        );
    }
}

fn log_command_start(command: &Command) {
    match command {
        Command::StateKey | Command::ReloadProbe | Command::ExportVersion => {}
        Command::Version => log_start(build_identity::binary_id(), command.diagnostic_verb()),
        _ => {
            if let Some(binary) = build_identity::ready_binary_id() {
                log_start(binary, command.diagnostic_verb());
            } else {
                build_identity::prepare_binary_id(Some(command.diagnostic_verb()));
            }
        }
    }
}

fn run(arguments: impl IntoIterator<Item = OsString>) -> CliResult<ProcessOutcome> {
    let command = parse(arguments)
        .map_err(|message| CliFailure::classified(CliFailureKind::Usage, message))?;

    if let Command::SessionImport { path } = &command {
        carry::read_document(path).map_err(|error| CliFailure::new(error.to_string()))?;
    }

    // A face that runs a turn arms the terminating-signal watcher here, the
    // first moment the face is known. Everything below it — the composition
    // root, diagnostics, retirement staging, admission, catalog validation and
    // the store open — runs before the face itself exists, and a supervisor's
    // stop inside that window met the default disposition: the process died at
    // once, with no announcement, an empty stream and a branch head nobody
    // could resume from. The window is milliseconds when the machine is idle
    // and seconds when it is not, which is why it showed up as an intermittent
    // failure rather than a broken face. The JSONL face arms its own, with its
    // own announcement, and is routed away below.
    if runs_a_cli_turn(&command) {
        signals::install(signals::cli_announcement(), || {});
    }

    // The composition root: `~/.plak`, the XDG state home and the build's
    // extra telemetry sink are resolved exactly once here, and every command
    // borrows them. Nothing below reads the environment for a path again.
    #[cfg(feature = "otel")]
    let services = Services::from_env(Some(otel_extra_telemetry_sink));
    #[cfg(not(feature = "otel"))]
    let services = Services::from_env(None);

    if let Command::SessionShow { id } = &command {
        return show_session_read_only(services.paths()?, id, &mut io::stdout().lock());
    }
    if matches!(&command, Command::ConfigEdit) {
        edit_config_file(services.paths()?)?;
        return Ok(ProcessOutcome::Success);
    }
    #[cfg(feature = "maintainer")]
    if let Command::SessionRehearse { id } = &command {
        commands::reload_rehearsal::rehearse(services.paths()?, id, &mut io::stdout().lock())?;
        return Ok(ProcessOutcome::Success);
    }
    if let Command::SessionLog {
        id,
        kind,
        tail,
        json,
    } = &command
        && id.contains('.')
    {
        commands::session::log_child_read_only(
            services.paths()?,
            id,
            kind.as_deref(),
            *tail,
            *json,
            &mut io::stdout().lock(),
        )?;
        return Ok(ProcessOutcome::Success);
    }

    let filter = diagnostics::requested_filter();
    // Automatic maintainer capture must leave safe failure diagnostics even
    // without an operator logging override, and never write over the TUI.
    #[cfg(feature = "maintainer")]
    let filter = Some(filter.map_or_else(
        || "info,plak.capture=warn".to_owned(),
        |filter| format!("{filter},plak.capture=warn"),
    ));
    #[cfg(feature = "otel")]
    let otlp_endpoint = diagnostics::otlp_endpoint();
    #[cfg(not(feature = "otel"))]
    let otlp_endpoint: Option<String> = None;
    let wants_diagnostics =
        !command_is_read_only(&command) && (filter.is_some() || otlp_endpoint.is_some());
    let diagnostics = if wants_diagnostics {
        match services
            .home()
            .paths_if_resolved()
            .ok_or_else(|| "could not resolve the product home".to_owned())
            .and_then(|paths| {
                diagnostics::install(
                    paths,
                    filter.as_deref(),
                    #[cfg(feature = "otel")]
                    otlp_endpoint.as_deref(),
                )
                .map_err(|error| error.to_string())
            }) {
            Ok(guard) => Some(guard),
            Err(error) => {
                eprintln!("plak: diagnostics unavailable: {error}");
                None
            }
        }
    } else {
        None
    };

    if diagnostics.is_some() {
        log_command_start(&command);
    }

    // The TUI owns the process terminal and must run before acquiring Rust's
    // global standard-stream locks. Keeping those locks across `launch_tui`
    // deadlocks when terminal setup or state admission uses stdout/stderr.
    let command = match command {
        Command::Tui {
            session_id,
            resume_picker,
        } => return launch_tui(&services, session_id, resume_picker, None),
        Command::Resume { session_id: None } => return launch_tui(&services, None, true, None),
        Command::Resume {
            session_id: Some(id),
        } => return launch_tui(&services, Some(id), false, None),
        Command::ImportResume { path } => return import_and_resume(&services, &path),
        command @ Command::Agent { .. } => return run_agent_command(&services, command),
        command => command,
    };

    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();
    if command_needs_product_state(&command) {
        home::state::ensure_usable_state(services.paths()?, &mut stderr)
            .map_err(CliFailure::new)?;
    }
    match command {
        Command::Help => {
            cli::render::global_help(&mut stdout).map_err(|_| CliFailure::new("stdout failed"))?;
            Ok(ProcessOutcome::Success)
        }
        Command::CommandHelp { command } => {
            cli::render::command_help(&command, &mut stdout)
                .map_err(|_| CliFailure::new("stdout failed"))?;
            Ok(ProcessOutcome::Success)
        }
        Command::Version => {
            writeln!(stdout, "plak {}", build_identity::version_label())
                .map_err(|_| CliFailure::new("stdout failed"))?;
            Ok(ProcessOutcome::Success)
        }
        Command::ReloadProbe => {
            let verdict = match services.paths() {
                Ok(paths) => carry::reload_probe(paths),
                Err(error) => carry::ReloadProbe {
                    state_key: home::state::state_key(),
                    export_versions: carry::IMPORTABLE_VERSIONS.to_vec(),
                    config_error: Some(error.message().to_owned()),
                },
            };
            serde_json::to_writer(&mut stdout, &verdict)
                .map_err(|_| CliFailure::new("stdout failed"))?;
            writeln!(stdout).map_err(|_| CliFailure::new("stdout failed"))?;
            Ok(ProcessOutcome::Success)
        }
        Command::ExportVersion => {
            writeln!(stdout, "{}", carry::VERSION).map_err(|_| CliFailure::new("stdout failed"))?;
            Ok(ProcessOutcome::Success)
        }
        Command::StateKey => {
            writeln!(stdout, "{}", home::state::state_key())
                .map_err(|_| CliFailure::new("stdout failed"))?;
            Ok(ProcessOutcome::Success)
        }
        Command::CompatibilityNotice(provider) => {
            print_compatibility_notice(provider, &mut stdout)?;
            Ok(ProcessOutcome::Success)
        }
        Command::TelemetryStatus { views } => product_telemetry::run_telemetry(
            services.paths()?,
            product_telemetry::TelemetryCommand::Status { views },
            &mut stdout,
        ),
        Command::TelemetrySql { query, json } => product_telemetry::run_telemetry(
            services.paths()?,
            product_telemetry::TelemetryCommand::Sql { query, json },
            &mut stdout,
        ),
        Command::TelemetryPrune {
            max_age_days,
            max_bytes,
        } => product_telemetry::run_telemetry(
            services.paths()?,
            product_telemetry::TelemetryCommand::Prune {
                max_age_days,
                max_bytes,
            },
            &mut stdout,
        ),
        Command::Doctor => run_doctor(&services, &mut stdout),
        Command::StatePruneLegacy {
            yes,
            forget_missing_launches,
            prune_orphaned_references,
        } => {
            let terminal = rustix::termios::isatty(io::stdin());
            if !yes && !terminal {
                return Err(CliFailure::classified(
                    CliFailureKind::Usage,
                    "state prune-legacy requires interactive confirmation or explicit --yes",
                ));
            }
            let inventory = home::state::confirm_prune_legacy_options(
                services.paths()?,
                yes,
                home::state::PruneOptions {
                    forget_missing_launches,
                    prune_orphaned_references,
                },
                terminal,
                &mut stdin,
                &mut stderr,
            )
            .map_err(CliFailure::new)?;
            let count = home::state::prune_confirmed_with_report(
                services.paths()?,
                &inventory,
                &mut stderr,
            )
            .map_err(CliFailure::new)?;
            writeln!(stdout, "pruned {} non-current state entries; removed {} owned artifact groups; forgot {} missing-launch ownership groups; kept {} state entries and {} owned artifact groups",
                count.state_entries, count.artifact_groups, count.forgotten_groups, count.kept_entries, count.kept_groups)
                .map_err(|_| CliFailure::new("stdout failed"))?;
            Ok(ProcessOutcome::Success)
        }
        command => {
            let failure_kind = command.default_failure_kind();
            let read_only = command_is_read_only(&command);
            let mut backend = PlakousApplication::new(
                services.home().state()?.to_path_buf(),
                if read_only {
                    None
                } else {
                    services.extra_telemetry_sink()
                },
                if read_only {
                    None
                } else {
                    services
                        .paths()
                        .ok()
                        .cloned()
                        .zip(services.session_records().ok())
                },
            )?;
            execute_application_command(
                command,
                &services,
                &mut backend,
                &mut stdin,
                &mut stdout,
                &mut stderr,
            )
            .map_err(|failure| failure.or_kind(failure_kind))
        }
    }
}

fn show_session_read_only(
    paths: &product_paths::PlakPaths,
    id: &str,
    stdout: &mut dyn io::Write,
) -> CliResult<ProcessOutcome> {
    let (id, child) = commands::session::parse_read_address(paths, id)?;
    let id = id.as_str();
    let s = store::ProductStore::inspect_session_read_only(paths, id).map_err(CliFailure::from)?;
    if let Some(child) = child {
        commands::session::show_selected_child(paths, &s, child, stdout)?;
        return Ok(ProcessOutcome::Success);
    }
    let n = product_session_log::count_events_read_only(&paths.session_home(), id)
        .map_or_else(|_| "unavailable".to_owned(), |count| count.to_string());
    let safe = |value: &str| cli_output::safe_terminal_text(value, 512);
    writeln!(stdout, "Session {}", safe(&s.id)).map_err(|_| CliFailure::new("stdout failed"))?;
    writeln!(stdout, "\nWorkspace")
        .and_then(|()| {
            writeln!(
                stdout,
                "  Path          {}",
                safe(&s.workspace_root.display().to_string())
            )
        })
        .and_then(|()| {
            writeln!(
                stdout,
                "  Created       {}",
                format_unix_seconds(s.created_at_unix)
            )
        })
        .and_then(|()| {
            writeln!(
                stdout,
                "  Updated       {}",
                format_unix_seconds(s.updated_at_unix)
            )
        })
        .map_err(|_| CliFailure::new("stdout failed"))?;
    if let Some(link) = carry::carried_forward_label(&s) {
        writeln!(stdout, "  Lineage       {}", safe(&link))
            .map_err(|_| CliFailure::new("stdout failed"))?;
    }
    writeln!(stdout, "\nBinding")
        .and_then(|()| {
            writeln!(
                stdout,
                "  Provider      {}",
                safe(&product_account_label(s.account.as_deref()))
            )
        })
        .and_then(|()| {
            writeln!(
                stdout,
                "  Model         {}",
                safe(s.model.as_deref().unwrap_or("—"))
            )
        })
        .and_then(|()| {
            writeln!(
                stdout,
                "  Effort        {}",
                safe(s.effort.as_deref().unwrap_or("—"))
            )
        })
        .and_then(|()| {
            // `plakous-default` resolves against the authenticated
            // catalog at attach; this offline view has no
            // catalog row, so it must not print a resolved label — a
            // Code Mode session read "Plakous default · Direct" here.
            if s.composition == product_config::ProductCompositionSelection::FollowCatalog {
                writeln!(
                    stdout,
                    "  Composition   Plakous default · resolved against the catalog at attach"
                )
            } else {
                writeln!(
                    stdout,
                    "  Composition   {} · {}",
                    title_case_token(s.composition.as_str()),
                    title_case_token(product_agent::effective_presentation_label(
                        s.composition,
                        s.account
                            .as_deref()
                            .and_then(product_host::oauth_provider_for_account),
                        None,
                    ))
                )
            }
        })
        .map_err(|_| CliFailure::new("stdout failed"))?;
    writeln!(stdout, "\nState")
        .and_then(|()| writeln!(stdout, "  Transcript    {} entries", s.transcript.len()))
        .and_then(|()| writeln!(stdout, "  Pending       {}", s.pending_queue.len()))
        .and_then(|()| writeln!(stdout, "  Events        {n}"))
        .and_then(|()| {
            writeln!(
                stdout,
                "  Event log     {}",
                safe(&paths.session_events_file(id).display().to_string())
            )
        })
        .map_err(|_| CliFailure::new("stdout failed"))?;
    let root_inspection = show_recovery_status(paths, &s, stdout);
    if let Err(error) = &root_inspection {
        writeln!(
            stdout,
            "  Root inspection incomplete: {}",
            safe(error.message())
        )
        .map_err(|_| CliFailure::new("stdout failed"))?;
    }
    let child_inspection = show_child_recovery_status(paths, &s, stdout);
    if let Err(error) = &child_inspection {
        writeln!(
            stdout,
            "  Child inspection incomplete: {}",
            safe(error.message())
        )
        .map_err(|_| CliFailure::new("stdout failed"))?;
    }
    if !s.transcript.is_empty() {
        writeln!(stdout, "\nRecent conversation").map_err(|_| CliFailure::new("stdout failed"))?;
        for line in s
            .transcript
            .iter()
            .rev()
            .take(8)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            let Some((role, text)) = transcript_entry(line) else {
                continue;
            };
            let role = safe(role);
            let text = safe(text);
            writeln!(stdout, "  {role}\n    {text}")
                .map_err(|_| CliFailure::new("stdout failed"))?;
        }
    }
    root_inspection?;
    child_inspection?;
    Ok(ProcessOutcome::Success)
}

fn command_needs_product_state(command: &Command) -> bool {
    matches!(
        command,
        Command::Run { .. }
            | Command::RunOpenRouter { .. }
            | Command::RunPlatform { .. }
            | Command::ContinueSession { .. }
            | Command::Agent { .. }
            | Command::SessionList
            | Command::SessionRecover { .. }
            | Command::SessionExport { .. }
            | Command::SessionImport { .. }
            | Command::SessionDelete { .. }
            | Command::SessionGc { .. }
            | Command::Fork { .. }
            | Command::Compact { .. }
            | Command::Usage {
                session_id: Some(_),
                ..
            }
    )
}

fn command_is_read_only(command: &Command) -> bool {
    matches!(
        command,
        Command::StatusAll
            | Command::AuthStatus(_)
            | Command::Doctor
            | Command::KeyStatus { .. }
            | Command::ConfigShow
            | Command::SessionShow { .. }
            | Command::SessionLog { .. }
            | Command::SessionReference { .. }
    )
}

/// Whether this command runs a model turn on the CLI face.
///
/// Only these arms reach `commands::run::run_config`, and only they leave a
/// resumable session behind, so only they need the watcher. The JSONL face
/// installs its own, with its own announcement, at the top of `run_agent`.
fn runs_a_cli_turn(command: &Command) -> bool {
    matches!(
        command,
        Command::Run { .. }
            | Command::RunOpenRouter { .. }
            | Command::RunPlatform { .. }
            | Command::ContinueSession { .. }
    )
}

fn execute_application_command(
    command: Command,
    services: &Services,
    backend: &mut PlakousApplication,
    stdin: &mut dyn io::BufRead,
    stdout: &mut dyn io::Write,
    stderr: &mut dyn io::Write,
) -> CliResult<ProcessOutcome> {
    // Every turn-running arm admits `config.toml` first, before the
    // compatibility prompt, a credential read or any session state.
    let admit = |stderr: &mut dyn io::Write| {
        commands::support::admit_product_config(services.paths()?, stderr)
    };
    match command {
        Command::Run {
            limits,
            provider,
            model,
            effort,
            composition,
            prompt,
            print_session_id,
        } => {
            let config = admit(stderr)?;
            #[cfg(feature = "test-hooks")]
            let injected = test_hooks::selected().is_some();
            #[cfg(not(feature = "test-hooks"))]
            let injected = false;
            if !injected {
                let status = AccountOps::offline_status(backend, provider)?;
                if status.credential != CredentialState::RefreshRequired {
                    commands::support::require_ready(provider, status.credential)?;
                }
            }
            commands::login::ensure_compatibility(backend, provider, stdin, stderr)?;
            backend.run(
                &config,
                provider,
                &model,
                effort,
                composition,
                prompt,
                print_session_id,
                stdout,
                stderr,
                limits,
            )
        }
        Command::RunOpenRouter {
            limits,
            model,
            effort,
            composition,
            wire,
            prompt,
            print_session_id,
        } => backend.run_api(
            &admit(stderr)?,
            &model,
            effort,
            composition,
            product_config::ProductTargetSelection::OpenRouter {
                wire,
                routing: product_config::ProductOpenRouterRoutingSelection::GatewayManaged,
            },
            prompt,
            print_session_id,
            stdout,
            stderr,
            limits,
        ),
        Command::RunPlatform {
            limits,
            provider,
            model,
            effort,
            composition,
            prompt,
            print_session_id,
        } => backend.run_api(
            &admit(stderr)?,
            &model,
            Some(effort),
            composition,
            product_config::ProductTargetSelection::Platform { provider },
            prompt,
            print_session_id,
            stdout,
            stderr,
            limits,
        ),
        Command::ContinueSession {
            limits,
            session_id,
            prompt,
            print_session_id,
            resume_tree,
        } => backend.continue_session(
            &admit(stderr)?,
            &session_id,
            prompt,
            print_session_id,
            resume_tree,
            stdout,
            stderr,
            limits,
        ),
        command => execute_command(
            command,
            services,
            backend,
            stdin,
            stdout,
            stderr,
            &mut commands::login_flow::SystemLoginUi,
        ),
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one exhaustive command dispatcher keeps every native command workflow visible"
)]
fn execute_command(
    command: Command,
    services: &Services,
    backend: &mut impl AccountOps,
    stdin: &mut dyn io::BufRead,
    stdout: &mut dyn io::Write,
    stderr: &mut dyn io::Write,
    login_ui: &mut impl commands::login_flow::LoginUi,
) -> CliResult<ProcessOutcome> {
    match command {
        Command::AuthStatus(provider) => {
            auth_status(backend, provider, services.paths().ok(), stdout)
        }
        Command::AuthLogin { provider, mode } => {
            auth_login(backend, provider, mode, stdin, stderr, login_ui)
        }
        Command::AuthRefresh(provider) => auth_refresh(backend, provider, stdin, stderr),
        Command::AuthLogout {
            provider,
            local_only: true,
        } => auth_logout_local(backend, provider, stderr),
        Command::AuthLogout {
            provider,
            local_only: false,
        } => auth_logout_remote(backend, provider, stdin, stderr),
        Command::Models(provider) => list_models(backend, provider, stdin, stdout, stderr),
        Command::ModelsOpenRouter => commands::catalog::list_openrouter_models(backend, stdout),
        Command::ModelsPlatform(provider) => {
            let catalog = backend.platform_catalog(provider)?;
            commands::catalog::list_platform_models(&catalog, stdout)
        }
        Command::ConfigShow => {
            let paths = services.paths()?;
            let cfg = product_config::ProductConfig::load(paths).map_err(CliFailure::from)?;
            write!(stdout, "{}", cfg.display_redacted(paths))
                .map_err(|_| CliFailure::new("stdout failed"))?;
            Ok(ProcessOutcome::Success)
        }
        Command::ConfigInit => {
            let paths = services.paths()?;
            let created = product_config::init_if_missing(paths).map_err(CliFailure::from)?;
            writeln!(
                stdout,
                "{} {}",
                if created {
                    "Created"
                } else {
                    "Already exists:"
                },
                paths.config_file().display()
            )
            .map_err(|_| CliFailure::new("stdout failed"))?;
            Ok(ProcessOutcome::Success)
        }
        Command::ConfigSet { key, value, scope } => {
            let paths = services.paths()?;
            let mut cfg = product_config::ProductConfig::load(paths).map_err(CliFailure::from)?;
            let field = product_config::config_key(&key).map(|row| row.field);
            if scope == product_config::ConfigWriteScope::Project
                && field.is_some_and(product_config::ConfigField::is_home_only)
            {
                return Err(CliFailure::new(format!(
                    "{key} is home-scoped; use --scope user to change orchestration authority"
                )));
            }
            if scope == product_config::ConfigWriteScope::User
                && let Some(field) = field
                && cfg.origin(field) == product_config::ConfigValueOrigin::Project
            {
                return Err(CliFailure::new(format!(
                    "{key} is set by the project config {}; use --scope project to change it there",
                    cfg.project
                        .as_ref()
                        .map(|source| source.root.join(".plak/config.toml").display().to_string())
                        .unwrap_or_else(|| ".plak/config.toml".to_owned())
                )));
            }
            cfg.set_key(&key, &value).map_err(config_set_failure)?;
            if scope == product_config::ConfigWriteScope::Project
                && let Some(field) = field
            {
                cfg.set_origin(field, product_config::ConfigValueOrigin::Project);
            }
            if config_key_changes_orchestration(&key) {
                // a roster or orchestration change is accepted only
                // from a quiescent tree. A headless `plak config set` names no
                // session, so every persisted tree must be quiescent; the file
                // is then saved and a new session reads it at genesis.
                let outcome = topology_product::apply_roster(paths, None, &cfg, scope)
                    .map_err(|error| CliFailure::new(error.into_message()))?;
                let disposition = match outcome {
                    topology_product::RosterApplyOutcome::Saved => {
                        "saved; applies at the next session genesis"
                    }
                    topology_product::RosterApplyOutcome::Applied
                    | topology_product::RosterApplyOutcome::AppliedDurabilityUncertain(_) => {
                        "applied"
                    }
                };
                writeln!(stdout, "set {key} · {disposition}")
                    .map_err(|_| CliFailure::new("stdout failed"))?;
                return Ok(ProcessOutcome::Success);
            }
            cfg.save_scoped(paths, scope)
                .map_err(|error| CliFailure::new(error.into_message()))?;
            let scope_note = if scope == product_config::ConfigWriteScope::Project {
                " · project"
            } else {
                ""
            };
            writeln!(stdout, "set {key}{scope_note}")
                .map_err(|_| CliFailure::new("stdout failed"))?;
            Ok(ProcessOutcome::Success)
        }
        Command::ConfigTrust => {
            let paths = services.paths()?;
            let Some(root) = paths.project_root() else {
                return Err(CliFailure::new(
                    "no project .plak was discovered from the current directory; create .plak/config.toml or .plak/agents/ first",
                ));
            };
            let recorded =
                product_config::project_trust::trust(paths, root).map_err(CliFailure::from)?;
            writeln!(
                stdout,
                "trusted {} · its .plak/config.toml and .plak/agents now apply",
                recorded.display()
            )
            .map_err(|_| CliFailure::new("stdout failed"))?;
            Ok(ProcessOutcome::Success)
        }
        Command::ConfigUntrust => {
            let paths = services.paths()?;
            let Some(root) = paths.project_root() else {
                return Err(CliFailure::new(
                    "no project .plak was discovered from the current directory; create .plak/config.toml or .plak/agents/ first",
                ));
            };
            let removed =
                product_config::project_trust::revoke(paths, root).map_err(CliFailure::from)?;
            writeln!(
                stdout,
                "{}",
                if removed {
                    "project trust revoked; its .plak configuration no longer applies"
                } else {
                    "project was not trusted"
                }
            )
            .map_err(|_| CliFailure::new("stdout failed"))?;
            Ok(ProcessOutcome::Success)
        }
        Command::KeySet { account } => key_set(&account, stdin, stderr),
        Command::KeyClear { account } => key_clear(&account, stdout),
        Command::KeyStatus { account } => key_status(account, stdout),
        Command::StatusAll => {
            struct StatusRow {
                ready: bool,
                provider: &'static str,
                kind: &'static str,
                profile: &'static str,
                credential: String,
            }

            let mut rows = Vec::with_capacity(7);
            for provider in [
                NativeProvider::Codex,
                NativeProvider::Xai,
                NativeProvider::Claude,
            ] {
                // One account's unreadable credential is a Setup needed row.
                // The other verticals, including API keys, still print.
                match backend.offline_status_row(provider) {
                    Ok(status) => rows.push(StatusRow {
                        ready: status.compatibility == CompatibilityState::Current
                            && status.credential == CredentialState::Ready,
                        provider: subscription_provider_label(provider),
                        kind: "OAuth",
                        profile: profile_acceptance_label(status.compatibility),
                        credential: commands::support::oauth_refresh_status_detail(
                            services.paths().ok(),
                            provider,
                            status.credential,
                        )
                        .unwrap_or_else(|| status_credential_cell(provider, status.credential)),
                    }),
                    Err(credential) => rows.push(StatusRow {
                        ready: false,
                        provider: subscription_provider_label(provider),
                        kind: "OAuth",
                        profile: "—",
                        credential,
                    }),
                }
            }
            let api_accounts = ["openai", "xai-api", "anthropic", "openrouter"];
            match services.paths() {
                Ok(paths) => {
                    for a in api_accounts {
                        match product_secrets::key_status(paths, a) {
                            Ok(status) => {
                                let credential = match status.source {
                                    product_secrets::KeySource::Env => "Environment",
                                    product_secrets::KeySource::File => "Key file",
                                    product_secrets::KeySource::Missing => "Missing",
                                };
                                rows.push(StatusRow {
                                    ready: status.source != product_secrets::KeySource::Missing,
                                    provider: api_provider_label(a),
                                    kind: "API key",
                                    profile: "—",
                                    credential: credential.to_owned(),
                                });
                            }
                            Err(error) => rows.push(StatusRow {
                                ready: false,
                                provider: api_provider_label(a),
                                kind: "API key",
                                profile: "—",
                                credential: api_key_status_failure_cell(&error),
                            }),
                        }
                    }
                }
                Err(error) => {
                    for a in api_accounts {
                        rows.push(StatusRow {
                            ready: false,
                            provider: api_provider_label(a),
                            kind: "API key",
                            profile: "—",
                            credential: api_key_status_failure_cell(error.message()),
                        });
                    }
                }
            }
            rows.sort_by_key(|row| row.provider.to_ascii_lowercase());

            let output = cli_human_output();
            for (heading, ready) in [("Ready", true), ("Setup needed", false)] {
                if !rows.iter().any(|row| row.ready == ready) {
                    continue;
                }
                writeln!(stdout, "{heading}").map_err(|_| CliFailure::new("stdout failed"))?;
                writeln!(stdout).map_err(|_| CliFailure::new("stdout failed"))?;
                let mut table =
                    cli_output::Table::new(["PROVIDER", "KIND", "PROFILE", "CREDENTIAL"]);
                for row in rows.iter().filter(|row| row.ready == ready) {
                    table
                        .push([row.provider, row.kind, row.profile, row.credential.as_str()])
                        .map_err(CliFailure::new)?;
                }
                output
                    .write_table(stdout, &table)
                    .map_err(|_| CliFailure::new("stdout failed"))?;
                writeln!(stdout).map_err(|_| CliFailure::new("stdout failed"))?;
            }
            writeln!(stdout, "Subscription compatibility profiles")
                .map_err(|_| CliFailure::new("stdout failed"))?;
            let mut deadlines = cli_output::Table::new(["PROVIDER", "REVIEW VALID UNTIL (UTC)"]);
            for provider in [
                NativeProvider::Codex,
                NativeProvider::Xai,
                NativeProvider::Claude,
            ] {
                let deadline = format_unix_seconds(
                    plakous::subscriptions::NativeSubscriptions::notice(provider).expires_at(),
                );
                deadlines
                    .push([subscription_provider_label(provider), deadline.as_str()])
                    .map_err(CliFailure::new)?;
            }
            output
                .write_table(stdout, &deadlines)
                .map_err(|_| CliFailure::new("stdout failed"))?;
            writeln!(stdout, "An expired profile requires a newly reviewed build; logging in again does not renew it.\n")
                .map_err(|_| CliFailure::new("stdout failed"))?;
            // An emptied PLAK_HOME still reports OAuth accounts Ready, because
            // the subscription store is XDG state and never lived in the home.
            // Saying where each kind of credential actually is turns that from
            // a surprise into a fact the reader can act on.
            writeln!(stdout, "{}", credential_locations_line(services))
                .map_err(|_| CliFailure::new("stdout failed"))?;
            writeln!(
                stdout,
                "Local credential state only; no provider request was made."
            )
            .map_err(|_| CliFailure::new("stdout failed"))?;
            Ok(ProcessOutcome::Success)
        }
        Command::SessionList => {
            let paths = services.paths()?;
            let cwd = env::current_dir().map_err(|_| CliFailure::new("cwd unavailable"))?;
            let sessions = services
                .session_records()?
                .list_for_workspace(&cwd)
                .map_err(CliFailure::from)?;
            if sessions.is_empty() {
                writeln!(stdout, "No sessions in this workspace.")
                    .map_err(|_| CliFailure::new("stdout failed"))?;
            } else {
                writeln!(stdout, "Sessions in {}", cwd.display())
                    .map_err(|_| CliFailure::new("stdout failed"))?;
                writeln!(stdout).map_err(|_| CliFailure::new("stdout failed"))?;
                let mut table = cli_output::Table::new([
                    "SESSION ID",
                    "UPDATED",
                    "PROVIDER",
                    "MODEL",
                    "EVENTS",
                    "TITLE",
                ]);
                for s in sessions {
                    let n = product_session_log::count_events(&paths.session_home(), &s.id)
                        .map_or_else(|_| "—".to_owned(), |count| count.to_string());
                    let preview = match carry::carried_forward_label(&s) {
                        Some(link) => format!("{} [{link}]", session_preview_line(&s)),
                        None => session_preview_line(&s),
                    };
                    table
                        .push([
                            s.id,
                            format_unix_seconds(s.updated_at_unix),
                            product_account_label(s.account.as_deref()),
                            s.model.unwrap_or_else(|| "—".to_owned()),
                            n,
                            preview,
                        ])
                        .map_err(CliFailure::new)?;
                }
                cli_human_output()
                    .write_table(stdout, &table)
                    .map_err(|_| CliFailure::new("stdout failed"))?;
            }
            Ok(ProcessOutcome::Success)
        }
        Command::SessionShow { id } => show_session_read_only(services.paths()?, &id, stdout),
        #[cfg(feature = "maintainer")]
        Command::SessionRehearse { id } => {
            commands::reload_rehearsal::rehearse(services.paths()?, &id, stdout)?;
            Ok(ProcessOutcome::Success)
        }
        Command::SessionRecover { id } => {
            let record = load_exact_product_session(services.session_records()?.as_ref(), &id)?;
            // Opening the session is what recovers it; the explicit action then
            // reports it, or that there was nothing to recover.
            run_attached_action(
                services,
                record,
                surface_protocol::UserAction::Recover,
                stdout,
            )
            .map_err(|failure| {
                if failure.is_recovery_required() {
                    failure
                } else {
                    CliFailure::recovery_required(failure.message().to_owned())
                }
            })?;
            Ok(ProcessOutcome::Success)
        }
        Command::SessionExport { id, out } => {
            commands::session::export_session(services, &id, out, stdout)?;
            Ok(ProcessOutcome::Success)
        }
        Command::SessionImport { path } => {
            let (record, note) = commands::session::import_session(services, &path)?;
            let note =
                cli_output::terminal_block(&note, cli_output::TerminalMode::of_stdout(), None);
            writeln!(stdout, "{note}")
                .and_then(|()| writeln!(stdout, "resume with: plak --resume {}", record.id))
                .map_err(|_| CliFailure::new("stdout failed"))?;
            Ok(ProcessOutcome::Success)
        }
        Command::SessionReference { path, search, tail } => {
            let destination = cli_output::TerminalMode::of_stdout();
            let rendered = if path.exists() {
                carry::read_document(&path)
                    .map_err(|error| CliFailure::new(error.to_string()))?
                    .reference_text(search.as_deref(), tail, destination)
            } else {
                let session_id = path.to_str().ok_or_else(|| {
                    CliFailure::new("session reference requires a session id or export path")
                })?;
                let session_id = commands::session::resolve_read_only_session_id(
                    services.paths()?,
                    session_id,
                )
                .map_err(|failure| {
                    if failure.exit_code() != 2 {
                        return failure;
                    }
                    CliFailure::classified(
                        CliFailureKind::Usage,
                        format!(
                            "`{session_id}` is neither an existing export file nor a session in this workspace"
                        ),
                    )
                })?;
                carry::read_retained_archive(
                    services.paths()?,
                    &session_id,
                    search.as_deref(),
                    tail,
                    destination,
                )
                .map_err(CliFailure::new)?
            };
            write!(stdout, "{rendered}").map_err(|_| CliFailure::new("stdout failed"))?;
            Ok(ProcessOutcome::Success)
        }
        Command::SessionDelete {
            id,
            yes,
            abandon_children,
        } => {
            commands::session::delete_session(
                services,
                &id,
                yes,
                abandon_children,
                stdin,
                stderr,
                stdout,
            )?;
            Ok(ProcessOutcome::Success)
        }
        Command::SessionGc { id } => {
            commands::session::gc_sessions(services, id.as_deref(), stdout)?;
            Ok(ProcessOutcome::Success)
        }
        Command::SessionLog {
            id,
            kind,
            tail,
            json,
        } => {
            let paths = services.paths()?;
            if id.contains('.') {
                commands::session::log_child_read_only(
                    paths,
                    &id,
                    kind.as_deref(),
                    tail,
                    json,
                    stdout,
                )?;
                return Ok(ProcessOutcome::Success);
            }
            let id = commands::session::resolve_read_only_session_id(paths, &id)?;
            let _ = store::ProductStore::inspect_session_read_only(paths, &id)
                .map_err(CliFailure::from)?;
            let lines = product_session_log::read_tail_for_kind(
                &paths.session_home(),
                &id,
                tail,
                kind.as_deref(),
            )
            .map_err(CliFailure::from)?;
            if json {
                for line in lines {
                    writeln!(stdout, "{line}").map_err(|_| CliFailure::new("stdout failed"))?;
                }
            } else if lines.is_empty() {
                writeln!(stdout, "No observational events recorded for this session.")
                    .map_err(|_| CliFailure::new("stdout failed"))?;
            } else {
                let mut table = cli_output::Table::new(["TIME", "SEQ", "KIND", "DETAILS"]);
                for line in &lines {
                    let value: serde_json::Value = serde_json::from_str(line)
                        .map_err(|_| CliFailure::new("session event is invalid"))?;
                    table
                        .push(event_table_row(&value))
                        .map_err(CliFailure::new)?;
                }
                cli_human_output()
                    .write_table(stdout, &table)
                    .map_err(|_| CliFailure::new("stdout failed"))?;
                writeln!(
                    stdout,
                    "\nEvents are observational and are not recovery authority."
                )
                .map_err(|_| CliFailure::new("stdout failed"))?;
            }
            Ok(ProcessOutcome::Success)
        }
        Command::Fork {
            session_id,
            confirmed,
        } => {
            confirm_session_mutation("fork", &session_id, confirmed, stdin, stderr)?;
            let record =
                load_exact_product_session(services.session_records()?.as_ref(), &session_id)?;
            run_attached_action(
                services,
                record,
                surface_protocol::UserAction::ForkSession {
                    account: None,
                    compact: false,
                },
                stdout,
            )?;
            Ok(ProcessOutcome::Success)
        }
        Command::Compact {
            session_id,
            confirmed,
        } => {
            confirm_session_mutation("compact", &session_id, confirmed, stdin, stderr)?;
            let record =
                load_exact_product_session(services.session_records()?.as_ref(), &session_id)?;
            run_attached_action(
                services,
                record,
                surface_protocol::UserAction::Compact,
                stdout,
            )?;
            Ok(ProcessOutcome::Success)
        }
        Command::Usage {
            account,
            session_id,
        } => {
            if let Some(session_id) = session_id {
                let record =
                    load_exact_product_session(services.session_records()?.as_ref(), &session_id)?;
                let account = record.account.clone();
                run_attached_action(
                    services,
                    record,
                    surface_protocol::UserAction::Extension(SurfaceIntent::OpenUsage),
                    stdout,
                )?;
                write_selected_subscription_usage(
                    account.as_deref(),
                    backend,
                    services.paths().ok(),
                    stdout,
                )?;
                return Ok(ProcessOutcome::Success);
            }
            let selected = match account.as_deref() {
                None => None,
                Some(account) => match usage_vertical_for_account(account) {
                    Some(vertical) => Some(vertical),
                    None => {
                        return Err(CliFailure::classified(
                            CliFailureKind::Usage,
                            "usage account must be codex|grok|claude|openrouter|supergrok",
                        ));
                    }
                },
            };
            let view = collect_usage_limits(selected, backend)?;
            product_subscription_usage::write_limits(
                &view,
                services.paths().ok(),
                local_time_zone(),
                stdout,
            )
            .map_err(|_| CliFailure::new("stdout failed"))?;
            writeln!(
                stdout,
                "\nTurn tokens and context capacity require `plak usage --session <uuid>`; they are not subscription allowance."
            )
            .map_err(|_| CliFailure::new("stdout failed"))?;
            Ok(ProcessOutcome::Success)
        }
        Command::Run { .. }
        | Command::RunOpenRouter { .. }
        | Command::RunPlatform { .. }
        | Command::ContinueSession { .. }
        | Command::Tui { .. }
        | Command::Resume { .. }
        | Command::Agent { .. }
        | Command::Help
        | Command::CommandHelp { .. }
        | Command::Version
        | Command::ConfigEdit
        | Command::StatePruneLegacy { .. }
        | Command::StateKey
        | Command::ReloadProbe
        | Command::ExportVersion
        | Command::ImportResume { .. }
        | Command::Doctor
        | Command::TelemetryStatus { .. }
        | Command::TelemetrySql { .. }
        | Command::TelemetryPrune { .. } => Err(CliFailure::new("internal command dispatch error")),
        Command::CompatibilityNotice(_) => Err(CliFailure::new("internal command dispatch error")),
    }
}
/// Appends the attached account's allowance through the one host-owned Usage
/// projection. There is no second CLI renderer: `plak usage`, `plak usage
/// --session` and the TUI Limits text all come out of `render_limits_text`.
fn write_selected_subscription_usage(
    account: Option<&str>,
    backend: &mut impl AccountOps,
    paths: Option<&product_paths::PlakPaths>,
    stdout: &mut dyn io::Write,
) -> CliResult<()> {
    let Some(vertical) = account.and_then(usage_vertical_for_account) else {
        return Ok(());
    };
    writeln!(stdout).map_err(|_| CliFailure::new("stdout failed"))?;
    let view = collect_usage_limits(Some(vertical), backend)?;
    product_subscription_usage::write_limits(&view, paths, local_time_zone(), stdout)
        .map_err(|_| CliFailure::new("stdout failed"))
}

fn run_agent_command(services: &Services, command: Command) -> CliResult<ProcessOutcome> {
    let Command::Agent {
        session_id,
        provider,
        platform,
        wire,
        model,
        effort,
        effort_explicit,
        composition,
        resume_tree,
    } = command
    else {
        return Err(CliFailure::new("internal command dispatch error"));
    };
    run_agent(
        services,
        session_id,
        provider,
        platform,
        wire,
        model,
        effort,
        effort_explicit,
        composition,
        resume_tree,
    )
}

/// Quit ordering (§5.3). The terminal is handed back the moment the UI stops
/// driving it: a live turn's cancel settle and the durable session release
/// must not keep the alternate screen up with mouse reporting on — that is
/// how a slow release used to read as a frozen screen and then spill the
/// mouse reports it had buffered into the shell. After the terminal is cooked
/// and the resume hint is flushed, a voluntary quit asks the host to cancel
/// and then exits. It does not wait out session release and does not print
/// `quitting… releasing the session`; that wait is the hang. A TUI error still
/// joins under the product deadline and reports the worker's outcome.
/// Re-attach and hand-off rounds keep the terminal, since the next round
/// paints into the same alternate screen.
fn settle_tui_round(
    ui: Result<tui::TuiOutcome, tui::TuiError>,
    persistent_tui: &mut Option<tui::PersistentTerminal>,
    join: thread::JoinHandle<CliResult<()>>,
    host_mailbox: surface_protocol::UserActionSender,
    label: &str,
    store: &dyn store::session_records::SessionRecordStore,
    session_id: &str,
    cwd: &Path,
) -> CliResult<Result<tui::TuiOutcome, tui::TuiError>> {
    if matches!(
        ui,
        Ok(tui::TuiOutcome::RetryAttach { .. } | tui::TuiOutcome::Handoff { .. })
    ) {
        if !matches!(
            &ui,
            Ok(tui::TuiOutcome::Handoff {
                handoff: tui::TuiHandoff::Reload { .. },
                ..
            })
        ) {
            drop(host_mailbox);
            join_host_worker_bounded(join, HOST_WORKER_JOIN_TIMEOUT, label)?;
            return Ok(ui);
        }
    }
    let cleanup_errors = persistent_tui
        .take()
        .map(tui::PersistentTerminal::close)
        .unwrap_or_default();
    for error in &cleanup_errors {
        let _ = writeln!(io::stderr(), "terminal restore: {error}");
    }
    tui_timing::restored();
    let restoration = if cleanup_errors.is_empty() {
        Ok(())
    } else {
        Err(CliFailure::new(format!(
            "terminal restoration failed: {}",
            cleanup_errors.join("; ")
        )))
    };
    let mut stdout = io::stdout();
    if matches!(
        &ui,
        Ok(tui::TuiOutcome::Handoff {
            handoff: tui::TuiHandoff::Reload { .. },
            ..
        })
    ) {
        let _ = writeln!(stdout, "reloading…");
        let _ = stdout.flush();
        host_mailbox.request_abandon_and_release();
        let _ = join_host_worker_bounded(join, tui_timing::remaining_exit_budget(), label);
        tui_timing::exiting();
        return restoration.map(|()| ui);
    }
    // Only a session the product store lists is resumable; a shell that never
    // accepted a prompt is not saved and gets no hint. Print it before the
    // join so a slow release cannot delay the cooked prompt.
    if let Ok(tui::TuiOutcome::Exited { attached_session }) = &ui
        && let Ok(record) = store.load(attached_session.as_deref().unwrap_or(session_id))
    {
        let _ = writeln!(stdout, "{}", resume_exit_line(&record.id, cwd));
        let _ = stdout.flush();
    }
    if matches!(&ui, Ok(tui::TuiOutcome::Exited { .. })) {
        // Restoration problems were already written to stderr. Returning
        // would join host release and hold the cooked shell.
        exit_restored_tui(join, host_mailbox);
    }
    host_mailbox.request_abandon_and_release();
    drop(host_mailbox);
    let cleanup = combine_cleanup(join_tui_host_worker(join, label), restoration);
    match (ui, cleanup) {
        (ui, Ok(())) => Ok(ui),
        (Ok(_), Err(cleanup)) => Err(cleanup),
        (Err(ui_error), Err(cleanup)) => Err(CliFailure::new(format!(
            "{ui_error}; shutdown cleanup also failed: {}",
            cleanup.message()
        ))),
    }
}

/// Pair the host worker's verdict with terminal restoration: either one
/// failing fails the round, and both failures keep both reasons.
fn combine_cleanup(primary: CliResult<()>, cleanup: CliResult<()>) -> CliResult<()> {
    match (primary, cleanup) {
        (Ok(()), result) | (result, Ok(())) => result,
        (Err(primary), Err(cleanup)) => Err(CliFailure::new(format!(
            "{}; cleanup: {}",
            primary.message(),
            cleanup.message()
        ))),
    }
}

/// The cooked-terminal line printed after a quit. The short id is the resume
/// picker's prefix, which `plak resume` resolves within the workspace.
fn resume_exit_line(session_id: &str, workspace: &Path) -> String {
    let short: String = session_id.chars().take(tui::SHORT_ID_CHARS).collect();
    format!(
        "Resume this session with: plak resume {short}  (workspace {})",
        workspace.display()
    )
}

/// Quits after a declined launch resume picker found no attachable default.
///
/// The terminal is restored and the ordinary exit hint is printed, so the
/// operator lands back in the shell with the same line any quit gives them
/// rather than in a `not ready` product whose only escape is `/attach`.
fn finish_declined_resume_launch(
    persistent_tui: &mut Option<tui::PersistentTerminal>,
    store: &dyn store::session_records::SessionRecordStore,
    session_id: &str,
    cwd: &Path,
    attach_error: &str,
) -> ProcessOutcome {
    let cleanup_errors = persistent_tui
        .take()
        .map(tui::PersistentTerminal::close)
        .unwrap_or_default();
    for error in &cleanup_errors {
        let _ = writeln!(io::stderr(), "terminal restore: {error}");
    }
    let mut stdout = io::stdout();
    let _ = writeln!(
        stdout,
        "no session resumed, and this workspace has no attachable default ({attach_error})."
    );
    if let Ok(record) = store.load(session_id) {
        let _ = writeln!(stdout, "{}", resume_exit_line(&record.id, cwd));
    }
    let _ = stdout.flush();
    ProcessOutcome::Success
}

/// `plak resume <id>` takes the canonical id or the short prefix the exit line
/// and the picker print, resolved among this workspace's sessions.
/// Whether `requested` can only name another build's session: long enough to
/// mean something, and matching nothing in this build's store for `cwd`. An
/// ambiguous or too-short id is this build's refusal, never a carry.
fn names_no_local_session(
    store: &dyn store::session_records::SessionRecordStore,
    cwd: &Path,
    requested: &str,
) -> bool {
    if requested.chars().count() < commands::session::MIN_SESSION_ID_PREFIX {
        return false;
    }
    if uuid::Uuid::parse_str(requested).is_ok_and(|id| id.hyphenated().to_string() == requested) {
        return store.load(requested).is_err();
    }
    store.list_for_workspace(cwd).is_ok_and(|sessions| {
        !sessions
            .iter()
            .any(|session| session.id.starts_with(requested))
    })
}

fn resolve_resume_session_id(
    paths: &product_paths::PlakPaths,
    store: &dyn store::session_records::SessionRecordStore,
    cwd: &Path,
    requested: &str,
) -> CliResult<String> {
    if uuid::Uuid::parse_str(requested).is_ok_and(|id| id.hyphenated().to_string() == requested) {
        return Ok(requested.to_owned());
    }
    let sessions = store.list_for_workspace(cwd).map_err(CliFailure::new)?;
    let ids: Vec<&str> = sessions.iter().map(|session| session.id.as_str()).collect();
    commands::session::match_session_id_prefix(requested, &ids)
        .map(str::to_owned)
        .map_err(|error| {
            CliFailure::new(
                home::state::incompatible_session_refusal(paths, requested).unwrap_or(error),
            )
        })
}

fn run_doctor(services: &Services, stdout: &mut dyn io::Write) -> CliResult<ProcessOutcome> {
    let paths = services.paths()?;
    let doctor_paths = product_doctor::DoctorPaths::under(paths.home().to_path_buf());
    let loaded = product_config::ProductConfig::load(paths);
    let config = product_doctor::inspect_config(&paths.config_file(), |_| {
        loaded.as_ref().map(|_| ()).map_err(Clone::clone)
    });
    let config = match (&loaded, config) {
        (Err(error), product_doctor::ConfigStatus::Missing) => {
            product_doctor::ConfigStatus::Invalid(error.clone())
        }
        (_, config) => config,
    };
    let selected = loaded.unwrap_or_else(|_| product_config::ProductConfig::default_product());
    let product_database =
        product_doctor::inspect_state_database(&paths.product_database(), store::SCHEMA_VERSION);
    let subagents_database = product_doctor::inspect_state_database(
        &paths.subagents_database(),
        plakous_recursive_subagents_store::SCHEMA_VERSION,
    );
    let runtime_database = product_doctor::inspect_runtime_database(&paths.runtime_database());
    let runtime_store = product_doctor::inspect_runtime_store(&paths.runtime_database());
    let selected_composition = platform_catalog::root_composition(&selected);
    let telemetry = telemetry_sink::inspect_product_telemetry(paths);
    let local_composition_required = matches!(
        selected_composition,
        product_config::ProductCompositionSelection::JsPtc
            | product_config::ProductCompositionSelection::PyPtc
            | product_config::ProductCompositionSelection::NativePtc
    ) || (selected_composition
        == product_config::ProductCompositionSelection::FollowCatalog
        && selected.default_account() == Some("codex"));
    let composition_host = match selected_composition {
        product_config::ProductCompositionSelection::PyPtc => {
            match sidecars::python_bundle::locate() {
                Ok(path) => product_doctor::CompositionHostStatus::Included {
                    identity: path.display().to_string(),
                },
                Err(error) => product_doctor::CompositionHostStatus::Unavailable(error),
            }
        }
        product_config::ProductCompositionSelection::JsPtc
        | product_config::ProductCompositionSelection::NativePtc => {
            match sidecars::js_host::locate() {
                Ok(path) => product_doctor::CompositionHostStatus::Included {
                    identity: path.display().to_string(),
                },
                Err(error) => product_doctor::CompositionHostStatus::Unavailable(error),
            }
        }
        product_config::ProductCompositionSelection::FollowCatalog
            if selected.default_account() == Some("codex") =>
        {
            match sidecars::js_host::locate() {
                Ok(path) => product_doctor::CompositionHostStatus::Included {
                    identity: path.display().to_string(),
                },
                Err(error) => product_doctor::CompositionHostStatus::Unavailable(error),
            }
        }
        _ => product_doctor::CompositionHostStatus::Included {
            identity: "not required by the selected composition".to_owned(),
        },
    };
    let openrouter_catalog = match openrouter_product::probe_openrouter_catalog_allowlist(
        paths,
        selected.default_account(),
        selected.default_model(),
    ) {
        openrouter_product::OpenRouterCatalogProbe::NotConfigured => {
            product_doctor::OpenRouterCatalogStatus::NotConfigured
        }
        openrouter_product::OpenRouterCatalogProbe::KeyPresentNoModel => {
            product_doctor::OpenRouterCatalogStatus::KeyPresentNoModel
        }
        openrouter_product::OpenRouterCatalogProbe::Ok => {
            product_doctor::OpenRouterCatalogStatus::Ok
        }
        openrouter_product::OpenRouterCatalogProbe::Failed(detail) => {
            product_doctor::OpenRouterCatalogStatus::Failed(detail)
        }
    };
    // `config OK` means the effective config and all template files parsed. Say before launch
    // whether the template new sessions bind is actually attachable. Both
    // inputs are local — the persisted catalog and the offline credential
    // posture — so this adds no network I/O to `doctor`.
    let root_template = match selected.root_agent.as_deref() {
        None => product_doctor::RootTemplateStatus::Unset,
        Some(name) => match selected.templates.get(name) {
            None => product_doctor::RootTemplateStatus::Undefined {
                name: name.to_owned(),
            },
            Some(template) => {
                let binding = match template.effort.as_deref() {
                    Some(effort) => format!("{}/{} · {effort}", template.account, template.model),
                    None => format!("{}/{}", template.account, template.model),
                };
                match presentation::tui_presentation_values(paths) {
                    Err(error) => product_doctor::RootTemplateStatus::Unknown {
                        detail: error.message().to_owned(),
                    },
                    Ok((_, sections)) => {
                        let credential = product_config::catalog_credential_readiness(
                            &template.account,
                            &sections,
                        );
                        match product_config::template_readiness(template, credential, &sections) {
                            product_config::TemplateReadiness::Ready => {
                                product_doctor::RootTemplateStatus::Ready {
                                    name: name.to_owned(),
                                    binding,
                                }
                            }
                            product_config::TemplateReadiness::NotReady(reason) => {
                                product_doctor::RootTemplateStatus::NotReady {
                                    name: name.to_owned(),
                                    binding,
                                    reason: reason.reason(),
                                }
                            }
                        }
                    }
                }
            }
        },
    };
    let state_home = services.home().state()?.to_path_buf();
    let report = product_doctor::diagnose(product_doctor::DoctorInputs {
        version: plakous::VERSION.to_owned(),
        paths: doctor_paths,
        config,
        product_database,
        subagents_database,
        runtime_database,
        runtime_store,
        retention_failure_at_ns: telemetry_store::TelemetryStore::unresolved_retention_failure(
            paths,
        )
        .ok()
        .flatten(),
        telemetry,
        working_directory: env::current_dir().map_err(|_| "cwd unavailable".to_owned()),
        terminals: product_doctor::TerminalFacts::capture(),
        composition_requirement: if local_composition_required {
            product_doctor::Requirement::Required
        } else {
            product_doctor::Requirement::Optional
        },
        composition_host,
        python_containment_required: selected_composition
            == product_config::ProductCompositionSelection::PyPtc,
        runtime_directory: env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
        openrouter_catalog,
        root_template,
        oauth: product_doctor::OauthCredentialProbes {
            codex: product_doctor::probe_oauth_credential(
                "codex",
                &state_home.join("plak/subscriptions/codex/credential.json"),
                telemetry_store::TelemetryStore::latest_credential_refresh(paths, "codex")
                    .map_err(|error| error.to_string()),
            ),
            xai: product_doctor::probe_oauth_credential(
                "xai",
                &state_home.join("plak/subscriptions/xai/credential.json"),
                telemetry_store::TelemetryStore::latest_credential_refresh(paths, "xai")
                    .map_err(|error| error.to_string()),
            ),
            claude: product_doctor::probe_oauth_credential(
                "claude",
                &state_home.join("plak/subscriptions/claude/credential.json"),
                telemetry_store::TelemetryStore::latest_credential_refresh(paths, "claude")
                    .map_err(|error| error.to_string()),
            ),
        },
    });

    writeln!(stdout, "Plak doctor\n").map_err(|_| CliFailure::new("stdout failed"))?;
    let mut table = cli_output::Table::new(["CHECK", "RESULT", "DETAIL", "FIX"]);
    for check in &report.checks {
        table
            .push([
                match check.kind {
                    product_doctor::CheckKind::OauthCodex => "OAuth codex".to_owned(),
                    product_doctor::CheckKind::OauthXai => "OAuth grok".to_owned(),
                    product_doctor::CheckKind::OauthClaude => "OAuth claude".to_owned(),
                    product_doctor::CheckKind::OpenRouterCatalog => "OpenRouter catalog".to_owned(),
                    _ => title_case_token(check.kind.key()),
                },
                check.severity.label().to_ascii_uppercase(),
                format!("{} · {}", check.result, check.detail),
                check.remediation.unwrap_or("—").to_owned(),
            ])
            .map_err(CliFailure::new)?;
    }
    cli_human_output()
        .write_table(stdout, &table)
        .map_err(|_| CliFailure::new("stdout failed"))?;
    let legacy_warnings = home::state::report_legacy_state(paths, stdout)
        .map_err(|_| CliFailure::new("stdout failed"))?;
    writeln!(
        stdout,
        "\nResult: {} passed · {} warnings · {} failures\nProvider credentials: run `plak status`",
        report.summary.ok,
        report.summary.warnings + legacy_warnings,
        report.summary.errors
    )
    .map_err(|_| CliFailure::new("stdout failed"))?;
    if report.summary.errors > 0 {
        Err(CliFailure::classified(
            CliFailureKind::Agent,
            format!("doctor found {} blocking failures", report.summary.errors),
        ))
    } else {
        Ok(ProcessOutcome::Success)
    }
}

/// The role label and text `session show` prints for one transcript row;
/// rows without operator text are skipped.
fn transcript_entry(row: &product_sessions::TranscriptRow) -> Option<(&'static str, &str)> {
    use product_sessions::TranscriptRow;
    match row {
        TranscriptRow::User { text, .. } => Some(("User", text)),
        TranscriptRow::Steer { text, .. } => Some(("Steer", text)),
        TranscriptRow::Assistant { text } => Some(("Assistant", text)),
        TranscriptRow::Notice(notice)
            if notice.placement() != product_sessions::NoticePlacement::SessionLog =>
        {
            Some(("Note", &notice.text))
        }
        _ => None,
    }
}

/// First meaningful transcript snippet for session lists / resume picker.
fn session_preview_line(session: &product_sessions::SessionRecord) -> String {
    if let Some(title) = session.title.as_deref().filter(|t| !t.is_empty()) {
        return title.chars().take(48).collect();
    }
    for row in session.transcript.iter() {
        let (product_sessions::TranscriptRow::User { text, .. }
        | product_sessions::TranscriptRow::Assistant { text }) = row
        else {
            continue;
        };
        let body = text.trim();
        if !body.is_empty() {
            return body.chars().take(48).collect();
        }
    }
    "(empty)".to_owned()
}

fn cli_human_output() -> cli_output::HumanOutput {
    cli_output::HumanOutput::resolve(
        cli_output::TerminalMode::of_stdout(),
        env::var_os("NO_COLOR").as_deref(),
        cli_output::DetailLevel::Summary,
    )
}

fn product_account_label(account: Option<&str>) -> String {
    account.map_or_else(
        || "—".to_owned(),
        |account| cli_output::ProviderLabel::new(account).to_string(),
    )
}

fn format_unix_seconds(seconds: u64) -> String {
    i64::try_from(seconds)
        .ok()
        .and_then(|seconds| time::OffsetDateTime::from_unix_timestamp(seconds).ok())
        .and_then(|timestamp| {
            timestamp
                .format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_else(|| seconds.to_string())
}

fn format_unix_millis(milliseconds: u64) -> String {
    let nanos = i128::from(milliseconds).saturating_mul(1_000_000);
    time::OffsetDateTime::from_unix_timestamp_nanos(nanos)
        .ok()
        .and_then(|timestamp| {
            timestamp
                .format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_else(|| milliseconds.to_string())
}

fn title_case_token(value: &str) -> String {
    let words = value.replace(['-', '_'], " ");
    let mut chars = words.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

fn event_table_row(value: &serde_json::Value) -> [String; 4] {
    let time = value
        .get("ts")
        .and_then(serde_json::Value::as_u64)
        .map_or_else(|| "—".to_owned(), format_unix_millis);
    let sequence = value
        .get("seq")
        .and_then(serde_json::Value::as_u64)
        .map_or_else(|| "—".to_owned(), |value| value.to_string());
    let kind = value
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    let details = value
        .as_object()
        .map(|object| {
            object
                .iter()
                .filter(|(key, _)| !matches!(key.as_str(), "ts" | "seq" | "kind"))
                .take(4)
                .map(|(key, value)| {
                    let value = value
                        .as_str()
                        .map_or_else(|| value.to_string(), str::to_owned);
                    format!("{key}={value}")
                })
                .collect::<Vec<_>>()
                .join(" · ")
        })
        .filter(|details| !details.is_empty())
        .unwrap_or_else(|| "—".to_owned());
    [time, sequence, kind, details]
}

/// Keys whose save must move through the successor-epoch apply.
///
/// The verdict is the key table's own `scope` column, so a new orchestration
/// key cannot be added without one.
fn config_key_changes_orchestration(key: &str) -> bool {
    product_config::config_key(key).is_some_and(|row| row.scope == ConfigScope::Epoch)
}

fn restore_former_attachment(
    record: &mut product_sessions::SessionRecord,
    resume: &mut bool,
    resume_pending: &mut bool,
    former_attachment: &mut Option<(product_sessions::SessionRecord, bool, bool)>,
) -> bool {
    let Some((former, former_resume, former_resume_pending)) = former_attachment.take() else {
        return false;
    };
    *record = former;
    *resume = former_resume;
    *resume_pending = former_resume_pending;
    true
}

/// Adopt the host-authoritative attached session before re-entering the loop.
///
/// The launcher's `record` is the session the process started on. `/clear`,
/// an in-host `/resume`, and a rebind branch all replace the attached session
/// *inside* the host, and nothing else tells the launcher: the editor return
/// path and the retry-attach path would both re-open the stale record and
/// silently discard everything the operator did since the switch. This is the
/// one place that identity is reconciled, so every terminal re-entry shares it.
///
/// A pending resume is settled by the adoption: the host published a different
/// attachment than the one the resume selected, so its rollback source is no
/// longer the truth about what is attached.
fn adopt_attached_session(
    store: &dyn store::session_records::SessionRecordStore,
    record: &mut product_sessions::SessionRecord,
    resume: &mut bool,
    resume_pending: &mut bool,
    former_attachment: &mut Option<(product_sessions::SessionRecord, bool, bool)>,
    attached_session: Option<&str>,
) -> CliResult<()> {
    let Some(attached) = attached_session.map(str::trim).filter(|id| !id.is_empty()) else {
        return Ok(());
    };
    if attached == record.id {
        return Ok(());
    }
    *record = store.load(attached).map_err(|error| {
        CliFailure::new(format!(
            "the attached session {attached} could not be reopened after the terminal handoff: {error}"
        ))
    })?;
    *resume = false;
    *resume_pending = false;
    *former_attachment = None;
    Ok(())
}

fn apply_tui_handoff(
    paths: &product_paths::PlakPaths,
    store: &dyn store::session_records::SessionRecordStore,
    record: &mut product_sessions::SessionRecord,
    resume: &mut bool,
    resume_pending: &mut bool,
    former_attachment: &mut Option<(product_sessions::SessionRecord, bool, bool)>,
    handoff: tui::TuiHandoff,
    reload_note: &mut Option<String>,
) -> CliResult<()> {
    let selected_exact_session = matches!(&handoff, tui::TuiHandoff::ResumeSession { .. });
    let former = selected_exact_session.then(|| (record.clone(), *resume, *resume_pending));
    handle_tui_handoff(paths, store, record, handoff, reload_note)?;
    if let Some(former) = former {
        *former_attachment = Some(former);
        *resume = true;
        *resume_pending = true;
    }
    Ok(())
}

fn apply_tui_handoff_after_host(
    paths: &product_paths::PlakPaths,
    store: &dyn store::session_records::SessionRecordStore,
    record: &mut product_sessions::SessionRecord,
    resume: &mut bool,
    resume_pending: &mut bool,
    former_attachment: &mut Option<(product_sessions::SessionRecord, bool, bool)>,
    handoff: tui::TuiHandoff,
    reload_note: &mut Option<String>,
) -> CliResult<()> {
    // A failed pending resume must not become the rollback source for another
    // exact selection. Successful publication already cleared the former
    // attachment and `resume_pending` before this boundary.
    if *resume_pending && former_attachment.is_some() {
        let _ = restore_former_attachment(record, resume, resume_pending, former_attachment);
    }
    apply_tui_handoff(
        paths,
        store,
        record,
        resume,
        resume_pending,
        former_attachment,
        handoff,
        reload_note,
    )
}

fn launch_tui(
    services: &Services,
    session_id: Option<String>,
    resume_picker: bool,
    opening_note: Option<String>,
) -> CliResult<ProcessOutcome> {
    launch_tui_with_attacher(
        services,
        session_id,
        resume_picker,
        opening_note,
        &NativeTuiAttacher,
    )
}

fn begin_tui_session_launch(
    _paths: &product_paths::PlakPaths,
    _record: &mut product_sessions::SessionRecord,
    resume: bool,
    _cwd: &Path,
) -> bool {
    if resume {
        return true;
    }
    // A freshly painted shell is not yet a product session. Keep its stable
    // candidate identity in memory until an accepted user action first commits
    // it; opening and quitting the TUI must not manufacture resume entries.
    false
}

fn run_persistent_tui(
    terminal: &mut Option<tui::PersistentTerminal>,
    launch: tui::TuiLaunch,
    actions: &surface_protocol::UserActionSender,
    events: surface_protocol::HostEventReceiver,
) -> Result<tui::TuiOutcome, tui::TuiError> {
    if terminal.is_none() {
        *terminal = Some(tui::PersistentTerminal::open(launch.mouse_enabled)?);
    }
    let Some(terminal) = terminal.as_mut() else {
        return Err(tui::TuiError::Io(
            "persistent TUI terminal was not initialized".into(),
        ));
    };
    // Keep the caller's sender. Dropping the last sender is Disconnected, and
    // the host treats that as Quit; which shutdown the round requests on the
    // mailbox is `settle_tui_round`'s call once the terminal is cooked.
    terminal.run(launch, Some(actions.clone()), Some(events))
}

/// Shell status while `plak resume` waits for the operator to pick a session.
const RESUME_PICKER_SHELL_REASON: &str = "pick a session to resume · Esc keeps this new session";

/// Asks the host for the cwd-scoped session list. The TUI itself marks the
/// request outstanding through `TuiLaunch::open_resume_picker`; only that pair
/// opens the picker when `HostEvent::ResumeSessions` arrives.
fn request_resume_picker(action_tx: &surface_protocol::UserActionSender) -> CliResult<()> {
    action_tx
        .send(surface_protocol::UserAction::ResumeSession { id: None })
        .map_err(|_| CliFailure::new("resume picker host is unavailable"))
}

/// Where each kind of credential `plak status` reports actually lives.
///
/// OAuth subscription records are XDG state, not product home, so emptying
/// `PLAK_HOME` leaves every OAuth account Ready. API keys are the opposite:
/// they are files under the product home. One line says both.
fn credential_locations_line(services: &Services) -> String {
    let oauth = home::paths::application_state_home().map_or_else(
        |_| "$XDG_STATE_HOME".to_owned(),
        |root| root.join("plak/subscriptions").display().to_string(),
    );
    let keys = services.paths().map_or_else(
        |_| "$PLAK_HOME/secrets".to_owned(),
        |paths| paths.secrets_dir().display().to_string(),
    );
    credential_locations_text(&oauth, &keys)
}

/// The sentence alone, so a test pins it without a product home.
fn credential_locations_text(oauth: &str, keys: &str) -> String {
    format!(
        "OAuth credentials live in {oauth}, outside the product home, so emptying it does not log you out. API keys live in {keys}."
    )
}

/// The exact `plak run` a non-terminal reader can paste, given their config.
///
/// A configured default account and model make `plak run -- "<prompt>"` the
/// whole command; without them the flags have to be named, because a command
/// that would fail on the next line is not guidance.
fn headless_run_command(services: &Services) -> &'static str {
    let binding = services.paths().map_or_else(
        |_| product_config::DefaultBinding::unbound(),
        product_config::DefaultBinding::load,
    );
    headless_run_command_for(binding.account.is_some() && binding.model.is_some())
}

/// The command text alone, so a test can pin both forms without a product home.
const fn headless_run_command_for(bound: bool) -> &'static str {
    if bound {
        "plak run -- \"<prompt>\""
    } else {
        "plak run --account <codex|grok|claude|openrouter|openai|anthropic|xai-api> --model <model> -- \"<prompt>\""
    }
}

fn launch_tui_with_attacher(
    services: &Services,
    session_id: Option<String>,
    resume_picker: bool,
    opening_note: Option<String>,
    attacher: &impl TuiAttacher,
) -> CliResult<ProcessOutcome> {
    if !rustix::termios::isatty(io::stdin()) || !io::stdout().is_terminal() {
        // A piped or scripted `plak` is most people's first contact with the
        // product, and a bare refusal is a poor one. The exit code is a
        // contract and stays 2; what changes is that the reader gets the same
        // command summary `plak help` prints, and then the one command that
        // does what they were trying to do.
        let mut stderr = io::stderr().lock();
        cli::render::global_help(&mut stderr).map_err(|_| CliFailure::new("stderr failed"))?;
        let guidance = if let Some(session_id) = session_id.as_deref() {
            format!(
                "the interactive TUI needs a terminal. To continue this session without one, run: plak run --session {session_id} -- \"<prompt>\""
            )
        } else if resume_picker {
            "interactive session selection needs a terminal. To resume without one, run: plak session list, then plak run --session <uuid> -- \"<prompt>\"".to_owned()
        } else {
            format!(
                "the interactive TUI needs a terminal. To run one turn without one, run: {}",
                headless_run_command(services)
            )
        };
        return Err(CliFailure::classified(CliFailureKind::Usage, guidance));
    }
    // Armed before the terminal is taken, so no `SIGTERM` or `SIGHUP` can
    // meet the default disposition while the alternate screen is up.
    signals::install_interactive(tui::force_restore_terminal);
    let paths = services.paths()?.clone();
    let store = services.session_records()?;
    home::state::ensure_usable_state(&paths, &mut io::stderr().lock()).map_err(CliFailure::new)?;
    let state_roots = services.state_roots()?;
    let cwd = env::current_dir().map_err(|_| CliFailure::new("cwd unavailable"))?;
    // No-id resume enters the ordinary TUI first. The existing host-owned
    // `/resume` action supplies the cwd-scoped snapshot to the one picker UI;
    // there is no second cooked-terminal selector and no implicit choice.
    let mut open_resume_picker = resume_picker;
    let mut opening_note = opening_note;
    let session_id = match session_id {
        Some(requested) => {
            let carried = if names_no_local_session(store.as_ref(), &cwd, &requested) {
                carry_foreign_session(services, &paths, &cwd, &requested)?
            } else {
                None
            };
            Some(match carried {
                Some((id, note)) => {
                    opening_note = Some(note);
                    id
                }
                None => resolve_resume_session_id(&paths, store.as_ref(), &cwd, &requested)?,
            })
        }
        None => None,
    };

    // Persist a session shell record for resume.
    let mut resume = session_id.is_some();
    let mut record = if let Some(ref id) = session_id {
        store.load(id).map_err(|e| {
            CliFailure::new(
                home::state::incompatible_session_refusal(&paths, id).unwrap_or_else(
                    || {
                        format!(
                            "session {id} not found ({e}); list with `plak session list` or `plak resume`"
                        )
                    },
                ),
            )
        })?
    } else {
        product_sessions::SessionRecord::new(cwd.clone())
    };

    let mut initial_cfg = product_config::ProductConfig::load(&paths)
        .map_err(|error| CliFailure::new(format!("invalid product config: {error}")))?;
    for warning in &initial_cfg.warnings {
        let _ = writeln!(io::stderr().lock(), "warning: {warning}");
    }
    if !resume {
        seed_new_session_binding(&mut record, &initial_cfg)?;
        // Never attach with a partial cross-provider tuple. Repair the complete
        // model-dependent default set in one save so later launches are stable.
        if repair_invalid_new_session_binding(&mut record, &mut initial_cfg) {
            let _ = initial_cfg.save(&paths);
        }
    }

    if resume {
        let requested = std::fs::canonicalize(&cwd).unwrap_or_else(|_| cwd.clone());
        let recorded = std::fs::canonicalize(&record.workspace_root)
            .unwrap_or_else(|_| record.workspace_root.clone());
        if requested != recorded {
            return Err(CliFailure::new(format!(
                "session {} belongs to {}; exact resume from {} was refused",
                record.id,
                recorded.display(),
                requested.display()
            )));
        }
    }

    // A fresh product shell is durable immediately. Exact resume remains a
    // read-only presentation until the runtime host opens its canonical
    // locator and publishes the one resume acknowledgement.
    let mut resume_pending = begin_tui_session_launch(&paths, &mut record, resume, &cwd);
    tui_timing::attach(&paths, &record.id, resume);
    build_identity::prepare_binary_id(None);
    tui::warm_highlighting();

    // Re-attach loop: profile Use now and `/attach` return TuiOutcome::RetryAttach so we
    // re-resolve OAuth and spawn the live agent without the operator quitting plak.
    let mut tui_presentation_cache: Option<(
        Instant,
        Vec<plakous_host_protocol::views::ProviderDef>,
        Vec<plakous_host_protocol::views::ProviderSection>,
    )> = None;
    let mut former_attachment: Option<(product_sessions::SessionRecord, bool, bool)> = None;
    let mut persistent_tui: Option<tui::PersistentTerminal> = None;
    // New and exact-resume launches paint a disabled shell before provider,
    // catalog, or runtime preparation. Attachment is claimed only after the
    // deferred host has authoritatively opened the selected runtime.
    let authoritative_catalog_admission = attacher.supports_deferred_preparation();
    let mut defer_startup = authoritative_catalog_admission;
    // Set when the operator declined the launch resume picker: this round is
    // the product attaching the workspace default on their behalf, so a failure
    // to attach quits cleanly rather than parking them in a shell they never
    // asked for.
    let mut declined_resume_picker = false;
    // One line for the first round that can show it: why the session moved
    // builds, or why a reload kept it here.
    let mut pending_note = opening_note.or_else(|| {
        resume
            .then(|| carry::carried_forward_label(&record))
            .flatten()
    });
    loop {
        // The flag lives for exactly one round: the round the operator declined
        // the picker sets it, and the round the product starts on their behalf
        // consumes it.
        let declined_resume_round = std::mem::take(&mut declined_resume_picker);
        let cfg = product_config::ProductConfig::load(&paths)
            .map_err(|error| CliFailure::new(format!("invalid product config: {error}")))?;

        // Reload session body (transcript/queue) and merge freshest bindings.
        // TUI/shell write both session + config.toml; config is the binding source of
        // truth after a profile's Use now, disk session carries conversation state.
        if let Ok(updated) = store.load(&record.id)
            && updated.id == record.id
        {
            // Reload the complete product authority. Selectively copying UI
            // fields can discard the stable create operation or an unresolved
            // delivery intent after an attach/retry boundary.
            record = updated;
        }
        if !resume_pending && record.store_revision != 0 {
            store.save(&mut record).map_err(CliFailure::new)?;
        }

        let account = record.account.clone();
        let model_id = record.model.clone();
        let effort_src = record.effort.as_deref();
        let (mut effort, mut effort_explicit) = product_host::effort_from_config(effort_src);
        // Chrome: only show effort when operator/config set it explicitly
        // (no silent medium paint).
        let mut effort_label = if effort_explicit {
            match effort {
                ReasoningEffort::Low => Some("low".to_owned()),
                ReasoningEffort::Medium => Some("medium".to_owned()),
                ReasoningEffort::High => Some("high".to_owned()),
                ReasoningEffort::XHigh => Some("xhigh".to_owned()),
                ReasoningEffort::Max => Some("max".to_owned()),
                ReasoningEffort::Ultra => Some("ultra".to_owned()),
                ReasoningEffort::None => Some("none".to_owned()),
                ReasoningEffort::TokenBudget(n) => Some(format!("tokens:{n}")),
                _ => None,
            }
        } else {
            None
        };
        let (providers, model_sections) = if defer_startup {
            (supported_provider_rows(), Vec::new())
        } else if let Some((at, providers, sections)) = &tui_presentation_cache
            && at.elapsed() < Duration::from_secs(45)
        {
            (providers.clone(), sections.clone())
        } else {
            let (providers, sections) = tui_presentation_values(&paths)?;
            tui_presentation_cache = Some((Instant::now(), providers.clone(), sections.clone()));
            (providers, sections)
        };
        if record.plakous_locator.is_none()
            && !effort_explicit
            && account
                .as_deref()
                .zip(model_id.as_deref())
                .and_then(|(account, model)| {
                    model_sections
                        .iter()
                        .find(|section| section.ready && section.account == account)
                        .and_then(|section| section.entries.iter().find(|entry| entry.id == model))
                })
                .is_some_and(|entry| entry.effort_options.iter().any(|value| value == "medium"))
        {
            effort = ReasoningEffort::Medium;
            effort_explicit = true;
            effort_label = Some("medium".to_owned());
            record.effort.clone_from(&effort_label);
            store.save(&mut record).map_err(CliFailure::new)?;
        }
        let launch_base = tui::TuiLaunch {
            version: plakous::VERSION,
            session_id: Some(record.id.clone()),
            workspace: cwd.clone(),
            workspace_chrome: product_workspace::workspace_label(&cwd),
            open_resume_picker: false,
            account: account.clone(),
            model: model_id.clone(),
            effort: effort_label,
            extra: (record.target_selection
                == product_config::ProductTargetSelection::NativePriority)
                .then(|| "priority".to_owned()),
            composition: record.composition.into(),
            protocol: match &record.target_selection {
                product_config::ProductTargetSelection::OpenRouter { wire, .. } => {
                    match wire.selection {
                        product_config::ProductProtocolSelection::Auto => {
                            plakous_host_protocol::ProtocolSelection::Auto
                        }
                        product_config::ProductProtocolSelection::Chat => {
                            plakous_host_protocol::ProtocolSelection::Chat
                        }
                        product_config::ProductProtocolSelection::Responses => {
                            plakous_host_protocol::ProtocolSelection::Responses
                        }
                        product_config::ProductProtocolSelection::Messages => {
                            plakous_host_protocol::ProtocolSelection::Messages
                        }
                    }
                }
                product_config::ProductTargetSelection::Native
                | product_config::ProductTargetSelection::NativePriority
                | product_config::ProductTargetSelection::Platform { .. } => {
                    plakous_host_protocol::ProtocolSelection::Auto
                }
            },
            effective_presentation: product_agent::effective_presentation_label(
                record.composition,
                account
                    .as_deref()
                    .and_then(product_host::oauth_provider_for_account),
                None,
            )
            .to_owned(),
            agent_enabled: false,
            mouse_enabled: cfg.tui_mouse,
            theme: match cfg.theme {
                product_config::ProductTheme::DefaultDark => {
                    surface_protocol::SettingsTheme::DefaultDark
                }
                product_config::ProductTheme::DefaultLight => {
                    surface_protocol::SettingsTheme::DefaultLight
                }
                product_config::ProductTheme::CatppuccinLatte => {
                    surface_protocol::SettingsTheme::CatppuccinLatte
                }
                product_config::ProductTheme::CatppuccinFrappe => {
                    surface_protocol::SettingsTheme::CatppuccinFrappe
                }
                product_config::ProductTheme::CatppuccinMacchiato => {
                    surface_protocol::SettingsTheme::CatppuccinMacchiato
                }
                product_config::ProductTheme::CatppuccinMocha => {
                    surface_protocol::SettingsTheme::CatppuccinMocha
                }
            },
            settings: Some(shell::settings::settings_snapshot(&cfg)),
            providers,
            telemetry_paths: Some(paths.clone()),
            time_zone: local_time_zone(),
            // This round only: the note explains the round the product started
            // on the operator's behalf, and must not reappear on the next one.
            opening_note: pending_note
                .take()
                .or_else(|| declined_resume_round.then(|| tui::DECLINED_RESUME_NOTE.to_owned())),
        };

        let ready_catalog = plakous_host_protocol::views::ReadyCatalog::new(model_sections.clone());
        // Reasoning OAuth models require explicit effort before agent attach
        // (picker enforces; legacy configs without effort stay on shell).
        let attach_eligibility = if authoritative_catalog_admission {
            tui_attach_eligibility(
                &record.target_selection,
                &ready_catalog,
                account.as_deref(),
                model_id.as_deref(),
                effort_explicit,
            )
        } else if account.as_deref().zip(model_id.as_deref()).is_some() && effort_explicit {
            // Injected test/product attachers carry their own sealed target
            // authority and deliberately do not depend on live discovery.
            TuiAttachEligibility::Ready
        } else {
            TuiAttachEligibility::Unavailable
        };
        let model_needs_explicit_effort =
            attach_eligibility == TuiAttachEligibility::EffortRequired;
        let can_attach = attach_eligibility == TuiAttachEligibility::Ready;

        let resume_publication =
            resume_pending.then(|| Arc::new(std::sync::atomic::AtomicBool::new(false)));
        let attach = if defer_startup {
            Err("deferred startup".to_owned())
        } else if can_attach {
            attacher.attach(
                &paths,
                &state_roots,
                record.clone(),
                account.as_deref(),
                model_id.as_deref(),
                effort,
                effort_explicit,
                &cwd,
                &ready_catalog,
                resume_publication.clone(),
            )
        } else if model_needs_explicit_effort {
            Err("effort_required".to_owned())
        } else {
            Err("selected model is absent from the discovered Ready catalog".to_owned())
        };

        let clear_publication = Arc::new(std::sync::Mutex::new(None::<String>));
        let result = if defer_startup {
            defer_startup = false;
            let prep_paths = paths.clone();
            let prep_state_roots = state_roots.clone();
            let prep_record = record.clone();
            let prep_cwd = cwd.clone();
            let prep_account = account.clone();
            let prep_model = model_id.clone();
            let prep_resume_picker = open_resume_picker;
            let deferred_resume_publication = resume_publication.clone();
            let prep_needs_setup = product_onboarding::needs_setup(&cfg);
            let (action_tx, event_rx, join) =
                product_host::spawn_deferred_tui_host_with_clear_publication(
                    move || {
                        let shell_host = shell::host::ProductShellHost::boxed(&prep_paths);
                        if prep_resume_picker {
                            let mode =
                                product_host::TuiHostMode::Shell(product_host::ShellHostConfig {
                                    surface: Some(shell::settings::ProductSurface::boxed(
                                        &prep_paths,
                                    )),
                                    home: prep_paths.session_home(),
                                    binding:
                                        shell::binding_document::ProductRootBindingDocument::shared(
                                            &prep_paths,
                                        ),
                                    subscriptions: shell::host::open_product_subscriptions(),
                                    openrouter_credentials:
                                        openrouter_product::ProductOpenRouterCredentials::shared(
                                            &prep_paths,
                                        ),
                                    store: prep_state_roots.session_records(),
                                    product_home: prep_paths.home().to_path_buf(),
                                    session: prep_record,
                                    cwd: prep_cwd,
                                    reason: RESUME_PICKER_SHELL_REASON.to_owned(),
                                    ready_catalog:
                                        plakous_host_protocol::views::ReadyCatalog::default(),
                                });
                            let catalog_refresh =
                                shell::catalog_refresh::hook(prep_paths.clone(), &mode);
                            return product_host::TuiHostPreparation {
                                providers: supported_provider_rows(),
                                catalog_refresh: Some(catalog_refresh),
                                shell_host,
                                mode,
                            };
                        }
                        // First-paint shell must still open if the bundle read
                        // fails. Empty native_models never attach Codex: Ready
                        // goes through build_host_config, which refuses
                        // a persist miss.
                        let presentation_started = Instant::now();
                        let presentation =
                            tui_presentation_bundle(&prep_paths).unwrap_or_else(|_| {
                                TuiPresentationBundle {
                                    providers: supported_provider_rows(),
                                    sections: plakous_host_protocol::views::ReadyCatalog::default(),
                                    native_models: Vec::new(),
                                }
                            });
                        let presentation_ms =
                            u64::try_from(presentation_started.elapsed().as_millis())
                                .unwrap_or(u64::MAX);
                        let TuiPresentationBundle {
                            providers,
                            sections,
                            native_models,
                        } = presentation;
                        let eligibility = tui_attach_eligibility(
                            &prep_record.target_selection,
                            &sections,
                            prep_account.as_deref(),
                            prep_model.as_deref(),
                            effort_explicit,
                        );
                        let mode = if eligibility == TuiAttachEligibility::Ready {
                            let resolve_started = Instant::now();
                            match build_host_config(
                                &prep_paths,
                                &prep_state_roots,
                                prep_record.clone(),
                                prep_account.as_deref(),
                                prep_model.as_deref(),
                                effort,
                                effort_explicit,
                                &prep_cwd,
                                &sections,
                                deferred_resume_publication,
                                &native_models,
                            ) {
                                Ok(mut config) => {
                                    config.ready_path = product_host::ReadyPathTiming {
                                        presentation_ms,
                                        resolve_ms: u64::try_from(
                                            resolve_started.elapsed().as_millis(),
                                        )
                                        .unwrap_or(u64::MAX),
                                    };
                                    product_host::TuiHostMode::Attached(config)
                                }
                                Err(error) => product_host::TuiHostMode::Shell(
                                    product_host::ShellHostConfig {
                                        surface: Some(shell::settings::ProductSurface::boxed(
                                            &prep_paths,
                                        )),
                                        product_home: prep_paths.home().to_path_buf(),
                                        home: prep_paths.session_home(),
                                        binding: shell::binding_document::ProductRootBindingDocument::shared(&prep_paths),
                                        subscriptions: shell::host::open_product_subscriptions(),
                                        openrouter_credentials: openrouter_product::ProductOpenRouterCredentials::shared(&prep_paths),
                                        store: prep_state_roots.session_records(),
                                        session: prep_record,
                                        cwd: prep_cwd,
                                        reason: format!("resolve_failed: {error}"),
                                        ready_catalog: sections.clone(),
                                    },
                                ),
                            }
                        } else {
                            product_host::TuiHostMode::Shell(product_host::ShellHostConfig {
                                surface: Some(shell::settings::ProductSurface::boxed(&prep_paths)),
                                home: prep_paths.session_home(),
                                binding:
                                    shell::binding_document::ProductRootBindingDocument::shared(
                                        &prep_paths,
                                    ),
                                subscriptions: shell::host::open_product_subscriptions(),
                                openrouter_credentials:
                                    openrouter_product::ProductOpenRouterCredentials::shared(
                                        &prep_paths,
                                    ),
                                store: prep_state_roots.session_records(),
                                product_home: prep_paths.home().to_path_buf(),
                                session: prep_record,
                                cwd: prep_cwd,
                                reason: deferred_attach_refusal_reason(
                                    eligibility,
                                    prep_needs_setup,
                                ),
                                ready_catalog: sections.clone(),
                            })
                        };
                        let catalog_refresh =
                            shell::catalog_refresh::hook(prep_paths.clone(), &mode);
                        product_host::TuiHostPreparation {
                            providers,
                            mode,
                            catalog_refresh: Some(catalog_refresh),
                            shell_host,
                        }
                    },
                    Some(Arc::clone(&clear_publication)),
                );
            let mut launch = launch_base;
            if open_resume_picker {
                request_resume_picker(&action_tx)?;
                launch.open_resume_picker = true;
                open_resume_picker = false;
            }
            let ui = run_persistent_tui(&mut persistent_tui, launch, &action_tx, event_rx);
            let ui = settle_tui_round(
                ui,
                &mut persistent_tui,
                join,
                action_tx,
                "TUI controller",
                store.as_ref(),
                &record.id,
                &cwd,
            )?;
            if resume_publication
                .as_ref()
                .is_some_and(|publication| publication.load(std::sync::atomic::Ordering::Acquire))
            {
                resume_pending = false;
            }
            ui
        } else {
            match attach {
                Ok((action_tx, event_rx, join)) => {
                    let mut launch = launch_base;
                    launch.agent_enabled = true;
                    if open_resume_picker {
                        request_resume_picker(&action_tx)?;
                        launch.open_resume_picker = true;
                        open_resume_picker = false;
                    }
                    let ui = run_persistent_tui(&mut persistent_tui, launch, &action_tx, event_rx);
                    let ui = settle_tui_round(
                        ui,
                        &mut persistent_tui,
                        join,
                        action_tx,
                        "attached session",
                        store.as_ref(),
                        &record.id,
                        &cwd,
                    )?;
                    // Channels prove only that the worker started. The exact
                    // resume becomes authoritative only after canonical
                    // runtime open and durable product publication set this
                    // acknowledgement inside the host.
                    if resume_publication.as_ref().is_some_and(|publication| {
                        publication.load(std::sync::atomic::Ordering::Acquire)
                    }) {
                        former_attachment = None;
                        resume_pending = false;
                    }
                    ui
                }
                Err(attach_err) => {
                    if restore_former_attachment(
                        &mut record,
                        &mut resume,
                        &mut resume_pending,
                        &mut former_attachment,
                    ) {
                        continue;
                    }
                    // The operator declined the resume picker and this
                    // workspace has no attachable default. Opening another
                    // not-ready shell would strand them,
                    // so quit through the ordinary exit path instead.
                    if declined_resume_round {
                        return Ok(finish_declined_resume_launch(
                            &mut persistent_tui,
                            store.as_ref(),
                            &record.id,
                            &cwd,
                            &attach_err,
                        ));
                    }
                    let reason = if model_needs_explicit_effort && !effort_explicit {
                        "effort_required".to_owned()
                    } else if product_onboarding::needs_setup(&cfg) {
                        "missing_defaults".to_owned()
                    } else if attach_err == "effort_required" {
                        "effort_required".to_owned()
                    } else {
                        // Keep short reason tag for logs; detail is in attach_err for TUI.
                        format!("resolve_failed: {attach_err}")
                    };
                    product_session_log::emit(
                        &paths.session_home(),
                        &record.id,
                        product_session_log::SessionEvent::new("host.attach_failed")
                            .opt_str("account", account.as_deref())
                            .opt_str("model", model_id.as_deref())
                            .str("reason", &reason),
                    );
                    let (action_tx, event_rx, join) =
                        shell::host::spawn_shell_host_with_clear_publication(
                            product_host::ShellHostConfig {
                                surface: Some(shell::settings::ProductSurface::boxed(&paths)),
                                product_home: paths.home().to_path_buf(),
                                home: paths.session_home(),
                                binding:
                                    shell::binding_document::ProductRootBindingDocument::shared(
                                        &paths,
                                    ),
                                subscriptions: shell::host::open_product_subscriptions(),
                                openrouter_credentials:
                                    openrouter_product::ProductOpenRouterCredentials::shared(&paths),
                                store: state_roots.session_records(),
                                session: record.clone(),
                                cwd: cwd.clone(),
                                reason: reason.clone(),
                                ready_catalog: plakous_host_protocol::views::ReadyCatalog::new(
                                    model_sections.clone(),
                                ),
                            },
                            Some(Arc::clone(&clear_publication)),
                        );
                    let mut launch = launch_base;
                    launch.agent_enabled = false;
                    if open_resume_picker {
                        request_resume_picker(&action_tx)?;
                        launch.open_resume_picker = true;
                        open_resume_picker = false;
                    }
                    let ui = run_persistent_tui(&mut persistent_tui, launch, &action_tx, event_rx);
                    settle_tui_round(
                        ui,
                        &mut persistent_tui,
                        join,
                        action_tx,
                        "shell session",
                        store.as_ref(),
                        &record.id,
                        &cwd,
                    )?
                }
            }
        };

        match result {
            Ok(tui::TuiOutcome::RetryAttach {
                attached_session,
                declined_resume_picker: declined,
            }) => {
                declined_resume_picker = declined;
                product_session_log::emit(
                    &paths.session_home(),
                    &record.id,
                    product_session_log::SessionEvent::new("host.reattach_requested")
                        .opt_str("account", account.as_deref())
                        .opt_str("model", model_id.as_deref()),
                );
                // The not-ready shell loop breaks before it can publish a
                // binding, so its clear target arrives on its own channel.
                let clear_target = clear_publication
                    .lock()
                    .map_err(|_| CliFailure::new("clear handoff publication was poisoned"))?
                    .take();
                adopt_attached_session(
                    store.as_ref(),
                    &mut record,
                    &mut resume,
                    &mut resume_pending,
                    &mut former_attachment,
                    clear_target.as_deref().or(attached_session.as_deref()),
                )?;
            }
            Ok(tui::TuiOutcome::Handoff {
                handoff,
                attached_session,
            }) => {
                if matches!(
                    &handoff,
                    tui::TuiHandoff::EditConfig | tui::TuiHandoff::Reload { .. }
                ) {
                    persistent_tui = None;
                }
                adopt_attached_session(
                    store.as_ref(),
                    &mut record,
                    &mut resume,
                    &mut resume_pending,
                    &mut former_attachment,
                    attached_session.as_deref(),
                )?;
                apply_tui_handoff_after_host(
                    &paths,
                    store.as_ref(),
                    &mut record,
                    &mut resume,
                    &mut resume_pending,
                    &mut former_attachment,
                    handoff,
                    &mut pending_note,
                )?;
                tui_presentation_cache = None;
            }
            Ok(tui::TuiOutcome::Exited { .. }) => return Ok(ProcessOutcome::Success),
            Err(err) => return Err(CliFailure::new(err.to_string())),
        }
    }
}

fn tui_presentation_values(
    paths: &product_paths::PlakPaths,
) -> CliResult<(
    Vec<plakous_host_protocol::views::ProviderDef>,
    Vec<plakous_host_protocol::views::ProviderSection>,
)> {
    presentation::tui_presentation_values(paths)
}

#[cfg(test)]
fn tui_presentation_values_with_backend(
    paths: &product_paths::PlakPaths,
    backend: &mut impl PresentationBackend,
) -> (
    Vec<plakous_host_protocol::views::ProviderDef>,
    Vec<plakous_host_protocol::views::ProviderSection>,
) {
    presentation::tui_presentation_values_with_backend(paths, backend)
}

fn tui_presentation_bundle(paths: &product_paths::PlakPaths) -> CliResult<TuiPresentationBundle> {
    presentation::tui_presentation_bundle(paths)
}

/// Fallback rows used when the credential store cannot be read at all. Every
/// vertical reads as unready, so nothing is painted as connected.
fn supported_provider_rows() -> Vec<plakous_host_protocol::views::ProviderDef> {
    [
        (
            "codex",
            "Codex (ChatGPT)",
            plakous_host_protocol::views::AuthKind::Oauth,
        ),
        (
            "grok",
            "Grok (SuperGrok)",
            plakous_host_protocol::views::AuthKind::Oauth,
        ),
        (
            "claude",
            "Claude (Max)",
            plakous_host_protocol::views::AuthKind::Oauth,
        ),
        (
            "openai",
            "OpenAI",
            plakous_host_protocol::views::AuthKind::ApiKey,
        ),
        (
            "xai-api",
            "xAI",
            plakous_host_protocol::views::AuthKind::ApiKey,
        ),
        (
            "anthropic",
            "Anthropic",
            plakous_host_protocol::views::AuthKind::ApiKey,
        ),
        (
            "openrouter",
            "OpenRouter",
            plakous_host_protocol::views::AuthKind::ApiKey,
        ),
    ]
    .into_iter()
    .map(
        |(id, label, kind)| plakous_host_protocol::views::ProviderDef {
            id: id.to_owned(),
            label: label.to_owned(),
            kind,
            state: match kind {
                plakous_host_protocol::views::AuthKind::Oauth => {
                    plakous_host_protocol::views::ProviderState::NotLoggedIn
                }
                plakous_host_protocol::views::AuthKind::ApiKey => {
                    plakous_host_protocol::views::ProviderState::NoKey
                }
            },
            detail: String::new(),
            account_label: None,
        },
    )
    .collect()
}

fn handle_tui_handoff(
    paths: &product_paths::PlakPaths,
    store: &dyn store::session_records::SessionRecordStore,
    record: &mut product_sessions::SessionRecord,
    handoff: tui::TuiHandoff,
    reload_note: &mut Option<String>,
) -> CliResult<()> {
    match handoff {
        tui::TuiHandoff::EditConfig => {
            edit_config_file(paths)?;
        }
        tui::TuiHandoff::Reload { target } => {
            *reload_note = Some(reload_installed_plak(paths, store, record, target)?);
        }
        tui::TuiHandoff::ResumeSession { id } => {
            let next = store.load(&id).map_err(CliFailure::from)?;
            let requested = std::fs::canonicalize(&record.workspace_root)
                .unwrap_or_else(|_| record.workspace_root.clone());
            let recorded = std::fs::canonicalize(&next.workspace_root)
                .unwrap_or_else(|_| next.workspace_root.clone());
            if requested != recorded {
                return Err(CliFailure::new(
                    "selected session belongs to a different workspace",
                ));
            }
            *record = next;
        }
    }
    Ok(())
}

/// Replace this process with the installed `plak` resuming `record`. The host
/// worker has already released the session and the terminal is restored, so
/// the new process attaches exactly as `plak --resume <id>` would. A shell that
/// never committed a session reopens fresh.
///
/// A build with another state key cannot open this build's state, so the
/// session is exported here and the new build imports it
/// (`--import-resume`). When the export is refused nothing is exec'd: the
/// returned note says why and the session stays open in this process. Exec
/// failure is the error.
fn reload_installed_plak(
    paths: &product_paths::PlakPaths,
    store: &dyn store::session_records::SessionRecordStore,
    record: &mut product_sessions::SessionRecord,
    target: Option<carry::ReloadTarget>,
) -> CliResult<String> {
    let Some(target) = target else {
        return Err(CliFailure::new("reload: no installed plak found on PATH"));
    };
    if let Some(reason) = target.refusal(&home::state::state_key()) {
        return Ok(reason);
    }
    let executable = target.executable;
    let installed_key = target.state_key;
    // The loop's copy can trail the store: the host commits every turn's
    // head itself, and an export of a stale record is refused as behind it.
    let stored = store.load(&record.id).ok();
    let resumable = stored.is_some();
    if let Some(stored) = stored {
        *record = stored;
    }
    let mut command = std::process::Command::new(&executable);
    std::os::unix::process::CommandExt::arg0(&mut command, build_identity::invocation_argv0());
    let mut carry_file = None;
    let mut unverified = None;
    match carry::reload_plan(
        resumable,
        &home::state::state_key(),
        installed_key.as_deref(),
    ) {
        carry::ReloadPlan::Fresh => {}
        carry::ReloadPlan::Resume => {
            command.arg("--resume").arg(&record.id);
        }
        carry::ReloadPlan::ResumeUnverified => {
            unverified = Some(format!(
                "; {} did not report its state key, so {} was resumed without carrying it forward",
                executable.display(),
                record.id
            ));
            command.arg("--resume").arg(&record.id);
        }
        carry::ReloadPlan::Carry => {
            let path = match carry_out(paths, record) {
                Ok(path) => path,
                Err(reason) => {
                    return Ok(format!(
                        "reload kept this session open: the new build reads another state format, and {reason}"
                    ));
                }
            };
            command.arg("--import-resume").arg(&path);
            carry_file = Some(path);
        }
    }
    tracing::info!(
        target: "plak.reload",
        executable = %executable.display(),
        session = %record.id,
        carried = carry_file.is_some(),
        "exec installed plak"
    );
    let error = std::os::unix::process::CommandExt::exec(&mut command);
    if resumable {
        let _ = writeln!(
            io::stdout(),
            "{}",
            resume_exit_line(&record.id, &record.workspace_root)
        );
    }
    let kept = carry_file
        .map(|path| format!("; the session export is kept at {}", path.display()))
        .unwrap_or_default();
    Err(CliFailure::new(format!(
        "reload: could not start {}: {error}{kept}{}",
        executable.display(),
        unverified.unwrap_or_default()
    )))
}

/// Exports `record` into a fresh private carry file and returns its path.
fn carry_out(
    paths: &product_paths::PlakPaths,
    record: &product_sessions::SessionRecord,
) -> Result<PathBuf, String> {
    let document = carry::export_session(paths, record)
        .map_err(|refusal| format!("session {} cannot be exported: {refusal}", record.id))?;
    let path = carry::new_carry_file(paths, &record.id)?;
    carry::write_document(&path, &document)?;
    Ok(path)
}

/// `plak --resume` of a session another build's state holds. When a kept
/// maintainer snapshot of that build exists, it exports the session and this
/// build imports it as a new session; the new id and its note are returned.
/// Without one the caller's refusal stands.
fn carry_foreign_session(
    services: &Services,
    paths: &product_paths::PlakPaths,
    cwd: &Path,
    requested: &str,
) -> CliResult<Option<(String, String)>> {
    let Some((key, id)) = home::state::foreign_session(paths, requested) else {
        return Ok(None);
    };
    if let Some(existing) =
        carry::already_carried(services.session_records()?.as_ref(), cwd, &id, &key)
            .map_err(CliFailure::new)?
    {
        let note = format!(
            "session {id} ({key}) was already carried forward as {}; resuming it",
            existing.id
        );
        return Ok(Some((existing.id, note)));
    }
    let Some(snapshot) = carry::snapshot_for_key(&home::paths::installed_snapshot_roots(), &key)
    else {
        return Ok(None);
    };
    let workspace_paths = product_paths::PlakPaths::for_workspace(paths.home().to_path_buf(), cwd);
    product_config::SessionPolicy::load(&workspace_paths).map_err(|error| {
        CliFailure::new(format!(
            "could not carry session {id} forward: {error}; fix the config and retry the same command"
        ))
    })?;
    let export_version = carry::snapshot_export_version(&snapshot).map_err(|error| {
        CliFailure::new(format!(
            "could not carry session {id} forward: {error}; fix the error and retry the same command"
        ))
    })?;
    if let Some(version) = export_version
        && !carry::IMPORTABLE_VERSIONS.contains(&version)
    {
        return Err(CliFailure::new(format!(
            "could not carry session {id} forward: the kept snapshot writes version {version} session exports; this build imports versions {:?}; use this workspace's newest carried session from `plak session list`",
            carry::IMPORTABLE_VERSIONS
        )));
    }
    let out = carry::new_carry_file(paths, &id).map_err(CliFailure::new)?;
    let result = carry::export_with_snapshot(&snapshot, &id, &out, cwd)
        .map_err(CliFailure::new)
        .and_then(|()| {
            carry::read_document(&out).map_err(|error| {
                CliFailure::new(format!(
                    "{error}; use this workspace's newest carried session from `plak session list`"
                ))
            })?;
            commands::session::import_session(services, &out).map_err(|failure| {
                CliFailure::new(format!(
                    "{}; fix the error and retry the same command",
                    failure.message()
                ))
            })
        });
    let (record, note) = result.map_err(|failure| {
        let cleanup = match std::fs::remove_file(&out) {
            Ok(()) => String::new(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
            Err(error) => format!("; could not delete {}: {error}", out.display()),
        };
        CliFailure::new(format!(
            "could not carry session {id} forward: {}{cleanup}",
            failure.message()
        ))
    })?;
    let note = match std::fs::remove_file(&out) {
        Ok(()) => note,
        Err(error) => format!("{note}; could not delete {}: {error}", out.display()),
    };
    Ok(Some((record.id, note)))
}

/// `plak --import-resume <file>`: the new build's half of a carrying
/// `/reload`. The file is deleted once the import committed and kept, with
/// its path named, when it did not.
fn import_and_resume(services: &Services, path: &Path) -> CliResult<ProcessOutcome> {
    let document = carry::read_document(path).map_err(|failure| {
        CliFailure::new(format!(
            "could not carry the session forward: {failure}; the export is kept at {}",
            path.display()
        ))
    })?;
    let source_id = document.source().0.to_owned();
    let paths = product_paths::PlakPaths::for_workspace(
        services.paths()?.home().to_path_buf(),
        document.workspace_root(),
    );
    product_config::SessionPolicy::load(&paths).map_err(CliFailure::new)?;
    if !rustix::termios::isatty(io::stdin()) || !io::stdout().is_terminal() {
        return Err(CliFailure::classified(
            CliFailureKind::Usage,
            "the interactive TUI needs a terminal; import was refused before creating a successor",
        ));
    }
    let cwd = env::current_dir().map_err(|_| CliFailure::new("cwd unavailable"))?;
    let requested = std::fs::canonicalize(&cwd).unwrap_or(cwd);
    let recorded = std::fs::canonicalize(document.workspace_root())
        .unwrap_or_else(|_| document.workspace_root().to_path_buf());
    if requested != recorded {
        return Err(CliFailure::new(format!(
            "session {source_id} belongs to {}; import-resume from {} was refused before creating a successor",
            recorded.display(),
            requested.display()
        )));
    }
    home::state::ensure_usable_state(services.paths()?, &mut io::stderr().lock())
        .map_err(CliFailure::new)?;
    let (record, note) = commands::session::import_session(services, path).map_err(|failure| {
        CliFailure::new(format!(
            "could not carry the session forward: {}; the export is kept at {}",
            failure.message(),
            path.display()
        ))
    })?;
    let note = match std::fs::remove_file(path) {
        Ok(()) => note,
        Err(error) => format!("{note}; could not delete {}: {error}", path.display()),
    };
    let successor = record.id;
    launch_tui(services, Some(successor.clone()), false, Some(note))
        .map_err(|failure| import_resume_failure(failure, &successor, &source_id))
}

fn import_resume_failure(failure: CliFailure, successor: &str, source: &str) -> CliFailure {
    CliFailure::new(format!(
        "{}; session {successor} was carried forward from {source}; fix the error and run: plak --resume {successor}",
        failure.message()
    ))
}

#[cfg(test)]
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

#[cfg(test)]
#[test]
fn product_gate_allows_inference() {
    let limits = product_subscription_request_limits()
        .unwrap_or_else(|()| panic!("product subscription request limits must be valid"));
    assert!(
        limits.allowance(plakous::subscriptions::SubscriptionRequestClass::Inference) > 0,
        "product turns must not use catalog-only inference=0 gates"
    );
}
fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
    cli::parse(arguments)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use std::io::Cursor;

    use super::*;

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
                "native credential profile changed; run `plak logout codex --local`, then `plak login codex`",
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
        assert!(stdout.is_empty());
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
        assert_eq!(
            cell,
            "Profile changed · plak logout claude --local, then plak login claude"
        );
        let long = native_subscription_failure(
            NativeSubscriptionError::CredentialProfileChanged,
            Some(NativeProvider::Claude),
            state,
        );
        assert!(long.message().contains("plak logout claude --local"));
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
            br#"{"format_version":2,"profile_id":"previous-claude-profile","slot_id":"primary","account_binding":"old-claude-grant","generation":1,"status":"ready","payload":"e30="}"#,
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
            claude.contains("Profile changed · plak logout claude --local, then plak login claude"),
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
        let receipt =
            std::fs::read_to_string(subscriptions.join("compatibility/codex.json")).unwrap();
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
        Revoke(NativeProvider),
        LogoutLocal(NativeProvider),
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
        completed_login_modes: Vec<NativeLoginMode>,
        compatibility: CompatibilityState,
        credential: CredentialState,
        terminal: CredentialState,
        calls: Vec<Call>,
        model_summaries: Vec<CliModelSummary>,
        models_failure: Option<CliFailure>,
        openrouter_key_usage: Option<product_usage::VerticalObservation>,
    }

    impl FakeBackend {
        fn new(compatibility: CompatibilityState, terminal: CredentialState) -> Self {
            Self {
                login_completion: None,
                login_outcome: None,
                login_failure: None,
                login_begin_failure: None,
                login_input: None,
                completed_login_modes: Vec::new(),
                compatibility,
                credential: CredentialState::SignedOut,
                terminal,
                calls: Vec::new(),
                model_summaries: Vec::new(),
                models_failure: None,
                openrouter_key_usage: None,
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

        fn subscription_usage(
            &mut self,
            provider: NativeProvider,
        ) -> CliResult<SubscriptionUsageRead> {
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
            Ok(commands::application::LoginChallenge {
                pending: FakePendingLogin(mode),
                authorize_url: "https://auth.invalid/authorize?fixture=1".to_owned(),
                completion: self
                    .login_completion
                    .unwrap_or(if mode == NativeLoginMode::Browser {
                        plakous::subscriptions::NativeLoginCompletion::Automatic
                    } else {
                        plakous::subscriptions::NativeLoginCompletion::RedirectUrl
                    }),
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
                    assert_eq!(timeout, oauth_ui::LOCAL_HARD_TIMEOUT);
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

        fn revoke(&mut self, provider: NativeProvider) -> CliResult<CredentialState> {
            self.calls.push(Call::Revoke(provider));
            Ok(self.terminal)
        }

        fn logout_local(&mut self, provider: NativeProvider) -> CliResult<CredentialState> {
            self.calls.push(Call::LogoutLocal(provider));
            Ok(self.terminal)
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
                    assert!(entry.extras.is_empty());
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
            assert!(sections.is_empty());
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
        assert!(
            product_session_log::read_tail(&paths.session_home(), &id, 8)
                .expect("missing observation log is empty")
                .is_empty()
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
            let paths =
                product_paths::PlakPaths::explicit(root.path().join(format!("home-{index}")));
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
    fn root_config(
        account: &str,
        model: &str,
        effort: Option<&str>,
    ) -> product_config::ProductConfig {
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
        assert!(deferred_attach_refusal_reason(eligibility, false).starts_with("resolve_failed:"));
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
            rollback.as_ref().map(|(record, resume, pending)| (
                record.id.as_str(),
                *resume,
                *pending
            )),
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
        product_sessions::save_session(&paths, &mut mismatched)
            .expect("persist mismatched session");
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
        assert!(sections.is_empty());
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
        assert!(providers[0].detail.is_empty());
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
        assert!(sections[1].entries[0].effort_options.is_empty());
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
                native_tool_mode: (exact_id == "gpt-5.6-sol")
                    .then_some(NativeToolMode::CodeModeOnly),
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

        let mut signed_out =
            FakeBackend::new(CompatibilityState::Current, CredentialState::SignedOut);
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

            write_selected_subscription_usage(Some(account), &mut backend, None, &mut stdout)
                .unwrap();

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
        assert!(backend.calls.is_empty());
        assert!(stdout.is_empty());
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
        let error = combine_cleanup(Err(CliFailure::new("release checkpoint refused")), Ok(()))
            .unwrap_err();
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
        join_host_worker_bounded(thread::spawn(|| Ok(())), Duration::from_secs(1), "timely")
            .unwrap();

        let worker = thread::spawn(move || {
            thread::sleep(Duration::from_secs(5));
            Ok(())
        });
        let started = Instant::now();
        let error =
            join_host_worker_bounded(worker, Duration::from_millis(5), "stuck").unwrap_err();
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
                    mode: NativeLoginMode::RemoteBrowser,
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
                mode: NativeLoginMode::RemoteBrowser,
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
                prompt: "hello world".to_owned(),
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
                prompt: "hello world".to_owned(),
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
                prompt: "research me".to_owned(),
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
                prompt: "spark me".to_owned(),
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
                prompt: "patch me".to_owned(),
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
                    "reviewed_at=1791072000\n",
                    "expires_at=1793750399\n",
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
                    mode: NativeLoginMode::RemoteBrowser,
                },
                Call::Login(NativeProvider::Xai, NativeLoginMode::RemoteBrowser),
                CredentialState::Ready,
            ),
            (
                Command::AuthRefresh(NativeProvider::Codex),
                Call::Refresh(NativeProvider::Codex),
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
                Call::Login(provider, _) | Call::Refresh(provider) | Call::Revoke(provider) => {
                    *provider
                }
                Call::Status(_)
                | Call::Usage(_)
                | Call::Acknowledge(_, _)
                | Call::LogoutLocal(_)
                | Call::Models(_)
                | Call::OpenRouterModels => {
                    panic!("test case must end in a network action")
                }
            };
            let mut backend = FakeBackend::new(CompatibilityState::Required, terminal);
            let (result, _, _) =
                invoke(command, &mut backend, b"exact-review\r\nsynthetic-code\r\n");
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
            assert!(stdout.is_empty());
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
            let mut backend =
                FakeBackend::new(CompatibilityState::Required, CredentialState::Ready);
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

            let (result, stdout, stderr) =
                invoke(Command::Models(provider), &mut backend, b"yes\r\n");

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
        assert!(stdout.is_empty());
        assert_eq!(inactive.calls, vec![Call::Status(NativeProvider::Codex)]);

        let mut rejected = FakeBackend::new(CompatibilityState::Required, CredentialState::Ready);
        rejected.credential = CredentialState::Ready;
        let (result, stdout, stderr) = invoke(
            Command::Models(NativeProvider::Xai),
            &mut rejected,
            b"not-exact\n",
        );
        assert!(result.is_err());
        assert!(stdout.is_empty());
        assert_eq!(rejected.calls, vec![Call::Status(NativeProvider::Xai)]);
        let stderr = String::from_utf8(stderr).unwrap_or_default();
        assert!(stderr.contains("Continue? [y/N]"));
        assert!(!stderr.contains("not-exact"));
    }

    #[test]
    fn openrouter_models_dispatches_only_catalog_and_failure_emits_no_listing() {
        let mut backend =
            FakeBackend::new(CompatibilityState::Required, CredentialState::SignedOut);
        let (result, stdout, stderr) = invoke(Command::ModelsOpenRouter, &mut backend, b"");
        assert_eq!(result, Ok(ProcessOutcome::Success));
        assert_eq!(stdout, b"author/model\n");
        assert!(stderr.is_empty());
        assert_eq!(backend.calls, vec![Call::OpenRouterModels]);
        backend.calls.clear();
        backend.models_failure = Some(CliFailure::new("catalog failed"));
        let (result, stdout, stderr) = invoke(Command::ModelsOpenRouter, &mut backend, b"");
        assert!(result.is_err());
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
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
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
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
            b"grok: compatibility=acceptance-required, credential=refresh-required; run plak login grok\n"
        );
        assert!(stderr.is_empty());

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
        assert!(stdout.is_empty());
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
            assert!(stdout.is_empty());
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
        assert!(output.is_empty());
        assert!(!paths.product_database().exists());
        assert!(!paths.runtime_database().exists());
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
    fn pasted_login_uses_the_challenge_completion_and_never_echoes_the_response() {
        for (provider, mode, completion, browser_count) in [
            (
                NativeProvider::Xai,
                NativeLoginMode::RemoteBrowser,
                plakous::subscriptions::NativeLoginCompletion::RedirectUrl,
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
        assert!(ui.presented_urls.is_empty());
        assert!(backend.completed_login_modes.is_empty());

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
        assert!(backend.completed_login_modes.is_empty());

        let mut backend = FakeBackend::new(CompatibilityState::Current, CredentialState::Ready);
        let mut ui = commands::login_flow::TestLoginUi::default();
        assert!(
            auth_login(
                &mut backend,
                NativeProvider::Codex,
                NativeLoginMode::RemoteBrowser,
                &mut Cursor::new(b"\n"),
                &mut Vec::new(),
                &mut ui
            )
            .is_err()
        );
        assert_eq!(
            backend.calls,
            [
                Call::Status(NativeProvider::Codex),
                Call::Login(NativeProvider::Codex, NativeLoginMode::RemoteBrowser)
            ]
        );
        assert!(backend.completed_login_modes.is_empty());
        assert!(ui.browser_urls.is_empty());
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
                assert!(stdout.is_empty());
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
                Some(
                    "local logout did not reach a safely retired local credential state".to_owned()
                )
            );
        }

        let mut remote = FakeBackend::new(CompatibilityState::Current, CredentialState::Revoked);
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
}

mod platform_catalog;
mod platform_product;
