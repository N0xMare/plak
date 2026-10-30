//! One-shot attached actions and carry-capacity probes.

use super::import_export::PreparedImport;
use crate::commands::host_config::{
    TuiAttachEligibility, build_host_config, tui_attach_eligibility,
};
use crate::commands::support::{HOST_WORKER_JOIN_TIMEOUT, join_host_worker_bounded};
use crate::presentation::tui_presentation_bundle;
use crate::product_config;
use crate::store::sessions as product_sessions;
use plakous_hosted::error::{CliFailure, CliResult};
use plakous_hosted::runtime as product_host;
use plakous_hosted::sessions::{NoticePlacement, NoticeTone, TranscriptRow};
use std::io;
use std::path::Path;

#[derive(Debug)]
pub(super) enum AttachedConfigError {
    Unresolvable(CliFailure),
    Other(CliFailure),
    /// An exact-reopen verification answered before resolving anything.
    Answered(Box<crate::carry::ReopenProbe>),
}

impl From<CliFailure> for AttachedConfigError {
    fn from(error: CliFailure) -> Self {
        Self::Other(error)
    }
}

impl From<AttachedConfigError> for CliFailure {
    fn from(error: AttachedConfigError) -> Self {
        match error {
            AttachedConfigError::Unresolvable(error) | AttachedConfigError::Other(error) => error,
            AttachedConfigError::Answered(_) => {
                CliFailure::new("exact reopen was answered before resolution")
            }
        }
    }
}

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

/// Whether an exact reopen of `record` would attach on this build, answered
/// without owning it: the attach's own binding resolution, then the open's
/// verification at `head` (spec 02 §2.5.1).
pub(crate) fn verify_exact_reopen(
    services: &crate::shell::services::Services,
    record: product_sessions::SessionRecord,
    head: plakous::sessions::SessionLocator,
) -> CliResult<crate::carry::ReopenProbe> {
    let config = match build_attached_host_config(services, record, AttachedPurpose::Verify) {
        Ok(config) => config,
        Err(AttachedConfigError::Answered(probe)) => return Ok(*probe),
        Err(error) => return Err(error.into()),
    };
    product_host::verify_exact_reopen(config, head).map(Into::into)
}

/// What an attached host configuration is built for.
#[derive(Clone, Copy, Eq, PartialEq)]
enum AttachedPurpose {
    /// One attached action.
    Action,
    /// A `/rebind` target, which classifies a removed binding as fallback.
    Rebind,
    /// An exact-reopen verification, which must not write.
    Verify,
}

pub(super) fn attached_host_config(
    services: &crate::shell::services::Services,
    record: product_sessions::SessionRecord,
) -> Result<product_host::HostConfig<crate::surface_protocol::PlakExtension>, AttachedConfigError> {
    build_attached_host_config(services, record, AttachedPurpose::Action)
}

pub(super) fn rebind_host_config(
    services: &crate::shell::services::Services,
    record: product_sessions::SessionRecord,
) -> Result<product_host::HostConfig<crate::surface_protocol::PlakExtension>, AttachedConfigError> {
    build_attached_host_config(services, record, AttachedPurpose::Rebind)
}

fn build_attached_host_config(
    services: &crate::shell::services::Services,
    record: product_sessions::SessionRecord,
    purpose: AttachedPurpose,
) -> Result<product_host::HostConfig<crate::surface_protocol::PlakExtension>, AttachedConfigError> {
    let classify_rebind_fallback = purpose == AttachedPurpose::Rebind;
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
                    if purpose == AttachedPurpose::Verify {
                        crate::test_hooks::OfflineFace::Verify
                    } else {
                        crate::test_hooks::OfflineFace::OneShot
                    },
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
            .map_err(|error| AttachedConfigError::Other(CliFailure::new(error)));
    }
    // After the offline model's early return: the probe never renews a login.
    if purpose == AttachedPurpose::Verify
        && let Some(probe) = crate::carry::reopen_credential_gate(record.account.as_deref())
    {
        return Err(AttachedConfigError::Answered(Box::new(probe)));
    }
    let account = record
        .account
        .clone()
        .ok_or_else(|| CliFailure::new("session has no exact account binding"))?;
    let model = record
        .model
        .clone()
        .ok_or_else(|| CliFailure::new("session has no exact model binding"))?;
    if plakous_hosted::capabilities::for_account(&account).is_none() {
        return Err(AttachedConfigError::Unresolvable(CliFailure::new(format!(
            "account `{account}` is no longer available"
        ))));
    }
    let presentation = tui_presentation_bundle(paths)?;
    let ready_catalog = &presentation.sections;
    let (effort, explicit) = product_host::effort_from_config(record.effort.as_deref());
    validate_catalog_for_attach(
        &record.target_selection,
        ready_catalog,
        &presentation.native_models,
        &account,
        &model,
        explicit,
        classify_rebind_fallback,
    )?;
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
    .map_err(|error| AttachedConfigError::Other(CliFailure::new(error)))
}

fn validate_catalog_for_attach(
    target: &product_config::ProductTargetSelection,
    catalog: &plakous_host_protocol::views::ReadyCatalog,
    native_models: &[crate::presentation::HostLaunchNativeModelFact],
    account: &str,
    model: &str,
    effort_explicit: bool,
    classify_rebind_fallback: bool,
) -> Result<(), AttachedConfigError> {
    if classify_rebind_fallback {
        require_catalog_binding(target, catalog, native_models, account, model)?;
    }
    if tui_attach_eligibility(target, catalog, Some(account), Some(model), effort_explicit)
        == TuiAttachEligibility::EffortRequired
    {
        return Err(CliFailure::new("session has no exact effort binding").into());
    }
    // An unavailable picker is not runtime authority. Exact actions/imports
    // resolve the binding below rather than requiring the picker's Ready row.
    Ok(())
}

fn require_catalog_binding(
    target: &product_config::ProductTargetSelection,
    catalog: &plakous_host_protocol::views::ReadyCatalog,
    native_models: &[crate::presentation::HostLaunchNativeModelFact],
    account: &str,
    model: &str,
) -> Result<(), AttachedConfigError> {
    let route = target
        .route(account)
        .map_err(|error| CliFailure::new(error.message(account)))?;
    if matches!(
        route,
        plakous_hosted::binding::AttachRoute::OpenRouter { .. }
    ) {
        return Ok(());
    }
    let section = catalog
        .accounts
        .iter()
        .find(|section| section.account == account);
    let Some(section) = section.filter(|section| section.ready) else {
        return Err(CliFailure::new(format!(
            "the `{account}` catalog is not Ready; restore the login or key before /rebind"
        ))
        .into());
    };
    let catalog_present = section.entries.iter().any(|entry| entry.id == model)
        || native_models
            .iter()
            .any(|fact| fact.account == account && fact.exact_id == model);
    if !catalog_present {
        return Err(AttachedConfigError::Unresolvable(CliFailure::new(format!(
            "model `{model}` is no longer in the Ready `{account}` catalog"
        ))));
    }
    Ok(())
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

#[cfg(test)]
mod binding_tests;
