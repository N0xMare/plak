//! Plak product application library (CLI workflows over the public `plakous` facade).
//!
//! The `plak` binary is a thin entrypoint; product logic and clap-backed parsing live here.

#![allow(
    clippy::doc_markdown,
    clippy::missing_errors_doc,
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
mod agent_wire;
#[doc(hidden)]
pub mod agents_projection;
mod build_features;
mod build_identity;
mod capabilities;
mod carry;
mod cli;
pub(crate) mod cli_output;
mod command;
#[doc(hidden)]
pub mod commands;
#[cfg(test)]
mod composition_tests;
#[cfg(feature = "tui")]
mod config;
mod diagnostics;
mod dispatch;
mod environment;
mod host_status_text;
mod log_text;
mod lookup_status;
mod notice_text;
mod refusal_text;
mod review_expiry;
pub(crate) mod surface_protocol;
/// Terminal-only application adapters.
pub(crate) mod terminal_application;
#[allow(
    dead_code,
    reason = "host terminal authority also compiles without the web transport"
)]
mod terminal_registry;
pub(crate) mod tui_preferences;
mod usage_remedy;
pub mod wire_transport;
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
mod open_file_limit;
#[doc(hidden)]
pub mod openrouter_product;
#[doc(hidden)]
pub mod presentation;
#[doc(hidden)]
pub mod product_config;
mod profiles;
#[doc(hidden)]
pub mod prompts;
#[cfg_attr(
    not(feature = "tui"),
    allow(
        dead_code,
        reason = "the interactive host is the request channel's reader"
    )
)]
mod reload_request;
#[doc(hidden)]
#[doc(hidden)]
pub mod roster_input;
#[doc(hidden)]
pub mod sidecars;
pub mod signals;
mod storage_space;
#[doc(hidden)]
pub mod store;
#[cfg(test)]
mod wire_profile_reply_tests;
/// Product telemetry: where it is written, what emits it, and the topology
/// projection over it.
#[doc(hidden)]
pub mod telemetry {
    pub mod topology;
}
mod observation_projection;
#[cfg(feature = "otel")]
mod otel_bridge;
#[cfg(all(test, unix))]
mod product_compaction_runtime_e2e_tests;
#[cfg(all(test, unix, feature = "tui"))]
mod product_compaction_terminal_e2e_tests;
#[cfg(test)]
mod product_crash_tests;
#[doc(hidden)]
pub mod product_doctor;
#[cfg(any(feature = "tui", feature = "web"))]
mod product_onboarding;
#[cfg(all(test, unix))]
mod product_runtime_e2e_tests;
mod product_subscription_usage;
mod product_telemetry;
#[cfg(all(test, unix, feature = "tui"))]
mod product_terminal_e2e_tests;
#[cfg(any(test, feature = "test-hooks"))]
mod profile_test_catalog;
#[cfg(any(test, feature = "tui", feature = "test-hooks"))]
mod reload_probe_latency;
#[doc(hidden)]
pub mod shell;
#[doc(hidden)]
pub mod telemetry_sink;
#[doc(hidden)]
pub mod telemetry_store;
#[cfg(feature = "test-hooks")]
mod test_hooks;
#[cfg(not(feature = "test-hooks"))]
mod test_hooks {
    pub(crate) const fn binding_executor_cut() -> bool {
        false
    }
    pub(crate) fn upgrade_state_key() -> Option<String> {
        None
    }
    pub(crate) fn upgrade_input_limit() -> Option<String> {
        None
    }
    pub(crate) const fn upgrade_import_refused() -> bool {
        false
    }
    #[allow(
        dead_code,
        reason = "only the optional host can exercise same-image handoff"
    )]
    pub(crate) const fn force_host_upgrade() -> bool {
        false
    }
    #[allow(dead_code, reason = "only the optional host coordinates upgrades")]
    pub(crate) fn upgrade_crash_cut(_name: &str) {}
    #[allow(dead_code, reason = "only the optional host prepares upgrades")]
    pub(crate) const fn upgrade_preparation_refused(_name: &str) -> bool {
        false
    }
    #[allow(dead_code, reason = "only the optional host starts replacement images")]
    pub(crate) fn upgrade_key_environment(
        _key: &str,
    ) -> impl Iterator<Item = (&'static str, String)> {
        std::iter::empty()
    }
    pub(crate) const fn profile_route_ready(
        _catalog: &[plakous_host_protocol::views::ProviderSection],
        _account: &str,
        _model: &str,
    ) -> Option<bool> {
        None
    }
    pub(crate) fn refresh_profile_ready_catalog(
        sections: Vec<plakous_host_protocol::views::ProviderSection>,
    ) -> Vec<plakous_host_protocol::views::ProviderSection> {
        sections
    }

    #[allow(dead_code, reason = "only builds with the host select startup inputs")]
    pub(crate) fn host_start_environment() -> Vec<(&'static str, std::ffi::OsString)> {
        Vec::new()
    }
    #[allow(dead_code, reason = "only the optional host dials its local runner")]
    pub(crate) fn runner_windows() -> plakous::runner::ClientWindows {
        plakous::runner::ClientWindows::default()
    }
    pub(crate) fn runner_before_ready(
        _client: &plakous::runner::RunnerClient,
    ) -> std::future::Ready<Result<(), plakous::runner::PlacementFailure>> {
        std::future::ready(Ok(()))
    }
    pub(crate) fn runner_ready_result(_result: &Result<(), plakous::runner::PlacementFailure>) {}
    pub(crate) fn runner_link(
        client: plakous::runner::RunnerClient,
    ) -> std::sync::Arc<dyn plakous::tools::runner::RunnerLink> {
        std::sync::Arc::new(client)
    }
    pub(crate) fn runner_lease_clock(
        _home: &std::path::Path,
    ) -> Option<std::sync::Arc<plakous::runner::RunnerLeaseClock>> {
        None
    }
    pub(crate) const fn runner_services() -> Option<plakous::runner::RunnerServices> {
        None
    }
    #[allow(dead_code, reason = "headless builds do not start hosts")]
    pub(crate) const fn runner_detached() -> bool {
        false
    }
    #[allow(dead_code, reason = "headless builds do not start hosts")]
    pub(crate) const fn host_idle_exit_seconds() -> Option<std::ffi::OsString> {
        None
    }
}
#[cfg(test)]
mod test_support;
#[doc(hidden)]
pub mod topology_product;
#[cfg(test)]
mod topology_resources_projection;
#[cfg(feature = "tui")]
mod tui;
#[cfg(feature = "tui")]
mod tui_launch;
#[cfg(not(feature = "tui"))]
#[path = "tui_unavailable.rs"]
mod tui_launch;
#[cfg(feature = "tui")]
mod tui_timing;
#[cfg(not(feature = "tui"))]
#[path = "tui_timing_unavailable.rs"]
mod tui_timing;
#[cfg(feature = "tui")]
mod ui_wake {
    pub(super) use crate::tui::wake::wake;
}
#[cfg(not(feature = "tui"))]
mod ui_wake {
    pub(super) fn wake() {}
}
#[cfg(feature = "web")]
mod host;
#[cfg(not(feature = "web"))]
#[path = "host_unavailable.rs"]
mod host;
#[cfg(feature = "web")]
mod web;
#[cfg(all(feature = "web", any(feature = "maintainer", feature = "test-hooks")))]
#[path = "web/dev_login.rs"]
mod web_dev_login;
#[cfg(all(
    feature = "web",
    not(any(feature = "maintainer", feature = "test-hooks"))
))]
#[path = "web/dev_login_unavailable.rs"]
mod web_dev_login;
mod wire_schema;

