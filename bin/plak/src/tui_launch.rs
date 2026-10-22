//! The interactive TUI: launch, the attach/handoff round loop, reload and teardown.

use std::env;
use std::io::{self, IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use plakous::agent::ReasoningEffort;

#[cfg(test)]
use crate::PresentationBackend;
use crate::commands::host_config::{
    NativeTuiAttacher, TuiAttachEligibility, TuiAttacher, build_host_config,
    deferred_attach_refusal_reason, tui_attach_eligibility,
};
use crate::commands::session::{repair_invalid_new_session_binding, seed_new_session_binding};
use crate::commands::support::{
    HOST_WORKER_JOIN_TIMEOUT, edit_config_file, exit_restored_tui, join_host_worker_bounded,
    join_tui_host_worker,
};
use crate::shell::services::Services;
use crate::{
    CliFailure, CliFailureKind, CliResult, ProcessOutcome, TuiPresentationBundle, build_identity,
    carry, cli, commands, home, local_time_zone, openrouter_product, presentation, product_agent,
    product_config, product_host, product_onboarding, product_paths, product_session_log,
    product_sessions, product_workspace, shell, signals, store, surface_protocol, tui, tui_timing,
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

pub(crate) fn apply_tui_handoff(
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

pub(crate) fn apply_tui_handoff_after_host(
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

pub(crate) fn launch_tui(
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

pub(crate) fn launch_tui_with_attacher(
    services: &Services,
    session_id: Option<String>,
    resume_picker: bool,
    opening_note: Option<String>,
    attacher: &impl TuiAttacher,
) -> CliResult<ProcessOutcome> {
    refuse_without_terminal(services, session_id.as_deref(), resume_picker)?;
    let (launch, rounds) =
        prepare_tui_launch(services, session_id, resume_picker, opening_note, attacher)?;
    run_tui_rounds(launch, rounds, attacher)
}

/// What stays fixed for every round of one TUI launch.
struct TuiLaunchContext {
    paths: product_paths::PlakPaths,
    store: Arc<dyn store::session_records::SessionRecordStore>,
    state_roots: product_agent::ProductStateRoots,
    cwd: PathBuf,
    authoritative_catalog_admission: bool,
}

/// What one round of a TUI launch hands to the next.
struct TuiRoundState {
    record: product_sessions::SessionRecord,
    resume: bool,
    resume_pending: bool,
    open_resume_picker: bool,
    former_attachment: Option<(product_sessions::SessionRecord, bool, bool)>,
    persistent_tui: Option<tui::PersistentTerminal>,
    tui_presentation_cache: Option<(
        Instant,
        Vec<plakous_host_protocol::views::ProviderDef>,
        Vec<plakous_host_protocol::views::ProviderSection>,
    )>,
    defer_startup: bool,
    declined_resume_picker: bool,
    pending_note: Option<String>,
}

fn refuse_without_terminal(
    services: &Services,
    session_id: Option<&str>,
    resume_picker: bool,
) -> CliResult<()> {
    if !rustix::termios::isatty(io::stdin()) || !io::stdout().is_terminal() {
        // A piped or scripted `plak` is most people's first contact with the
        // product, and a bare refusal is a poor one. The exit code is a
        // contract and stays 2; what changes is that the reader gets the same
        // command summary `plak help` prints, and then the one command that
        // does what they were trying to do.
        let mut stderr = io::stderr().lock();
        cli::render::global_help(&mut stderr).map_err(|_| CliFailure::new("stderr failed"))?;
        let guidance = if let Some(session_id) = session_id {
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
    Ok(())
}

fn prepare_tui_launch(
    services: &Services,
    session_id: Option<String>,
    resume_picker: bool,
    opening_note: Option<String>,
    attacher: &impl TuiAttacher,
) -> CliResult<(TuiLaunchContext, TuiRoundState)> {
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
    let open_resume_picker = resume_picker;
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
    let resume = session_id.is_some();
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
    let resume_pending = begin_tui_session_launch(&paths, &mut record, resume, &cwd);
    tui_timing::attach(&paths, &record.id, resume);
    build_identity::prepare_binary_id(None);
    tui::warm_highlighting();

    // Re-attach loop: profile Use now and `/attach` return TuiOutcome::RetryAttach so we
    // re-resolve OAuth and spawn the live agent without the operator quitting plak.
    let tui_presentation_cache: Option<(
        Instant,
        Vec<plakous_host_protocol::views::ProviderDef>,
        Vec<plakous_host_protocol::views::ProviderSection>,
    )> = None;
    let former_attachment: Option<(product_sessions::SessionRecord, bool, bool)> = None;
    let persistent_tui: Option<tui::PersistentTerminal> = None;
    // New and exact-resume launches paint a disabled shell before provider,
    // catalog, or runtime preparation. Attachment is claimed only after the
    // deferred host has authoritatively opened the selected runtime.
    let authoritative_catalog_admission = attacher.supports_deferred_preparation();
    let defer_startup = authoritative_catalog_admission;
    // Set when the operator declined the launch resume picker: this round is
    // the product attaching the workspace default on their behalf, so a failure
    // to attach quits cleanly rather than parking them in a shell they never
    // asked for.
    let declined_resume_picker = false;
    // One line for the first round that can show it: why the session moved
    // builds, or why a reload kept it here.
    let pending_note = opening_note.or_else(|| {
        resume
            .then(|| carry::carried_forward_label(&record))
            .flatten()
    });
    Ok((
        TuiLaunchContext {
            paths,
            store,
            state_roots,
            cwd,
            authoritative_catalog_admission,
        },
        TuiRoundState {
            record,
            resume,
            resume_pending,
            open_resume_picker,
            former_attachment,
            persistent_tui,
            tui_presentation_cache,
            defer_startup,
            declined_resume_picker,
            pending_note,
        },
    ))
}

fn run_tui_rounds(
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
            workspace_chrome: product_workspace::workspace_label(&cwd),
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
            settings: Some(shell::settings::settings_snapshot(&cfg)),
            providers,
            telemetry_paths: Some(paths.clone()),
            time_zone: local_time_zone(),
            // This round only: the note explains the round the product started
            // on the operator's behalf, and must not reappear on the next one.
            opening_note: state
                .pending_note
                .take()
                .or_else(|| declined_resume_round.then(|| tui::DECLINED_RESUME_NOTE.to_owned())),
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
            Err("deferred startup".to_owned())
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
            let (action_tx, event_rx, join) =
                product_host::spawn_deferred_tui_host_with_clear_publication(
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
                );
            let mut launch = launch_base;
            if state.open_resume_picker {
                request_resume_picker(&action_tx)?;
                launch.open_resume_picker = true;
                state.open_resume_picker = false;
            }
            let ui = run_persistent_tui(&mut state.persistent_tui, launch, &action_tx, event_rx);
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
                    let ui =
                        run_persistent_tui(&mut state.persistent_tui, launch, &action_tx, event_rx);
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
                        &state.record.id,
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
                    let ui =
                        run_persistent_tui(&mut state.persistent_tui, launch, &action_tx, event_rx);
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
) -> product_host::TuiHostPreparation<surface_protocol::PlakExtension> {
    let shell_host = shell::host::ProductShellHost::boxed(&prep_paths);
    if prep_resume_picker {
        let mode = product_host::TuiHostMode::Shell(product_host::ShellHostConfig {
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
            reason: RESUME_PICKER_SHELL_REASON.to_owned(),
            ready_catalog: plakous_host_protocol::views::ReadyCatalog::default(),
        });
        let catalog_refresh = shell::catalog_refresh::hook(prep_paths.clone(), &mode);
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
                product_host::TuiHostMode::Attached(config)
            }
            Err(error) => product_host::TuiHostMode::Shell(product_host::ShellHostConfig {
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
                reason: format!("resolve_failed: {error}"),
                ready_catalog: sections.clone(),
            }),
        }
    } else {
        product_host::TuiHostMode::Shell(product_host::ShellHostConfig {
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
    product_host::TuiHostPreparation {
        providers,
        mode,
        catalog_refresh: Some(catalog_refresh),
        shell_host,
    }
}

/// Settles one round: re-attach, apply a handoff, or end the launch.
fn finish_tui_round(
    paths: &product_paths::PlakPaths,
    store: &dyn store::session_records::SessionRecordStore,
    state: &mut TuiRoundState,
    result: Result<tui::TuiOutcome, tui::TuiError>,
    account: Option<&str>,
    model_id: Option<&str>,
    clear_publication: &std::sync::Mutex<Option<String>>,
) -> CliResult<Option<ProcessOutcome>> {
    match result {
        Ok(tui::TuiOutcome::RetryAttach {
            attached_session,
            declined_resume_picker: declined,
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
        Ok(tui::TuiOutcome::Handoff {
            handoff,
            attached_session,
        }) => {
            if matches!(
                &handoff,
                tui::TuiHandoff::EditConfig | tui::TuiHandoff::Reload { .. }
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

fn tui_presentation_values(
    paths: &product_paths::PlakPaths,
) -> CliResult<(
    Vec<plakous_host_protocol::views::ProviderDef>,
    Vec<plakous_host_protocol::views::ProviderSection>,
)> {
    presentation::tui_presentation_values(paths)
}

#[cfg(test)]
pub(crate) fn tui_presentation_values_with_backend(
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
    crate::capabilities::ROUTES
        .iter()
        .map(|capability| {
            (
                capability.route.account,
                capability.label,
                capability.auth_kind(),
            )
        })
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

pub(crate) fn handle_tui_handoff(
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
            let (path, summary) = match carry::carry_out(paths, record) {
                Ok(path) => path,
                Err(reason) => {
                    return Ok(format!(
                        "reload kept this session open: the new build reads another state format, and {reason}"
                    ));
                }
            };
            let _ = writeln!(io::stdout(), "carrying {summary}");
            let _ = io::stdout().flush();
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

fn remove_failed_handoff_receipt(path: Option<&Path>) {
    if let Some(path) = path {
        let _ = std::fs::remove_file(path.with_extension("handoff"));
    }
}

#[cfg(test)]
mod tests;
