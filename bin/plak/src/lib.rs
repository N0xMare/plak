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
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use surface_protocol::SurfaceIntent;

use plakous::agent::ReasoningEffort;
use plakous::subscriptions::NativeProvider;

use crate::command::{Command, command_is_read_only, command_needs_product_state, runs_a_cli_turn};
use crate::commands::agent::run_agent;
use crate::commands::application::{AccountOps, PlakousApplication};
use crate::commands::catalog::list_models;
use crate::commands::doctor::run_doctor;
use crate::commands::host_config::{
    NativeTuiAttacher, TuiAttachEligibility, TuiAttacher, build_host_config,
    deferred_attach_refusal_reason, tui_attach_eligibility,
};
use crate::commands::login::{
    auth_login, auth_logout, auth_logout_local, auth_refresh, auth_status, key_clear, key_set,
    key_status, print_compatibility_notice,
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
use crate::presentation::{
    cli_human_output, credential_locations_line, event_table_row, format_unix_seconds,
    product_account_label, session_preview_line, title_case_token, transcript_entry,
};
use crate::shell::services::Services;

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
                carry::carry_foreign_session(services, &paths, &cwd, &requested)?
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
            Err(
                commands::host_config::native_credential_refusal(account.as_deref()).map_or_else(
                    || "selected model is absent from the discovered Ready catalog".to_owned(),
                    |refusal| commands::host_config::credential_unusable_reason(&refusal),
                ),
            )
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
                                    (eligibility == TuiAttachEligibility::Unavailable
                                        && !prep_needs_setup)
                                        .then(|| {
                                            commands::host_config::native_credential_refusal(
                                                prep_account.as_deref(),
                                            )
                                        })
                                        .flatten(),
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
                    } else if attach_err.starts_with("credential_unusable: ") {
                        attach_err.clone()
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
            let path = match carry::carry_out(paths, record) {
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

fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
    cli::parse(arguments)
}

#[cfg(test)]
mod lib_tests;

mod platform_catalog;
mod platform_product;