/// Runs the repository-only TUI rendering benchmark.
#[cfg(all(feature = "test-support", feature = "tui"))]
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
pub(crate) use crate::presentation::CompatibilityState;
pub(crate) use crate::presentation::CredentialState;
#[cfg(test)]
pub(crate) use crate::presentation::PresentationBackend;
#[cfg(feature = "tui")]
use crate::presentation::TuiPresentationBundle;
#[cfg(any(feature = "tui", test))]
use crate::store::sessions as product_sessions;
pub(crate) use plakous_hosted::agent as product_agent;
pub(crate) use plakous_hosted::error::{CliFailure, CliFailureKind, CliResult, ProcessOutcome};
pub(crate) use plakous_hosted::runtime as product_host;
pub(crate) use plakous_hosted::session_log as product_session_log;
#[cfg(test)]
pub(crate) use plakous_hosted::subscription_usage as product_usage;

use crate::product_config::ConfigScope;
#[cfg(test)]
use plakous::subscriptions::NativeProvider;
use std::env;
use std::ffi::OsString;
use std::io::{self, Write as _};
#[cfg(feature = "test-support")]
use std::path::Path;
use std::process::ExitCode;
#[cfg(feature = "otel")]
use std::sync::Arc;

use crate::command::{Command, command_is_read_only, command_needs_product_state, runs_a_cli_turn};
use crate::commands::agent::run_agent;
use crate::commands::application::{AccountOps, PlakousApplication};
use crate::commands::catalog::list_models;
use crate::commands::doctor::run_doctor;
use crate::commands::login::{
    auth_login, auth_logout, auth_logout_local, auth_refresh, auth_status, key_clear, key_set,
    key_status, print_compatibility_notice,
};
use crate::commands::session::{
    confirm_session_mutation, load_admitted_product_session, run_attached_action,
    show_child_recovery_status, show_recovery_status,
};
use crate::commands::support::edit_config_file;
#[cfg(test)]
use crate::commands::support::{api_key_status_failure_cell, status_credential_cell};
use crate::commands::usage::{collect_usage_limits, usage_vertical_for_account};
use crate::presentation::{
    format_unix_seconds, product_account_label, title_case_token, transcript_entry,
};
use crate::shell::services::Services;
use crate::tui_launch::launch_tui;

