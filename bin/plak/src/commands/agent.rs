//! `plak agent`: one hosted agent bound to stdio.

use std::env;
use std::io;
use std::time::Instant;

use plakous::agent::ReasoningEffort;
use plakous::subscriptions::NativeProvider;

use super::host_config::build_host_config;
use super::session::{
    load_exact_product_session, resolve_api_composition, seed_new_session_binding,
};
use super::support::cli_provider_name;
use crate::presentation::tui_presentation_bundle;
use crate::product_config;
use crate::store::sessions as product_sessions;
use plakous_hosted::agent as product_agent;
use plakous_hosted::error::{CliFailure, CliFailureKind, CliResult, ProcessOutcome};
use plakous_hosted::runtime as product_host;

/// Runs one `plak agent` invocation over the exact or newly seeded session.
///
/// A rejection *before* the host opens — an unattested reasoning effort for
/// the route, a missing credential, an unusable state directory — used to
/// reach a JSONL client as end of stream with nothing on stdout, while the CLI
/// printed a typed line and exited non-zero. The client could not tell a
/// refusal from a crash. Every such failure now terminates the stream the way
/// the protocol says a stream ends: one `error` object carrying the same
/// message stderr gets, then `bye`. The exit code and the stderr line are
/// unchanged, and nothing here invents a `hello` for a session that never
/// opened.
#[allow(clippy::fn_params_excessive_bools, reason = "clap-shaped agent verb")]
pub(crate) fn run_agent(
    services: &crate::shell::services::Services,
    session_id: Option<String>,
    provider: Option<NativeProvider>,
    platform: Option<plakous_hosted::binding::PlatformProvider>,
    wire: Option<product_config::ProductWireBinding>,
    model: Option<String>,
    effort: Option<ReasoningEffort>,
    effort_explicit: bool,
    composition: Option<product_config::ProductCompositionSelection>,
    resume_tree: bool,
) -> CliResult<ProcessOutcome> {
    // Installed before admission, and before the session opens: a session open
    // reaches the provider for discovery and can take seconds, and a signal
    // arriving in that window would otherwise take the default disposition and
    // end the stream with no `error` and no `bye`.
    //
    // The watcher records which signal arrived and nothing else. Acting on it
    // belongs to the drive, because only the drive knows whether a turn is in
    // flight, and the order matters: a quit issued while a turn runs waits for
    // that turn, so a stop that sent interrupt and quit together waited out the
    // provider it was trying to escape. Until the drive exists, the watcher's
    // own deadline is the whole answer.
    crate::signals::install(crate::signals::jsonl_announcement(), || {});
    // Ctrl-C (`SIGINT`) interrupts a running turn and ends the stream the way
    // end of input does; the drive polls the count.
    crate::signals::count_interrupts();
    let outcome = run_agent_after_admission(
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
    );
    // Exactly one terminal record. Everything before the host opens is
    // terminated here; once the host owns the stream it terminates its own,
    // including on a panic, so this must not also write one.
    let admitted = match outcome {
        Ok(admitted) => admitted,
        Err(failure) => {
            return Err(report_pre_dispatch_rejection(&mut io::stdout(), failure));
        }
    };
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    crate::agent_stdio::run_agent_stdio(
        admitted.config,
        admitted.resume_tree,
        &mut stdout,
        &mut stderr,
    )
}

/// Terminates the JSONL stream on a rejection that precedes the host.
///
/// Returns the failure unchanged so the caller's exit code and stderr stay
/// exactly as they were.
fn report_pre_dispatch_rejection(stdout: &mut dyn io::Write, failure: CliFailure) -> CliFailure {
    let _ = writeln!(
        stdout,
        "{}",
        plakous_host_protocol::envelope::diagnostic_line(None, failure.message())
    );
    let _ = writeln!(
        stdout,
        "{}",
        plakous_host_protocol::envelope::bye_line(None)
    );
    let _ = stdout.flush();
    failure
}

