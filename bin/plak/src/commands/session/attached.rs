//! One-shot attached actions and carry-capacity probes.

use super::import_export::PreparedImport;
use crate::commands::host_config::{attached_target_accepts_effort, build_host_config};
use crate::commands::support::{HOST_WORKER_JOIN_TIMEOUT, join_host_worker_bounded};
use crate::presentation::tui_presentation_bundle;
use crate::product_config;
use crate::store::sessions as product_sessions;
use plakous_hosted::error::{CliFailure, CliResult};
use plakous_hosted::runtime as product_host;
use plakous_hosted::sessions::{NoticePlacement, NoticeTone, TranscriptRow};
use std::io;
use std::path::Path;

/// The one-shot attach uses the composition root's record service and sinks.
pub(crate) fn run_attached_action(
    services: &crate::shell::services::Services,
    record: product_sessions::SessionRecord,
    action: crate::surface_protocol::UserAction,
    stdout: &mut dyn io::Write,
) -> CliResult<()> {
    let mut config = attached_host_config(services, record)?;
    config.attached_one_shot = true;
    run_attached_config(config, action, stdout)
}

/// A host config for `record` under its own exact binding, as a one-shot
/// command attaches it.
pub(super) fn carry_capacity_from_window(
    window: plakous::sessions::SessionInputWindow,
    estimated_tokens: u64,
    messages: usize,
) -> crate::carry::CarryCapacityProbe {
    crate::carry::CarryCapacityProbe {
        route: window.route,
        total_window: window.total_window,
        admitted_input: window.admitted_input,
        estimated_tokens,
        messages,
    }
}

pub(crate) fn probe_carry_capacity(
    services: &crate::shell::services::Services,
    path: &Path,
) -> CliResult<crate::carry::CarryCapacityProbe> {
    let document = crate::carry::read_document(path).map_err(|e| CliFailure::new(e.to_string()))?;
    probe_prepared_capacity(services, PreparedImport::new(document, false)?)
}

pub(crate) fn probe_prepared_capacity(
    services: &crate::shell::services::Services,
    prepared: PreparedImport,
) -> CliResult<crate::carry::CarryCapacityProbe> {
    let config = attached_host_config(services, prepared.carried.record)?;
    Ok(carry_capacity_from_window(
        product_host::carry_input_window(&config)?,
        prepared.estimated_tokens,
        prepared.messages,
    ))
}

pub(super) fn attached_host_config(
    services: &crate::shell::services::Services,
    record: product_sessions::SessionRecord,
) -> CliResult<product_host::HostConfig<crate::surface_protocol::PlakExtension>> {
    crate::build_features::admit_session(&record)?;
    let paths = &crate::home::paths::PlakPaths::for_workspace(
        services.paths()?.home().to_path_buf(),
        &record.workspace_root,
    );
    product_config::SessionPolicy::load(paths).map_err(CliFailure::new)?;
    let cwd = std::env::current_dir().map_err(|_| CliFailure::new("cwd unavailable"))?;
    // The repository gate's offline model; see `crate::test_hooks`.
    #[cfg(feature = "test-hooks")]
    if let Some(selection) = crate::test_hooks::selected() {
        let state_roots = services.state_roots()?;
        let (effort, explicit) = product_host::effort_from_config(record.effort.as_deref());
        return selection
            .and_then(|selection| {
                selection.host_config(
                    crate::test_hooks::OfflineFace::OneShot,
                    paths,
                    &state_roots,
                    record,
                    effort,
                    explicit,
                    &cwd,
                    &plakous_host_protocol::views::ReadyCatalog::default(),
                    None,
                )
            })
            .map_err(CliFailure::new);
    }
    let account = record
        .account
        .clone()
        .ok_or_else(|| CliFailure::new("session has no exact account binding"))?;
    let model = record
        .model
        .clone()
        .ok_or_else(|| CliFailure::new("session has no exact model binding"))?;
    let presentation = tui_presentation_bundle(paths)?;
    let ready_catalog = &presentation.sections;
    let (effort, explicit) = product_host::effort_from_config(record.effort.as_deref());
    if !attached_target_accepts_effort(
        &record.target_selection,
        ready_catalog,
        &account,
        &model,
        explicit,
    ) {
        return Err(CliFailure::new("session has no exact effort binding"));
    }
    let state_roots = services.state_roots()?;
    build_host_config(
        paths,
        &state_roots,
        record,
        Some(&account),
        Some(&model),
        effort,
        explicit,
        &cwd,
        ready_catalog,
        None,
        &presentation.native_models,
    )
    .map_err(CliFailure::new)
}

fn run_attached_config(
    config: product_host::HostConfig<crate::surface_protocol::PlakExtension>,
    action: crate::surface_protocol::UserAction,
    stdout: &mut dyn io::Write,
) -> CliResult<()> {
    let (actions, events, worker) = product_host::spawn_host_worker(config)?;
    actions
        .send(action)
        .map_err(|_| CliFailure::new("attached session command channel closed"))?;
    // Closing the sole producer drains the already-admitted ordinary action
    // before the host exits. An urgent Quit could overtake and lose it.
    drop(actions);
    join_host_worker_bounded(worker, HOST_WORKER_JOIN_TIMEOUT, "attached session")?;
    let mut failure = None;
    for event in events.try_iter() {
        match event {
            crate::surface_protocol::HostEvent::Usage(usage) => {
                let prepared = usage.prepared_input.map_or_else(
                    || "unavailable".to_owned(),
                    |value| match value {
                        plakous_host_protocol::views::presentation::PreparedInputUsage::Exact(tokens) => {
                            format!("exact:{tokens}")
                        }
                        plakous_host_protocol::views::presentation::PreparedInputUsage::UpperBound(tokens) => {
                            format!("upper-bound:{tokens}")
                        }
                    },
                );
                writeln!(
                    stdout,
                    "context_status={} prepared_input={} total_context_window={} admitted_input_ceiling={}",
                    usage.status,
                    prepared,
                    usage
                        .total_window
                        .map_or_else(|| "unavailable".to_owned(), |value| value.to_string()),
                    usage
                        .admitted_ceiling
                        .map_or_else(|| "unavailable".to_owned(), |value| value.tokens.to_string())
                )
                .map_err(|_| CliFailure::new("stdout failed"))?;
            }
            crate::surface_protocol::HostEvent::Row(TranscriptRow::Notice(notice))
                if notice.tone == NoticeTone::Error =>
            {
                failure = Some(notice.text);
            }
            crate::surface_protocol::HostEvent::Row(TranscriptRow::Notice(notice))
                if notice.placement() != NoticePlacement::SessionLog =>
            {
                writeln!(stdout, "{}", notice.text)
                    .map_err(|_| CliFailure::new("stdout failed"))?;
            }
            crate::surface_protocol::HostEvent::Status { phase, detail, .. }
                if phase == "error" =>
            {
                failure = detail.or_else(|| Some("attached session command failed".into()));
            }
            _ => {}
        }
    }
    failure.map_or(Ok(()), |message| Err(CliFailure::new(message)))
}
