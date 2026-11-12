//! One-shot attached actions and carry-capacity probes.

use super::import_export::PreparedImport;
use crate::commands::host_config::{
    TuiAttachEligibility, build_host_config, tui_attach_eligibility,
};
use crate::commands::support::{HOST_WORKER_JOIN_TIMEOUT, join_host_worker_bounded};
use crate::product_config;
use crate::store::sessions as product_sessions;
use plakous_host_protocol::status::HostPhase;
use plakous_hosted::error::{CliFailure, CliResult};
use plakous_hosted::runtime as product_host;
use plakous_hosted::sessions::{NoticePlacement, NoticeTone, TranscriptRow};
use std::io;
use std::path::Path;

pub(super) fn session_paths(
    services: &crate::shell::services::Services,
    record: &product_sessions::SessionRecord,
) -> CliResult<crate::home::paths::PlakPaths> {
    let paths = services.paths()?;
    Ok(if record.runner.is_some() {
        paths.for_remote_workspace(&record.workspace_root)
    } else {
        paths.for_workspace(&record.workspace_root)
    })
}

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

/// Upgrade admission cannot defer a capacity or credential refusal until the
/// source has surrendered. This probe never opens the source store or a turn.
pub(crate) fn probe_upgrade_capacity(
    services: &crate::shell::services::Services,
    path: &Path,
) -> CliResult<crate::carry::CarryCapacityProbe> {
    let document =
        crate::carry::read_document(path).map_err(|error| CliFailure::new(error.to_string()))?;
    let mut prepared = PreparedImport::new(document, false)?;
    let record = &mut prepared.carried.record;
    let placement = crate::carry::upgrade::remote::Placement::prepare(
        services,
        record
            .runner
            .as_ref()
            .map_or("local", |runner| runner.runner_id.as_str()),
        &record.workspace_root,
    )?;
    let services = &placement.services;
    let paths = session_paths(services, record)?;
    if let Some(profile) = &prepared.carried.profile {
        let home = product_config::ProductConfig::load(&paths).map_err(CliFailure::new)?;
        crate::profiles::selection_config(&home, profile).map_err(CliFailure::new)?;
        record.root_profile = Some(plakous_hosted::sessions::RootProfile {
            root_template: profile.root.clone(),
            working_set: profile.working_set.clone(),
        });
    }
    let config =
        build_attached_host_config(services, prepared.carried.record, AttachedPurpose::Verify)
            .map_err(|error| match error {
                AttachedConfigError::Answered(verdict) => CliFailure::new(format!(
                    "host_upgrade_credentials: destination cannot verify credentials: {verdict:?}"
                )),
                error => error.into(),
            })?;
    let capacity = carry_capacity_from_window(
        product_host::carry_input_window(&config)?,
        prepared.estimated_tokens,
        prepared.messages,
    );
    let capacity = {
        let mut capacity = capacity;
        if let Some(limit) = crate::test_hooks::upgrade_input_limit() {
            capacity.admitted_input = if limit == "unknown" {
                None
            } else {
                Some(
                    limit
                        .parse()
                        .map_err(|_| CliFailure::new("invalid upgrade input-limit test hook"))?,
                )
            };
        }
        capacity
    };
    crate::carry::upgrade::require_capacity(&capacity)?;
    Ok(capacity)
}