/// Run the Plak CLI using process arguments from the environment.
#[must_use]
pub fn main_from_env() -> ExitCode {
    tui_timing::process_started();
    open_file_limit::raise();
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
/// Exit codes (spec 02 §3.3): 0 ok, 2 usage, 3 auth, 4 cancel, 5 agent,
/// 6 run bound, 130 escalated quit, else 1.
#[must_use]
pub fn run_from_args(arguments: impl IntoIterator<Item = OsString>) -> ExitCode {
    match run(arguments) {
        Ok(ProcessOutcome::Success) => ExitCode::SUCCESS,
        Ok(ProcessOutcome::Interrupted) => ExitCode::from(4),
        Ok(ProcessOutcome::EscalatedQuit) => ExitCode::from(130),
        Ok(ProcessOutcome::Bounded) => ExitCode::from(6),
        Err(failure) => {
            eprintln!(
                "plak: {}",
                cli_output::safe_terminal_block(&refusal_text::failure_text(
                    &failure,
                    notice_text::RemedyContext {
                        frontend: notice_text::Frontend::Run,
                        session_id: None
                    }
                ))
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
        Command::StateKey
        | Command::HostUpgradeProbe { .. }
        | Command::HostUpgradeCapacity { .. }
        | Command::ReloadProbe
        | Command::ReopenProbe { .. }
        | Command::WriteBinaryManifest { .. }
        | Command::CarryProbe { .. }
        | Command::ExportVersion => {}
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

#[allow(
    clippy::too_many_lines,
    reason = "The composition root orders read-only shortcuts, signal installation, service construction, product-state admission, and face dispatch."
)]
fn run(arguments: impl IntoIterator<Item = OsString>) -> CliResult<ProcessOutcome> {
    // The launch directory, read once: the CLI's workspace, and what a
    // relative path on the command line means. Nothing below resolves a
    // path against the process working directory.
    let launch = shell::home::launch_directory();
    let arguments: Vec<_> = arguments.into_iter().collect();
    #[cfg(feature = "test-hooks")]
    {
        if let Some(result) =
            test_hooks::client_fixtures::seed_request(&arguments, launch.as_deref())
        {
            return result;
        }
        test_hooks::install().map_err(CliFailure::new)?;
    }
    let command = cli::parse(arguments)?.resolved_against(launch.as_deref());
    build_features::admit_command(&command)?;

    if matches!(command, Command::WireSchema) {
        return wire_schema::print(&mut io::stdout().lock());
    }

    if let Command::Runner(commands::runner::RunnerCommand::Connect {
        binary: Some(binary),
    }) = &command
    {
        return commands::runner::connect_binary(binary);
    }

    if let Command::WriteBinaryManifest { directory } = &command {
        build_identity::write_binary_manifest(directory).map_err(|error| {
            CliFailure::new(format!("could not write binary manifest: {error}"))
        })?;
        return Ok(ProcessOutcome::Success);
    }

    let prepared_import =
        if let Command::SessionImport { path } | Command::CarryProbe { path } = &command {
            Some(commands::session::PreparedImport::new(
                carry::read_document(path).map_err(|e| CliFailure::new(e.to_string()))?,
                false,
            )?)
        } else {
            None
        };

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
    let services = Services::from_env(Some(otel_extra_telemetry_sink), launch.as_deref());
    #[cfg(not(feature = "otel"))]
    let services = Services::from_env(None, launch.as_deref());
    if matches!(&command, Command::SessionCheckBinding) {
        return commands::session::check_binding::run(services.paths()?, &mut io::stdout().lock());
    }
    if matches!(
        command,
        Command::HostUpgradeProbe { .. } | Command::HostUpgradeCapacity { .. }
    ) {
        let capacity = match &command {
            Command::HostUpgradeCapacity { path } => Some(path.as_path()),
            _ => None,
        };
        let service = match &command {
            Command::HostUpgradeProbe { service } => service.as_deref(),
            _ => None,
        };
        return carry::upgrade::print_probe(&services, capacity, service, &mut io::stdout().lock());
    }
    if let Command::HostInstall(install) = &command {
        return commands::host_install::run(install, services.paths(), &mut io::stdout().lock());
    }
    let _upgrade_admission = if (command_needs_product_state(&command)
        && !matches!(command, Command::Host(_)))
        || matches!(
            command,
            Command::StatePruneLegacy { .. } | Command::TreeRelease { .. }
        ) {
        services
            .paths()?
            .ensure_private_state_home()
            .map_err(CliFailure::new)?;
        Some(carry::upgrade::admission(services.paths()?, false)?)
    } else {
        None
    };
    // Diagnostic setup must not recreate a local daemon's deleted home.
    if matches!(
        &command,
        Command::Runner(commands::runner::RunnerCommand::ServeLocal)
    ) && !std::fs::symlink_metadata(services.paths()?.home()).is_ok_and(|home| home.is_dir())
    {
        return Err(CliFailure::new(
            "the home-local runner's product home no longer exists",
        ));
    }
    // Only a command that uses product state records a fresh binary id in the
    // home; inspection commands read the identity cache and never write it.
    if command_needs_product_state(&command)
        || matches!(
            command,
            Command::Tui { .. }
                | Command::TuiPlacement { .. }
                | Command::Resume { .. }
                | Command::HostResume { .. }
        )
    {
        build_identity::allow_identity_cache_writes();
    }

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
        Command::TuiPlacement { runner, workspace } => {
            return tui_launch::launch_placed_tui(
                &services,
                runner.as_deref(),
                workspace.as_deref(),
            );
        }
        Command::HostResume { session_id } => {
            return tui_launch::launch_host_tui(&services, session_id);
        }
        Command::Resume {
            session_id: None, ..
        } => return launch_tui(&services, None, true, None),
        Command::Resume {
            session_id: Some(id),
            handoff,
        } => {
            carry::take_reload_handoff(&services, handoff.as_deref());
            return launch_tui(&services, Some(id), false, None);
        }
        Command::ImportResume { path, handoff } => {
            return carry::import_and_resume(&services, &path, handoff.as_deref());
        }
        command @ Command::Agent { .. } => return run_agent_command(&services, command),
        Command::Host(command) => return host::run_host_command(&services, &command),
        Command::HostConnect { auto_start } => return host::connect_stdio(&services, auto_start),
        Command::HostUpgrade {
            expect_binary,
            abandon,
            wait,
            drain,
        } => {
            return host::upgrade(
                &services,
                expect_binary.as_deref(),
                wait,
                drain.unwrap_or_default(),
                abandon.as_deref(),
            );
        }
        // The runner bridge owns raw stdin and stdout, so it runs before the
        // process takes their locks.
        Command::Runner(command) => {
            return commands::runner::run(services.paths()?, command, &mut io::stdout());
        }
        command => command,
    };

    if let Some(prepared) = prepared_import {
        if command_needs_product_state(&command) {
            home::state::ensure_usable_state(services.paths()?, &mut io::stderr().lock())
                .map_err(CliFailure::new)?;
        }
        if matches!(&command, Command::CarryProbe { .. }) {
            let capacity = commands::session::probe_prepared_capacity(&services, prepared)?;
            serde_json::to_writer(io::stdout(), &capacity)
                .map_err(|_| CliFailure::new("stdout failed"))?;
            writeln!(io::stdout()).map_err(|_| CliFailure::new("stdout failed"))?;
        } else {
            let (record, note) = commands::session::import_prepared(&services, prepared)?;
            writeln!(
                io::stdout(),
                "{}\nresume with: plak --resume {}",
                cli_output::terminal_block(&note, cli_output::TerminalMode::of_stdout(), None),
                record.id
            )
            .map_err(|_| CliFailure::new("stdout failed"))?;
        }
        return Ok(ProcessOutcome::Success);
    }
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
        Command::CarryProbe { path } => {
            let capacity = commands::session::probe_carry_capacity(&services, &path)?;
            serde_json::to_writer(&mut stdout, &capacity)
                .map_err(|_| CliFailure::new("stdout failed"))?;
            writeln!(stdout).map_err(|_| CliFailure::new("stdout failed"))?;
            Ok(ProcessOutcome::Success)
        }
        Command::ReloadProbe => {
            let verdict = match services.paths() {
                Ok(paths) => carry::reload_probe(paths),
                Err(error) => carry::ReloadProbe {
                    host_resume: tui_launch::HOST_RESUME_SUPPORTED,
                    state_key: home::state::state_key(),
                    export_versions: carry::IMPORTABLE_VERSIONS.to_vec(),
                    config_error: Some(error.message().to_owned()),
                    binary_id: build_identity::known_binary_id(),
                },
            };
            serde_json::to_writer(&mut stdout, &verdict)
                .map_err(|_| CliFailure::new("stdout failed"))?;
            writeln!(stdout).map_err(|_| CliFailure::new("stdout failed"))?;
            Ok(ProcessOutcome::Success)
        }
        Command::ReopenProbe {
            session,
            host_upgrade_reopen,
        } => {
            let verdict = carry::reopen_probe(&services, &session, host_upgrade_reopen);
            serde_json::to_writer(&mut stdout, &verdict)
                .map_err(|_| CliFailure::new("stdout failed"))?;
            writeln!(stdout).map_err(|_| CliFailure::new("stdout failed"))?;
            Ok(ProcessOutcome::Success)
        }
        Command::ExportVersion => {
            writeln!(stdout, "{}", carry::VERSION).map_err(|_| CliFailure::new("stdout failed"))?;
            Ok(ProcessOutcome::Success)
        }
        Command::WriteBinaryManifest { .. }
        | Command::HostUpgradeProbe { .. }
        | Command::HostUpgradeCapacity { .. } => {
            Err(CliFailure::new("internal command dispatch error"))
        }
        Command::StateKey => {
            writeln!(stdout, "{}", home::state::state_key())
                .map_err(|_| CliFailure::new("stdout failed"))?;
            Ok(ProcessOutcome::Success)
        }
        Command::CompatibilityNotice(provider) => {
            print_compatibility_notice(
                &commands::support::cli_provider_name(provider).parse()?,
                &mut stdout,
            )?;
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
        Command::TreeRelease { tree_id, force } => {
            if !force {
                return Err(CliFailure::classified(
                    CliFailureKind::Usage,
                    "plak tree release releases another process's custody without it; pass --force to confirm",
                ));
            }
            commands::tree::release(services.paths()?, &tree_id, &mut stdout, &mut stderr)?;
            Ok(ProcessOutcome::Success)
        }
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
            )?
            .with_account_home(services.paths().ok().cloned())
            .with_process_jobs(services.process_jobs());
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

#[allow(
    clippy::too_many_lines,
    reason = "Read-only session display gathers recorded binding, runtime history, workflows, and child status before rendering the requested view."
)]
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
        .and_then(|()| writeln!(stdout, "  Tool calls    {}", s.tool_call_count))
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

#[allow(
    clippy::too_many_lines,
    reason = "Application command routing preserves one explicit handler for each account, catalog, usage, and one-shot run verb."
)]
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
                let status = AccountOps::offline_status(backend, &provider)?;
                if status.credential != CredentialState::RefreshRequired {
                    commands::support::require_ready(&provider, status.credential)?;
                }
            }
            commands::login::ensure_compatibility(backend, &provider, stdin, stderr)?;
            backend.run(
                &config,
                &provider,
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
            account,
            limits,
            model,
            effort,
            composition,
            wire,
            prompt,
            print_session_id,
        } => backend.run_api(
            &admit(stderr)?,
            account,
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
            account,
            limits,
            provider,
            model,
            effort,
            composition,
            prompt,
            print_session_id,
        } => backend.run_api(
            &admit(stderr)?,
            account,
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
        } => backend.continue_session(
            &admit(stderr)?,
            // A session a carry moved on runs on its successor (spec 02 §2.6).
            &carry::work_id(services, &session_id, stderr)?,
            prompt,
            print_session_id,
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
    reason = "The product dispatcher exhaustively routes the remaining admitted commands without a second interpretation of their arguments."
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
            review_expiry::write_warnings(stdout, services.now_unix_seconds())
                .map_err(|_| CliFailure::new("stdout failed"))?;
            auth_status(backend, &provider, services.paths().ok(), stdout)
        }
        Command::AuthLogin { provider, mode } => {
            auth_login(backend, &provider, mode, stdin, stderr, login_ui)
        }
        Command::AuthRefresh(provider) => auth_refresh(backend, &provider, stdin, stderr),
        Command::AuthLogout {
            provider,
            local_only: true,
        } => auth_logout_local(backend, &provider, stderr),
        Command::AuthLogout {
            provider,
            local_only: false,
        } => auth_logout(backend, &provider, stdin, stderr),
        Command::Models(provider) => list_models(backend, &provider, stdin, stdout, stderr),
        Command::ModelsOpenRouter(account) => {
            commands::catalog::list_openrouter_models(backend, &account, stdout)
        }
        Command::ModelsPlatform(provider) => {
            let catalog = backend.platform_catalog(&provider)?;
            commands::catalog::list_platform_models(&catalog, &provider, stdout)
        }
        Command::ConfigShow => dispatch::execute_config_show(services, stdout),
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
            dispatch::execute_config_set(services, stdout, key, value, scope)
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
        Command::KeySet { account } => key_set(account.as_str(), stdin, stderr),
        Command::KeyClear { account } => key_clear(account.as_str(), stdout),
        Command::KeyStatus { account } => {
            key_status(account.map(|account| account.to_string()), stdout)
        }
        Command::SecretSet { shelf, name } => {
            commands::secret::secret_set(services.paths()?, shelf, &name, stdin, stderr)
        }
        Command::SecretRm { shelf, name } => {
            commands::secret::secret_rm(services.paths()?, shelf, &name, stdout)
        }
        Command::SecretList { shelf } => {
            commands::secret::secret_list(services.paths()?, shelf, stdout)
        }
        Command::Mcp(command) => {
            commands::mcp::run(services.paths()?, command, stdin, stdout, stderr, login_ui)
        }
        Command::Runner(_) => unreachable!("runner commands dispatch before the stdio locks"),
        Command::StatusAll => dispatch::execute_status_all(services, backend, stdout),
        Command::AccountsList => {
            for account in backend.accounts()? {
                writeln!(stdout, "{account}").map_err(|_| CliFailure::new("stdout failed"))?;
            }
            Ok(ProcessOutcome::Success)
        }
        Command::AccountsRemove { account, confirmed } => {
            commands::login::remove_account(backend, &account, confirmed, stdin, stdout, stderr)
        }
        Command::SessionList { include_archived } => {
            dispatch::execute_session_list(services, stdout, include_archived)
        }
        Command::SessionRename { id, title } => {
            commands::session::lifecycle::rename_cli(services, &id, title.as_deref())?;
            writeln!(stdout, "renamed session {id}")
                .map_err(|_| CliFailure::new("stdout failed"))?;
            Ok(ProcessOutcome::Success)
        }
        Command::SessionUnarchive { id } => {
            commands::session::lifecycle::archive_cli(services, &id, false)?;
            writeln!(stdout, "unarchived session {id}")
                .map_err(|_| CliFailure::new("stdout failed"))?;
            Ok(ProcessOutcome::Success)
        }
        Command::SessionArchive { id, yes } => {
            confirm_session_mutation("session archive", &id, yes, stdin, stderr)?;
            commands::session::lifecycle::archive_cli(services, &id, true)?;
            writeln!(stdout, "archived session {id}")
                .map_err(|_| CliFailure::new("stdout failed"))?;
            Ok(ProcessOutcome::Success)
        }
        Command::SessionShow { id } => show_session_read_only(services.paths()?, &id, stdout),
        Command::SessionRecover { id } => {
            let record = load_admitted_product_session(
                services.paths()?,
                services.session_records()?.as_ref(),
                &id,
            )?;
            // Opening the session is what recovers it; the explicit action then
            // reports it, or that there was nothing to recover.
            run_attached_action(
                services,
                record,
                surface_protocol::UserAction::Recover,
                stdout,
            )?;
            Ok(ProcessOutcome::Success)
        }
        Command::SessionExport { id, out } => {
            commands::session::export_session(services, &id, out, stdout)?;
            Ok(ProcessOutcome::Success)
        }
        Command::SessionReloadRequest { id, expect_binary } => {
            commands::session::request_reload(services, &id, &expect_binary, stdout)?;
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
        } => dispatch::execute_session_log(services, stdout, id, kind, tail, json),
        Command::Fork {
            session_id,
            confirmed,
        } => {
            confirm_session_mutation("fork", &session_id, confirmed, stdin, stderr)?;
            let record = load_admitted_product_session(
                services.paths()?,
                services.session_records()?.as_ref(),
                &session_id,
            )?;
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
            let record = load_admitted_product_session(
                services.paths()?,
                services.session_records()?.as_ref(),
                &session_id,
            )?;
            run_attached_action(
                services,
                record,
                surface_protocol::UserAction::Compact,
                stdout,
            )?;
            Ok(ProcessOutcome::Success)
        }
        #[cfg(feature = "maintainer")]
        Command::SessionRehearse { id } => {
            commands::reload_rehearsal::rehearse(services.paths()?, &id, stdout)?;
            Ok(ProcessOutcome::Success)
        }
        Command::Usage {
            account,
            session_id,
        } => dispatch::execute_usage(services, backend, stdout, account, session_id),
        Command::Run { .. }
        | Command::RunOpenRouter { .. }
        | Command::RunPlatform { .. }
        | Command::ContinueSession { .. }
        | Command::Tui { .. }
        | Command::TuiPlacement { .. }
        | Command::Resume { .. }
        | Command::HostResume { .. }
        | Command::Agent { .. }
        | Command::Host(_)
        | Command::HostInstall(_)
        | Command::HostConnect { .. }
        | Command::HostUpgrade { .. }
        | Command::WireSchema
        | Command::SessionCheckBinding
        | Command::Help
        | Command::CommandHelp { .. }
        | Command::Version
        | Command::ConfigEdit
        | Command::StatePruneLegacy { .. }
        | Command::TreeRelease { .. }
        | Command::StateKey
        | Command::WriteBinaryManifest { .. }
        | Command::ReloadProbe
        | Command::HostUpgradeProbe { .. }
        | Command::HostUpgradeCapacity { .. }
        | Command::ReopenProbe { .. }
        | Command::CarryProbe { .. }
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
        account,
        session_id,
        provider,
        platform,
        wire,
        model,
        effort,
        effort_explicit,
        composition,
        wire_v2,
    } = command
    else {
        return Err(CliFailure::new("internal command dispatch error"));
    };
    run_agent(
        services,
        session_id,
        account,
        provider,
        platform,
        wire,
        model,
        effort,
        effort_explicit,
        composition,
        wire_v2,
    )
}

/// Keys whose save must move through the successor-epoch apply.
///
/// The verdict is the key table's own `scope` column, so a new orchestration
/// key cannot be added without one.
fn config_key_changes_orchestration(key: &str) -> bool {
    product_config::config_key(key).is_some_and(|row| row.scope == ConfigScope::Epoch)
}

#[cfg(test)]
fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
    cli::parse(arguments).map_err(|failure| failure.message().to_owned())
}

#[cfg(test)]
mod lib_tests;

mod platform_catalog;
mod platform_product;
