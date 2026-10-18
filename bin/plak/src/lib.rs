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
mod command;
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
mod tui_launch;
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
use std::io::{self, Write as _};
#[cfg(feature = "test-support")]
use std::path::Path;
use std::process::ExitCode;
#[cfg(feature = "otel")]
use std::sync::Arc;
use surface_protocol::SurfaceIntent;

use plakous::subscriptions::NativeProvider;

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
    confirm_session_mutation, load_exact_product_session, run_attached_action,
    show_child_recovery_status, show_recovery_status,
};
use crate::commands::support::{
    api_key_status_failure_cell, api_provider_label, config_set_failure, edit_config_file,
    profile_acceptance_label, status_credential_cell, subscription_provider_label,
};
use crate::commands::usage::{collect_usage_limits, usage_vertical_for_account};
use crate::presentation::{
    cli_human_output, credential_locations_line, event_table_row, format_unix_seconds,
    product_account_label, session_preview_line, title_case_token, transcript_entry,
};
use crate::shell::services::Services;
use crate::tui_launch::launch_tui;

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
        Command::ImportResume { path } => return carry::import_and_resume(&services, &path),
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
        } => auth_logout(backend, provider, stdin, stderr),
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
        Command::SecretSet { name } => {
            commands::secret::secret_set(services.paths()?, &name, stdin, stderr)
        }
        Command::SecretRm { name } => commands::secret::secret_rm(services.paths()?, &name, stdout),
        Command::SecretList => commands::secret::secret_list(services.paths()?, stdout),
        Command::Mcp(command) => commands::mcp::run(services.paths()?, command, stdout),
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

/// Keys whose save must move through the successor-epoch apply.
///
/// The verdict is the key table's own `scope` column, so a new orchestration
/// key cannot be added without one.
fn config_key_changes_orchestration(key: &str) -> bool {
    product_config::config_key(key).is_some_and(|row| row.scope == ConfigScope::Epoch)
}

fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
    cli::parse(arguments)
}

#[cfg(test)]
mod lib_tests;

mod platform_catalog;
mod platform_product;