/// Whether an exact reopen of `record` would attach on this build, answered
/// without owning it: the attach's own binding resolution, then the open's
/// verification at `head` (spec 02 §2.5.1).
pub(crate) fn verify_host_upgrade_reopen(
    services: &crate::shell::services::Services,
    record: product_sessions::SessionRecord,
    head: plakous::sessions::SessionLocator,
) -> CliResult<crate::carry::ReopenProbe> {
    let placement = crate::carry::upgrade::remote::Placement::prepare(
        services,
        record
            .runner
            .as_ref()
            .map_or("local", |runner| runner.runner_id.as_str()),
        &record.workspace_root,
    )?;
    let services = &placement.services;
    verify_exact_reopen(services, record, head)
}

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
    if let Err(error) = crate::build_features::admit_session(&record) {
        let unknown_account = std::error::Error::source(&error).is_some_and(|source| {
            matches!(
                source.downcast_ref::<plakous_hosted::capabilities::AccountAdmissionError>(),
                Some(plakous_hosted::capabilities::AccountAdmissionError::UnknownAccount { .. })
            )
        });
        if classify_rebind_fallback && unknown_account {
            // Choosing the configured root requires a valid local policy first.
            let paths = session_paths(services, &record)?;
            product_config::SessionPolicy::load(&paths).map_err(CliFailure::new)?;
            return Err(AttachedConfigError::Unresolvable(error));
        }
        return Err(error.into());
    }
    let context = session_paths(services, &record)?;
    let paths = &context;
    product_config::SessionPolicy::load(paths).map_err(CliFailure::new)?;
    let cwd = record.workspace_root.clone();
    // The repository gate's offline model; see `crate::test_hooks`.
    #[cfg(feature = "test-hooks")]
    if let Some(selection) = crate::test_hooks::selected() {
        let state_roots = services.state_roots_on(
            paths,
            record
                .runner
                .as_ref()
                .map_or("local", |runner| runner.runner_id.as_str()),
        )?;
        let (effort, explicit) = product_host::effort_from_config(record.effort.as_deref());
        return selection
            .map_err(CliFailure::new)
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
            .map_err(AttachedConfigError::Other);
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
    let presentation = attached_presentation(paths, &account)?;
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
    let state_roots = services.state_roots_on(
        paths,
        record
            .runner
            .as_ref()
            .map_or("local", |runner| runner.runner_id.as_str()),
    )?;
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
    .map_err(AttachedConfigError::Other)
}

fn attached_presentation(
    paths: &crate::home::paths::PlakPaths,
    account: &str,
) -> CliResult<crate::presentation::TuiPresentationBundle> {
    let selection: plakous_hosted::account::AccountRef = account.parse()?;
    crate::commands::host_config::binding_presentation(paths, &selection)
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
    let selection: plakous_hosted::account::AccountRef =
        account.parse().map_err(CliFailure::from)?;
    if classify_rebind_fallback && !selection.is_named() {
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
    let session_id = config.session.id.clone();
    let (actions, events, worker) =
        crate::surface_protocol::face_of(product_host::spawn_host_worker(config)?);
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
                failure = Some(crate::notice_text::with_remedy(
                    &notice.text,
                    notice.remedy.as_ref(),
                    crate::notice_text::RemedyContext {
                        frontend: crate::notice_text::Frontend::Run,
                        session_id: Some(&session_id),
                    },
                ));
            }
            crate::surface_protocol::HostEvent::Row(TranscriptRow::Notice(notice))
                if notice.placement() != NoticePlacement::SessionLog =>
            {
                writeln!(
                    stdout,
                    "{}",
                    crate::notice_text::with_remedy(
                        &notice.text,
                        notice.remedy.as_ref(),
                        crate::notice_text::RemedyContext {
                            frontend: crate::notice_text::Frontend::Run,
                            session_id: Some(&session_id)
                        }
                    )
                )
                .map_err(|_| CliFailure::new("stdout failed"))?;
            }
            crate::surface_protocol::HostEvent::Status {
                phase: HostPhase::Error,
                detail,
                ..
            } => {
                failure = Some(detail.as_ref().map_or_else(
                    || "attached session command failed".into(),
                    |detail| {
                        crate::host_status_text::status_detail(
                            detail,
                            crate::notice_text::Frontend::Run,
                        )
                    },
                ));
            }
            _ => {}
        }
    }
    failure.map_or(Ok(()), |message| Err(CliFailure::new(message)))
}

#[cfg(test)]
mod binding_tests;