/// The record a new `plak agent` session saves: the root template's binding,
/// overridden by the command's account, model, effort and composition. A
/// direct API account binds what `platform::api_composition` decides for the
/// final model, as `plak run` does for the same arguments.
#[allow(
    clippy::fn_params_excessive_bools,
    clippy::too_many_arguments,
    reason = "clap-shaped agent verb"
)]
pub(crate) fn new_agent_record(
    cfg: &product_config::ProductConfig,
    cwd: std::path::PathBuf,
    provider: Option<NativeProvider>,
    platform: Option<plakous_hosted::binding::PlatformProvider>,
    wire: Option<product_config::ProductWireBinding>,
    model: Option<String>,
    effort: Option<ReasoningEffort>,
    effort_explicit: bool,
    composition: Option<product_config::ProductCompositionSelection>,
) -> CliResult<product_sessions::SessionRecord> {
    let mut record = product_sessions::SessionRecord::new(cwd);
    seed_new_session_binding(&mut record, cfg)?;
    if let Some(model) = model {
        record.model = Some(model);
    }
    if let Some(platform) = platform {
        record.account = Some(platform.account().to_owned());
        record.target_selection =
            product_config::ProductTargetSelection::Platform { provider: platform };
        record.composition = composition.unwrap_or_else(|| {
            cfg.composition_for_tuple(record.account.as_deref(), record.model.as_deref())
        });
        resolve_api_composition(&mut record, platform)?;
    } else if let Some(wire) = wire {
        record.account = Some("openrouter".to_owned());
        record.target_selection = product_config::ProductTargetSelection::OpenRouter {
            wire,
            routing: product_config::ProductOpenRouterRoutingSelection::GatewayManaged,
        };
        if let Some(composition) = composition {
            record.composition = composition;
        }
        if record.composition == product_config::ProductCompositionSelection::FollowCatalog {
            record.composition = product_config::ProductCompositionSelection::Direct;
        }
    } else {
        if let Some(provider) = provider {
            record.account = Some(cli_provider_name(provider).to_owned());
            record.target_selection = product_config::ProductTargetSelection::Native;
        }
        if let Some(composition) = composition {
            record.composition = composition;
        }
    }
    if effort_explicit {
        record.effort = effort.map(product_host::effort_label);
    } else if record.account.as_deref() == Some("openrouter") {
        record.effort = None;
    }
    if let (Some(account), Some(model)) = (record.account.as_deref(), record.model.as_deref()) {
        super::support::admit_account_model(account, model)
            .map_err(|refusal| CliFailure::classified(CliFailureKind::Usage, refusal))?;
    }
    Ok(record)
}

