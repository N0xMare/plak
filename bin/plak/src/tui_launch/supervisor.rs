//! The session supervisor: the product side of an interactive launch that
//! owns its rounds. It opens the session a round shows, settles the round
//! when the TUI hands the terminal back (the terminal restored before any
//! abandon), and answers the TUI's session requests (retry attach, resume
//! another session, rebind, reload, the config editor) by opening the next
//! session for the next round. It holds the launcher's cells: the `/new`
//! publication and the exact-resume publication fence.

use plakous_host_protocol::status::{DetachedReason, HostFailure};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use plakous::agent::ReasoningEffort;

use super::{supported_provider_rows, tui_presentation_bundle, tui_presentation_values};
use crate::commands::host_config::{
    TuiAttachEligibility, TuiAttacher, build_host_config, deferred_attach_refusal_reason,
    tui_attach_eligibility,
};
use crate::commands::support::{
    HOST_WORKER_JOIN_TIMEOUT, edit_config_file, exit_restored_tui, join_host_worker_bounded,
    join_tui_host_worker, resolved_workspace_path,
};
use crate::shell::services::Services;
use crate::{
    CliFailure, CliResult, ProcessOutcome, TuiPresentationBundle, build_identity, carry, commands,
    home, local_time_zone, openrouter_product, product_agent, product_config, product_host,
    product_onboarding, product_paths, product_session_log, product_sessions, shell, store,
    surface_protocol, tui, tui_timing,
};

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
#[allow(
    clippy::too_many_arguments,
    reason = "Round settlement joins the host and records unresolved input against the exact session, store, workspace, and terminal owner."
)]
fn settle_tui_round(
    ui: Result<tui::TuiOutcome, tui::TuiError>,
    persistent_tui: &mut Option<tui::PersistentTerminal>,
    join: plakous_hosted::runtime::HostWorkerHandle,
    host_mailbox: surface_protocol::FaceActionSender,
    label: &str,
    store: &dyn store::session_records::SessionRecordStore,
    session_id: &str,
    cwd: &Path,
) -> CliResult<Result<tui::TuiOutcome, tui::TuiError>> {
    if matches!(ui, Ok(tui::TuiOutcome::Request { .. })) {
        if !matches!(
            &ui,
            Ok(tui::TuiOutcome::Request {
                request: tui::SessionRequest::ReloadRelease { .. }
                    | tui::SessionRequest::Rebind { .. },
                ..
            })
        ) {
            drop(host_mailbox);
            if let Err(error) = join_host_worker_bounded(join, HOST_WORKER_JOIN_TIMEOUT, label) {
                if let Some(terminal) = persistent_tui.take() {
                    let _ = terminal.close();
                }
                tui::report_background_panic();
                return Err(error);
            }
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
        Ok(tui::TuiOutcome::Request {
            request: tui::SessionRequest::ReloadRelease { .. } | tui::SessionRequest::Rebind { .. },
            ..
        })
    ) {
        let _ = writeln!(stdout, "reloading…");
        let _ = stdout.flush();
        if matches!(
            &ui,
            Ok(tui::TuiOutcome::Request {
                request: tui::SessionRequest::ReloadRelease { .. },
                ..
            })
        ) {
            // A reload is a planned handoff: the host records each node with
            // work in flight as handed off before it releases (spec 03 §14.1).
            host_mailbox.request_handoff_and_release();
        } else {
            host_mailbox.request_abandon_and_release();
        }
        if join_host_worker_bounded(join, tui_timing::remaining_exit_budget(), label).is_err() {
            tui::report_background_panic();
        }
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
        exit_restored_tui(join, host_mailbox, tui::report_background_panic);
    }
    host_mailbox.request_abandon_and_release();
    drop(host_mailbox);
    let joined = join_tui_host_worker(join, label);
    if joined.is_err() {
        tui::report_background_panic();
    }
    let cleanup = combine_cleanup(joined, restoration);
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

pub(crate) fn restore_former_attachment(
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
/// The launcher's `record` is the session the process started on. `/new`,
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

#[allow(
    clippy::too_many_arguments,
    reason = "Handoff application mutates separately borrowed record, resume flags, rollback attachment, and note while preserving caller ownership."
)]
pub(crate) fn apply_tui_handoff(
    services: &Services,
    paths: &product_paths::PlakPaths,
    store: &dyn store::session_records::SessionRecordStore,
    record: &mut product_sessions::SessionRecord,
    resume: &mut bool,
    resume_pending: &mut bool,
    former_attachment: &mut Option<(product_sessions::SessionRecord, bool, bool)>,
    handoff: tui::SessionRequest,
    reload_note: &mut Option<String>,
) -> CliResult<()> {
    let selected_exact_session = matches!(&handoff, tui::SessionRequest::ResumeSession { .. });
    let former = selected_exact_session.then(|| (record.clone(), *resume, *resume_pending));
    let carried = matches!(&handoff, tui::SessionRequest::Rebind { .. });
    let previous_id = record.id.clone();
    handle_tui_handoff(services, paths, store, record, handoff, reload_note)?;
    if carried && record.id != previous_id {
        *resume = true;
        *resume_pending = true;
        *former_attachment = None;
    }
    if let Some(former) = former {
        *former_attachment = Some(former);
        *resume = true;
        *resume_pending = true;
    }
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "Post-host handoff restores a pending attachment before forwarding the separately borrowed session, resume flags, and note."
)]
pub(crate) fn apply_tui_handoff_after_host(
    services: &Services,
    paths: &product_paths::PlakPaths,
    store: &dyn store::session_records::SessionRecordStore,
    record: &mut product_sessions::SessionRecord,
    resume: &mut bool,
    resume_pending: &mut bool,
    former_attachment: &mut Option<(product_sessions::SessionRecord, bool, bool)>,
    handoff: tui::SessionRequest,
    reload_note: &mut Option<String>,
) -> CliResult<()> {
    // A failed pending resume must not become the rollback source for another
    // exact selection. Successful publication already cleared the former
    // attachment and `resume_pending` before this boundary.
    if *resume_pending
        && former_attachment.is_some()
        && !matches!(&handoff, tui::SessionRequest::Rebind { .. })
    {
        let _ = restore_former_attachment(record, resume, resume_pending, former_attachment);
    }
    apply_tui_handoff(
        services,
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

fn run_persistent_tui(
    terminal: &mut Option<tui::PersistentTerminal>,
    launch: tui::TuiLaunch,
    actions: &surface_protocol::FaceActionSender,
    events: surface_protocol::HostEventReceiver,
    lookup: &LookupPublication,
) -> Result<tui::TuiOutcome, tui::TuiError> {
    if terminal.is_none() {
        *terminal = Some(tui::PersistentTerminal::open(launch.mouse_enabled)?);
    }
    let Some(terminal) = terminal.as_mut() else {
        return Err(tui::TuiError::Io(
            "persistent TUI terminal was not initialized".into(),
        ));
    };
    lookup.attach(events.publisher());
    // Keep the caller's sender. Dropping the last sender is Disconnected, and
    // the host treats that as Quit; which shutdown the round requests on the
    // mailbox is `settle_tui_round`'s call once the terminal is cooked.
    let outcome = terminal.run(launch, Some(actions.clone()), Some(events));
    lookup.detach();
    outcome
}

/// The launch's pre-bind lookup progress, published to the face of the
/// round on screen. It is installed for the whole launch, so a lookup
/// retried between rounds is kept for the next face and never written under
/// the TUI.
struct LookupPublication {
    slot: Arc<std::sync::Mutex<LookupSlot>>,
}

#[derive(Default)]
struct LookupSlot {
    text: Option<String>,
    face: Option<plakous_hosted::host_service::FacePublisher<surface_protocol::PlakExtension>>,
}

impl LookupPublication {
    fn install() -> Self {
        let slot = Arc::new(std::sync::Mutex::new(LookupSlot::default()));
        let published = Arc::clone(&slot);
        crate::lookup_status::publish_to(Some(Box::new(move |text| {
            let mut slot = published
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            slot.text.clone_from(&text);
            if let Some(face) = &slot.face {
                publish_lookup(face, text);
            }
        })));
        Self { slot }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LookupSlot> {
        self.slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The round's face takes the progress from now on, starting with the
    /// line already showing.
    fn attach(
        &self,
        face: plakous_hosted::host_service::FacePublisher<surface_protocol::PlakExtension>,
    ) {
        let mut slot = self.lock();
        if slot.text.is_some() {
            publish_lookup(&face, slot.text.clone());
        }
        slot.face = Some(face);
    }

    fn detach(&self) {
        self.lock().face = None;
    }
}

impl Drop for LookupPublication {
    fn drop(&mut self) {
        crate::lookup_status::publish_to(None);
    }
}

fn publish_lookup(
    face: &plakous_hosted::host_service::FacePublisher<surface_protocol::PlakExtension>,
    text: Option<String>,
) {
    face.publish(surface_protocol::HostEvent::Extension(
        surface_protocol::SurfaceObservation::LookupStatus { text },
    ));
    tui::wake::wake();
}

/// Shell status while `plak resume` waits for the operator to pick a session.
const RESUME_PICKER_SHELL_REASON: &str = "pick a session to resume · Esc keeps this new session";

/// Asks the host for the cwd-scoped session list. The TUI itself marks the
/// request outstanding through `TuiLaunch::open_resume_picker`; only that pair
/// opens the picker when `HostEvent::ResumeSessions` arrives.
fn request_resume_picker(action_tx: &surface_protocol::FaceActionSender) -> CliResult<()> {
    action_tx
        .send(surface_protocol::UserAction::ResumeSession { id: None })
        .map_err(|_| CliFailure::new("resume picker host is unavailable"))
}

/// What stays fixed for every round of one TUI launch.
pub(super) struct TuiLaunchContext {
    pub(super) paths: product_paths::PlakPaths,
    pub(super) store: Arc<dyn store::session_records::SessionRecordStore>,
    pub(super) state_roots: product_agent::ProductStateRoots,
    pub(super) cwd: PathBuf,
    pub(super) authoritative_catalog_admission: bool,
}

/// What one round of a TUI launch hands to the next.
pub(super) struct TuiRoundState {
    pub(super) record: product_sessions::SessionRecord,
    pub(super) resume: bool,
    pub(super) resume_pending: bool,
    pub(super) open_resume_picker: bool,
    pub(super) former_attachment: Option<(product_sessions::SessionRecord, bool, bool)>,
    pub(super) persistent_tui: Option<tui::PersistentTerminal>,
    pub(super) tui_presentation_cache: Option<(
        Instant,
        Vec<plakous_host_protocol::views::ProviderDef>,
        Vec<plakous_host_protocol::views::ProviderSection>,
    )>,
    pub(super) defer_startup: bool,
    pub(super) declined_resume_picker: bool,
    pub(super) pending_note: Option<String>,
}

#[allow(
    clippy::too_many_lines,
    reason = "The TUI round loop owns deferred preparation, host attachment, terminal reuse, resume rollback, and handoff settlement in order."
)]
pub(super) fn run_tui_rounds(
    services: &Services,
    launch: TuiLaunchContext,
    mut state: TuiRoundState,
    attacher: &impl TuiAttacher,
) -> CliResult<ProcessOutcome> {
    let TuiLaunchContext {
        paths,
        store,
        state_roots,
        cwd,
        authoritative_catalog_admission,
    } = launch;
    let lookup = LookupPublication::install();
    loop {
        // The flag lives for exactly one round: the round the operator declined
        // the picker sets it, and the round the product starts on their behalf
        // consumes it.
        let declined_resume_round = std::mem::take(&mut state.declined_resume_picker);
        let cfg = product_config::ProductConfig::load(&paths)
            .map_err(|error| CliFailure::new(format!("invalid product config: {error}")))?;

        // Reload session body (transcript/queue) and merge freshest bindings.
        // TUI/shell write both session + config.toml; config is the binding source of
        // truth after a profile's Use now, disk session carries conversation state.
        if let Ok(updated) = store.load(&state.record.id)
            && updated.id == state.record.id
        {
            // Reload the complete product authority. Selectively copying UI
            // fields can discard the stable create operation or an unresolved
            // delivery intent after an attach/retry boundary.
            state.record = updated;
        }
        crate::build_features::admit_session(&state.record)?;
        if !state.resume_pending && state.record.store_revision != 0 {
            store.save(&mut state.record).map_err(CliFailure::new)?;
        }

        let account = state.record.account.clone();
        let model_id = state.record.model.clone();
        let effort_src = state.record.effort.as_deref();
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
        let (providers, model_sections) = if state.defer_startup {
            (supported_provider_rows(), Vec::new())
        } else if let Some((at, providers, sections)) = &state.tui_presentation_cache
            && at.elapsed() < Duration::from_secs(45)
        {
            (providers.clone(), sections.clone())
        } else {
            let (providers, sections) = tui_presentation_values(&paths)?;
            state.tui_presentation_cache =
                Some((Instant::now(), providers.clone(), sections.clone()));
            (providers, sections)
        };
        if state.record.plakous_locator.is_none()
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
            state.record.effort.clone_from(&effort_label);
            store.save(&mut state.record).map_err(CliFailure::new)?;
        }
        let launch_base = tui::TuiLaunch {
            version: plakous::VERSION,
            session_id: Some(state.record.id.clone()),
            workspace: cwd.clone(),
            workspace_chrome: surface_protocol::WorkspaceChrome {
                path: cwd.clone(),
                home: home::paths::user_home(),
                ..Default::default()
            },
            open_resume_picker: false,
            account: account.clone(),
            model: model_id.clone(),
            effort: effort_label,
            extra: (state.record.target_selection
                == product_config::ProductTargetSelection::NativePriority)
                .then(|| "priority".to_owned()),
            composition: state.record.composition.into(),
            protocol: match &state.record.target_selection {
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
                state.record.composition,
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
            settings: Some(shell::settings::settings_view(&cfg)),
            providers,
            usage_decoration: Some(crate::config::UsageDecorationReader::new(
                paths.clone(),
                local_time_zone(),
                tui::wake::wake,
            )),
            time_zone: local_time_zone(),
            // This round only: the note explains the round the product started
            // on the operator's behalf, and must not reappear on the next one.
            opening_note: state
                .pending_note
                .take()
                .or_else(|| declined_resume_round.then(|| tui::DECLINED_RESUME_NOTE.to_owned())),
            reload_home: Some(paths.clone()),
        };

        let ready_catalog = plakous_host_protocol::views::ReadyCatalog::new(model_sections.clone());
        // Reasoning OAuth models require explicit effort before agent attach
        // (picker enforces; legacy configs without effort stay on shell).
        let attach_eligibility = if authoritative_catalog_admission {
            tui_attach_eligibility(
                &state.record.target_selection,
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

        let resume_publication = state
            .resume_pending
            .then(|| Arc::new(std::sync::atomic::AtomicBool::new(false)));
        let attach = if state.defer_startup {
            Err(HostFailure::from("deferred startup"))
        } else if can_attach {
            attacher.attach(
                &paths,
                &state_roots,
                state.record.clone(),
                account.as_deref(),
                model_id.as_deref(),
                effort,
                effort_explicit,
                &cwd,
                &ready_catalog,
                resume_publication.clone(),
            )
        } else if model_needs_explicit_effort {
            Err(HostFailure::from("explicit reasoning effort is required"))
        } else {
            Err(
                commands::host_config::native_credential_refusal(account.as_deref())
                    .unwrap_or_else(|| {
                        HostFailure::from(
                            "selected model is absent from the discovered Ready catalog",
                        )
                    }),
            )
        };

        let clear_publication = Arc::new(std::sync::Mutex::new(None::<String>));
        let result = if state.defer_startup {
            state.defer_startup = false;
            let prep_paths = paths.clone();
            let prep_state_roots = state_roots.clone();
            let prep_record = state.record.clone();
            let prep_cwd = cwd.clone();
            let prep_account = account.clone();
            let prep_model = model_id.clone();
            let prep_resume_picker = state.open_resume_picker;
            let deferred_resume_publication = resume_publication.clone();
            let prep_needs_setup = product_onboarding::needs_setup(&cfg);
            let (action_tx, event_rx, join) = surface_protocol::face_of(
                product_host::spawn_deferred_host_with_clear_publication(
                    move || {
                        prepare_deferred_tui_host(
                            prep_paths,
                            prep_state_roots,
                            prep_record,
                            prep_cwd,
                            prep_account,
                            prep_model,
                            prep_resume_picker,
                            deferred_resume_publication,
                            prep_needs_setup,
                            effort,
                            effort_explicit,
                        )
                    },
                    Some(Arc::clone(&clear_publication)),
                ),
            );
            let mut launch = launch_base;
            if state.open_resume_picker {
                request_resume_picker(&action_tx)?;
                launch.open_resume_picker = true;
                state.open_resume_picker = false;
            }
            let ui = run_persistent_tui(
                &mut state.persistent_tui,
                launch,
                &action_tx,
                event_rx,
                &lookup,
            );
            let ui = settle_tui_round(
                ui,
                &mut state.persistent_tui,
                join,
                action_tx,
                "TUI controller",
                store.as_ref(),
                &state.record.id,
                &cwd,
            )?;
            if resume_publication
                .as_ref()
                .is_some_and(|publication| publication.load(std::sync::atomic::Ordering::Acquire))
            {
                state.resume_pending = false;
            }
            ui
        } else {
            match attach {
                Ok((action_tx, event_rx, join)) => {
                    let mut launch = launch_base;
                    launch.agent_enabled = true;
                    if state.open_resume_picker {
                        request_resume_picker(&action_tx)?;
                        launch.open_resume_picker = true;
                        state.open_resume_picker = false;
                    }
                    let ui = run_persistent_tui(
                        &mut state.persistent_tui,
                        launch,
                        &action_tx,
                        event_rx,
                        &lookup,
                    );
                    let ui = settle_tui_round(
                        ui,
                        &mut state.persistent_tui,
                        join,
                        action_tx,
                        "attached session",
                        store.as_ref(),
                        &state.record.id,
                        &cwd,
                    )?;
                    // Channels prove only that the worker started. The exact
                    // resume becomes authoritative only after canonical
                    // runtime open and durable product publication set this
                    // acknowledgement inside the host.
                    if resume_publication.as_ref().is_some_and(|publication| {
                        publication.load(std::sync::atomic::Ordering::Acquire)
                    }) {
                        state.former_attachment = None;
                        state.resume_pending = false;
                    }
                    ui
                }
                Err(attach_err) => {
                    if restore_former_attachment(
                        &mut state.record,
                        &mut state.resume,
                        &mut state.resume_pending,
                        &mut state.former_attachment,
                    ) {
                        continue;
                    }
                    // The operator declined the resume picker and this
                    // workspace has no attachable default. Opening another
                    // not-ready shell would strand them,
                    // so quit through the ordinary exit path instead.
                    if declined_resume_round {
                        return Ok(finish_declined_resume_launch(
                            &mut state.persistent_tui,
                            store.as_ref(),
                            &state.record.id,
                            &cwd,
                            attach_err.diagnostic.as_str(),
                        ));
                    }
                    let reason = if model_needs_explicit_effort && !effort_explicit {
                        DetachedReason::EffortRequired
                    } else if product_onboarding::needs_setup(&cfg) {
                        DetachedReason::MissingDefaults
                    } else if matches!(attach_err.remedy.as_ref(), Some(plakous_host_protocol::notice_remedy::NoticeRemedy::RepairCredentialOrChooseProfile { .. })) {
                        DetachedReason::CredentialUnusable(attach_err.clone())
                    } else {
                        DetachedReason::ResolveFailed(attach_err.clone())
                    };
                    product_session_log::emit(
                        &paths.session_home(),
                        &state.record.id,
                        product_session_log::SessionEvent::new("host.attach_failed")
                            .opt_str("account", account.as_deref())
                            .opt_str("model", model_id.as_deref())
                            .str("reason", reason.kind()),
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
                                session: state.record.clone(),
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
                    if state.open_resume_picker {
                        request_resume_picker(&action_tx)?;
                        launch.open_resume_picker = true;
                        state.open_resume_picker = false;
                    }
                    let ui = run_persistent_tui(
                        &mut state.persistent_tui,
                        launch,
                        &action_tx,
                        event_rx,
                        &lookup,
                    );
                    settle_tui_round(
                        ui,
                        &mut state.persistent_tui,
                        join,
                        action_tx,
                        "shell session",
                        store.as_ref(),
                        &state.record.id,
                        &cwd,
                    )?
                }
            }
        };

        if let Some(outcome) = finish_tui_round(
            services,
            &paths,
            store.as_ref(),
            &mut state,
            result,
            account.as_deref(),
            model_id.as_deref(),
            &clear_publication,
        )? {
            return Ok(outcome);
        }
    }
}

/// The host preparation a deferred-startup round runs off the UI thread.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "Deferred preparation owns its moved paths, state roots, session, binding hints, picker choice, publication fence, and effort on the worker thread."
)]
fn prepare_deferred_tui_host(
    prep_paths: product_paths::PlakPaths,
    prep_state_roots: product_agent::ProductStateRoots,
    prep_record: product_sessions::SessionRecord,
    prep_cwd: PathBuf,
    prep_account: Option<String>,
    prep_model: Option<String>,
    prep_resume_picker: bool,
    deferred_resume_publication: Option<Arc<std::sync::atomic::AtomicBool>>,
    prep_needs_setup: bool,
    effort: ReasoningEffort,
    effort_explicit: bool,
) -> product_host::HostLaunchPreparation<surface_protocol::PlakExtension> {
    let shell_host = shell::host::ProductShellHost::boxed(&prep_paths);
    if prep_resume_picker {
        let mode = product_host::HostLaunchMode::Shell(product_host::ShellHostConfig {
            surface: Some(shell::settings::ProductSurface::boxed(&prep_paths)),
            home: prep_paths.session_home(),
            binding: shell::binding_document::ProductRootBindingDocument::shared(&prep_paths),
            subscriptions: shell::host::open_product_subscriptions(),
            openrouter_credentials: openrouter_product::ProductOpenRouterCredentials::shared(
                &prep_paths,
            ),
            store: prep_state_roots.session_records(),
            product_home: prep_paths.home().to_path_buf(),
            session: prep_record,
            cwd: prep_cwd,
            reason: RESUME_PICKER_SHELL_REASON.into(),
            ready_catalog: plakous_host_protocol::views::ReadyCatalog::default(),
        });
        let catalog_refresh = shell::catalog_refresh::hook(prep_paths.clone(), &mode);
        return product_host::HostLaunchPreparation {
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
        tui_presentation_bundle(&prep_paths).unwrap_or_else(|_| TuiPresentationBundle {
            providers: supported_provider_rows(),
            sections: plakous_host_protocol::views::ReadyCatalog::default(),
            native_models: Vec::new(),
        });
    let presentation_ms =
        u64::try_from(presentation_started.elapsed().as_millis()).unwrap_or(u64::MAX);
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
                    resolve_ms: u64::try_from(resolve_started.elapsed().as_millis())
                        .unwrap_or(u64::MAX),
                };
                product_host::HostLaunchMode::Attached(config)
            }
            Err(error) => product_host::HostLaunchMode::Shell(product_host::ShellHostConfig {
                surface: Some(shell::settings::ProductSurface::boxed(&prep_paths)),
                product_home: prep_paths.home().to_path_buf(),
                home: prep_paths.session_home(),
                binding: shell::binding_document::ProductRootBindingDocument::shared(&prep_paths),
                subscriptions: shell::host::open_product_subscriptions(),
                openrouter_credentials: openrouter_product::ProductOpenRouterCredentials::shared(
                    &prep_paths,
                ),
                store: prep_state_roots.session_records(),
                session: prep_record,
                cwd: prep_cwd,
                reason: DetachedReason::ResolveFailed(error.host_failure()),
                ready_catalog: sections.clone(),
            }),
        }
    } else {
        product_host::HostLaunchMode::Shell(product_host::ShellHostConfig {
            surface: Some(shell::settings::ProductSurface::boxed(&prep_paths)),
            home: prep_paths.session_home(),
            binding: shell::binding_document::ProductRootBindingDocument::shared(&prep_paths),
            subscriptions: shell::host::open_product_subscriptions(),
            openrouter_credentials: openrouter_product::ProductOpenRouterCredentials::shared(
                &prep_paths,
            ),
            store: prep_state_roots.session_records(),
            product_home: prep_paths.home().to_path_buf(),
            session: prep_record,
            cwd: prep_cwd,
            reason: deferred_attach_refusal_reason(
                eligibility,
                prep_needs_setup,
                (eligibility == TuiAttachEligibility::Unavailable && !prep_needs_setup)
                    .then(|| {
                        commands::host_config::native_credential_refusal(prep_account.as_deref())
                    })
                    .flatten(),
            ),
            ready_catalog: sections.clone(),
        })
    };
    let catalog_refresh = shell::catalog_refresh::hook(prep_paths.clone(), &mode);
    product_host::HostLaunchPreparation {
        providers,
        mode,
        catalog_refresh: Some(catalog_refresh),
        shell_host,
    }
}

/// Settles one round: re-attach, apply a handoff, or end the launch.
#[allow(
    clippy::too_many_arguments,
    reason = "Round finishing combines services, storage, mutable round state, UI outcome, binding labels, and publication cleanup after the host has joined."
)]
fn finish_tui_round(
    services: &Services,
    paths: &product_paths::PlakPaths,
    store: &dyn store::session_records::SessionRecordStore,
    state: &mut TuiRoundState,
    result: Result<tui::TuiOutcome, tui::TuiError>,
    account: Option<&str>,
    model_id: Option<&str>,
    clear_publication: &std::sync::Mutex<Option<String>>,
) -> CliResult<Option<ProcessOutcome>> {
    match result {
        Ok(tui::TuiOutcome::Request {
            request:
                tui::SessionRequest::RetryAttach {
                    declined_resume_picker: declined,
                },
            attached_session,
        }) => {
            state.declined_resume_picker = declined;
            product_session_log::emit(
                &paths.session_home(),
                &state.record.id,
                product_session_log::SessionEvent::new("host.reattach_requested")
                    .opt_str("account", account)
                    .opt_str("model", model_id),
            );
            // The not-ready shell loop breaks before it can publish a
            // binding, so its clear target arrives on its own channel.
            let clear_target = clear_publication
                .lock()
                .map_err(|_| CliFailure::new("clear handoff publication was poisoned"))?
                .take();
            adopt_attached_session(
                store,
                &mut state.record,
                &mut state.resume,
                &mut state.resume_pending,
                &mut state.former_attachment,
                clear_target.as_deref().or(attached_session.as_deref()),
            )?;
        }
        Ok(tui::TuiOutcome::Request {
            request: handoff,
            attached_session,
        }) => {
            settle_left_reload_requests(paths, &handoff);
            if matches!(
                &handoff,
                tui::SessionRequest::EditConfig
                    | tui::SessionRequest::ReloadRelease { .. }
                    | tui::SessionRequest::Rebind { .. }
            ) {
                state.persistent_tui = None;
            }
            adopt_attached_session(
                store,
                &mut state.record,
                &mut state.resume,
                &mut state.resume_pending,
                &mut state.former_attachment,
                attached_session.as_deref(),
            )?;
            apply_tui_handoff_after_host(
                services,
                paths,
                store,
                &mut state.record,
                &mut state.resume,
                &mut state.resume_pending,
                &mut state.former_attachment,
                handoff,
                &mut state.pending_note,
            )?;
            state.tui_presentation_cache = None;
        }
        Ok(tui::TuiOutcome::Exited { .. }) => return Ok(Some(ProcessOutcome::Success)),
        Err(err) => return Err(CliFailure::new(err.to_string())),
    }
    Ok(None)
}

/// A round that resumes another session leaves its face's reload requests
/// behind: they belong to the session it left, whose host has ended, so
/// they are settled `not taken` (spec 02 §5.1). A reload carries its own
/// request in its handoff, and a resume of the same session leaves none.
fn settle_left_reload_requests(paths: &product_paths::PlakPaths, handoff: &tui::SessionRequest) {
    let tui::SessionRequest::ResumeSession {
        id,
        held_reload_requests,
    } = handoff
    else {
        return;
    };
    for request in held_reload_requests
        .iter()
        .filter(|request| request.session_id != *id)
    {
        crate::reload_request::settle_switched(paths, &request.request_id);
    }
}

pub(crate) fn handle_tui_handoff(
    services: &Services,
    paths: &product_paths::PlakPaths,
    store: &dyn store::session_records::SessionRecordStore,
    record: &mut product_sessions::SessionRecord,
    handoff: tui::SessionRequest,
    reload_note: &mut Option<String>,
) -> CliResult<()> {
    match handoff {
        tui::SessionRequest::Rebind { cause } => {
            match carry::rebind_session(services, record, &cause) {
                Ok((next, note)) => {
                    *record = next;
                    *reload_note = Some(note);
                }
                Err(error) => {
                    *reload_note = Some(format!(
                        "rebind kept this session open: {}",
                        error.message()
                    ));
                }
            }
        }
        tui::SessionRequest::EditConfig => {
            edit_config_file(paths)?;
        }
        tui::SessionRequest::ReloadRelease { target } => {
            *reload_note = Some(reload_installed_plak(paths, store, record, target)?);
        }
        // The supervisor settles a retry before it reaches here.
        tui::SessionRequest::RetryAttach { .. } => {}
        tui::SessionRequest::ResumeSession { id, .. } => {
            let next = carry::switch_to(services, store, &id, reload_note)?;
            let requested = resolved_workspace_path(&record.workspace_root)?;
            let recorded = resolved_workspace_path(&next.workspace_root)?;
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
    let Some(mut target) = target else {
        return Err(CliFailure::new("reload: no installed plak found on PATH"));
    };
    if let Some(reason) = carry::reverify_released(&mut target, &record.workspace_root, &record.id)
    {
        return Ok(reason);
    }
    if let Some(reason) = target.refusal(&home::state::state_key()) {
        return Ok(reason);
    }
    let binding_change = target.binding_change().map(str::to_owned);
    let executable = target.executable;
    let installed_key = target.state_key;
    // The loop's copy can trail the store: the host commits every turn's
    // head itself, and an export of a stale record is refused as behind it.
    let stored = store.load(&record.id).ok();
    let resumable = stored.is_some();
    if let Some(stored) = stored {
        *record = stored;
    }
    let plan = carry::reload_plan(
        resumable,
        &home::state::state_key(),
        installed_key.as_deref(),
        binding_change.is_some(),
    );
    let mut command = reload_command(&executable, &plan, &record.id);
    std::os::unix::process::CommandExt::arg0(&mut command, build_identity::invocation_argv0());
    let mut carry_file = None;
    let mut unverified = None;
    let mut handoff = carry::handoff::ReloadHandoff::default();
    handoff.request = target.request.map(|request| *request);
    match plan {
        carry::ReloadPlan::Fresh => {}
        carry::ReloadPlan::Resume => {}
        carry::ReloadPlan::ResumeUnverified => {
            unverified = Some(format!(
                "; {} did not report its state key, so {} was resumed without carrying it forward",
                executable.display(),
                record.id
            ));
        }
        carry::ReloadPlan::Carry => {
            let (path, carried) = match carry::carry_out(paths, record) {
                Ok(path) => path,
                Err(reason) => {
                    let why = if binding_change.is_some() {
                        "the installed build cannot reopen this session exactly"
                    } else {
                        "the new build reads another state format"
                    };
                    return Ok(format!(
                        "reload kept this session open: {why}, and {reason}"
                    ));
                }
            };
            let _ = writeln!(io::stdout(), "carrying {}", carried.summary);
            let _ = io::stdout().flush();
            handoff.carrying(
                paths,
                record,
                binding_change.as_deref(),
                carried.finished_subagents,
            );
            command.arg("--import-resume").arg(&path);
            carry_file = Some(path);
        }
    }
    let handoff_file = match handoff.pass(paths, &record.id, &mut command) {
        Ok(handoff_file) => handoff_file,
        Err(reason) => {
            remove_failed_handoff_receipt(carry_file.as_deref());
            let _ = carry_file.map(std::fs::remove_file);
            return Ok(format!("reload kept this session open: {reason}"));
        }
    };
    tracing::info!(
        target: "plak.reload",
        executable = %executable.display(),
        session = %record.id,
        carried = carry_file.is_some(),
        "exec installed plak"
    );
    let error = std::os::unix::process::CommandExt::exec(&mut command);
    let _ = handoff_file.map(std::fs::remove_file);
    remove_failed_handoff_receipt(carry_file.as_deref());
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

fn reload_command(
    executable: &Path,
    plan: &carry::ReloadPlan,
    session: &str,
) -> std::process::Command {
    let mut command = std::process::Command::new(executable);
    command.env_remove(crate::environment::RELOAD_EPOCH_ENV);
    if matches!(
        plan,
        carry::ReloadPlan::Resume | carry::ReloadPlan::ResumeUnverified
    ) {
        command.env(crate::environment::RELOAD_EPOCH_ENV, "1");
        command.arg("--resume").arg(session);
    }
    command
}

fn remove_failed_handoff_receipt(path: Option<&Path>) {
    if let Some(path) = path {
        let _ = std::fs::remove_file(path.with_extension("handoff"));
    }
}

#[cfg(test)]
mod tests;