#[allow(clippy::fn_params_excessive_bools, reason = "clap-shaped agent verb")]
fn run_agent_after_admission(
    services: &crate::shell::services::Services,
    session_id: Option<String>,
    provider: Option<NativeProvider>,
    platform: Option<plakous_hosted::binding::PlatformProvider>,
    wire: Option<product_config::ProductWireBinding>,
    model: Option<String>,
    effort: Option<ReasoningEffort>,
    effort_explicit: bool,
    composition: Option<product_config::ProductCompositionSelection>,
    resume_tree: bool,
) -> CliResult<AdmittedAgent> {
    let paths = services.paths()?;
    let cfg = super::support::admit_product_config(paths, &mut io::stderr().lock())?;
    let store = services.session_records()?;
    crate::home::state::ensure_usable_state(paths, &mut io::stderr().lock())
        .map_err(CliFailure::new)?;
    let cwd = env::current_dir().map_err(|_| CliFailure::new("cwd unavailable"))?;
    let (record, resume_tree) = if let Some(session_id) = session_id {
        (
            load_exact_product_session(paths, store.as_ref(), &session_id)?,
            resume_tree,
        )
    } else {
        let mut record = new_agent_record(
            &cfg,
            cwd.clone(),
            provider,
            platform,
            wire,
            model,
            effort,
            effort_explicit,
            composition,
        )?;
        store.save(&mut record).map_err(CliFailure::new)?;
        (record, false)
    };
    let state_roots = services.state_roots()?;
    let tree_id = crate::topology_product::durable_topology_tree_id(&state_roots, &record);
    let resume_tree = crate::topology_product::requested_tree_resume(paths, &tree_id, resume_tree)?;
    if !resume_tree && crate::topology_product::tree_requires_explicit_resume(paths, &tree_id) {
        return Err(CliFailure::new(product_agent::RESTORED_PAUSED_RESUME_HINT));
    }
    let account = record.account.clone().ok_or_else(|| {
        CliFailure::classified(
            CliFailureKind::Usage,
            "plak agent requires --account or a configured [root] agent template",
        )
    })?;
    let model_id = record.model.clone().ok_or_else(|| {
        CliFailure::classified(
            CliFailureKind::Usage,
            "plak agent requires --model or a configured [root] agent template model",
        )
    })?;
    // A missing API key is an authentication failure (exit 3) the client can
    // act on, not a host that fails to attach after the stream opened.
    if crate::home::secrets::is_api_key_account(&account) {
        crate::home::secrets::require_api_key(paths, &account)?;
    }
    let is_openrouter = account == "openrouter";
    let (resolved_effort, resolved_explicit) = if is_openrouter {
        if effort_explicit {
            (
                effort.ok_or_else(|| CliFailure::new("agent --effort is missing"))?,
                true,
            )
        } else {
            (ReasoningEffort::Medium, false)
        }
    } else if effort_explicit {
        (
            effort.ok_or_else(|| CliFailure::new("agent --effort is missing"))?,
            true,
        )
    } else {
        product_host::effort_from_config(record.effort.as_deref())
    };
    if !is_openrouter && !resolved_explicit {
        return Err(CliFailure::classified(
            CliFailureKind::Usage,
            "plak agent requires --effort or an effort in the [root] agent template",
        ));
    }
    let presentation_started = Instant::now();
    let presentation = tui_presentation_bundle(paths)?;
    let presentation_ms =
        u64::try_from(presentation_started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let resolve_started = Instant::now();
    let mut config = build_host_config(
        paths,
        &state_roots,
        record,
        Some(&account),
        Some(&model_id),
        resolved_effort,
        resolved_explicit,
        &cwd,
        &presentation.sections,
        None,
        &presentation.native_models,
    )
    .map_err(|error| CliFailure::classified(CliFailureKind::Auth, error))?;
    config.ready_path = product_host::ReadyPathTiming {
        presentation_ms,
        resolve_ms: u64::try_from(resolve_started.elapsed().as_millis()).unwrap_or(u64::MAX),
    };
    Ok(AdmittedAgent {
        config,
        resume_tree,
    })
}

/// The admitted host binding, past every pre-dispatch rejection.
struct AdmittedAgent {
    config: product_host::HostConfig<crate::surface_protocol::PlakExtension>,
    /// The resolved disposition, which the exact session record may set.
    resume_tree: bool,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::report_pre_dispatch_rejection;
    use plakous_hosted::error::{CliFailure, CliFailureKind};

    /// A rejection before the host opens terminates the stream properly.
    ///
    /// `plak agent` bound to an effort the route
    /// does not enumerate refused with exit 5 and one stderr line, while the
    /// JSONL client saw end of stream with `bye: false` and no record saying
    /// why. It could not tell a typed refusal from a crash.
    #[test]
    fn a_pre_dispatch_rejection_emits_a_terminal_error_then_bye() {
        let mut stdout = Vec::new();
        let message = "OpenRouter Chat does not enumerate reasoning effort \"low\" for this model \
             (attested: [xhigh, high])";
        let returned = report_pre_dispatch_rejection(
            &mut stdout,
            CliFailure::classified(CliFailureKind::Auth, message),
        );
        let lines: Vec<serde_json::Value> = String::from_utf8(stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 2, "exactly one error and one bye");
        assert_eq!(lines[0]["kind"], "error");
        assert_eq!(lines[0]["message"], message);
        assert_eq!(lines[1]["kind"], "bye", "the stream ends deliberately");
        // No `hello` is invented for a session that never opened, and the
        // failure passes through so the exit code and stderr are unchanged.
        assert!(lines.iter().all(|line| line["kind"] != "hello"));
        assert_eq!(
            returned.exit_code(),
            CliFailureKind::Auth.exit_code(),
            "the exit code is unchanged by the stream termination"
        );
        assert_eq!(returned.message(), message);
    }
}
