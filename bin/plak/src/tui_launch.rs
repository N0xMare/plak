//! The interactive TUI's launch: the terminal check, the first round's
//! preparation, and the presentation values every round reads. The rounds
//! themselves are the session supervisor's ([`supervisor`]) when embedded;
//! a running home's host instead supplies the socket face ([`hosted`]).

pub(crate) mod face;
mod hosted;
pub(crate) mod socket;
pub(crate) mod supervisor;

pub(crate) const HOST_RESUME_SUPPORTED: bool = true;

use std::io::{self, IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::Instant;

#[cfg(test)]
use crate::PresentationBackend;
use crate::commands::host_config::{NativeTuiAttacher, TuiAttacher};
use crate::commands::session::seed_new_session_binding;
use crate::shell::services::Services;
use crate::{
    CliFailure, CliFailureKind, CliResult, ProcessOutcome, TuiPresentationBundle, build_identity,
    carry, cli, commands, home, presentation, product_config, product_paths, product_sessions,
    signals, store, tui, tui_timing,
};

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

pub(crate) fn launch_host_tui(
    services: &Services,
    session_id: Option<String>,
) -> CliResult<ProcessOutcome> {
    refuse_without_terminal(services, session_id.as_deref(), false)?;
    let face = socket::SocketFace::discover(services.paths()?)?.ok_or_else(|| {
        CliFailure::new(format!(
            "the host stopped; start it, then plak resume{}",
            session_id
                .as_ref()
                .map_or_else(String::new, |id| format!(" {id}")),
        ))
    })?;
    hosted::run(services, face, session_id, false, None)
}

pub(crate) fn launch_placed_tui(
    services: &Services,
    runner: Option<&str>,
    workspace: Option<&str>,
) -> CliResult<ProcessOutcome> {
    refuse_without_terminal(services, None, false)?;
    let face = socket::SocketFace::discover(services.paths()?)?.ok_or_else(|| {
        CliFailure::new("--runner/--workspace requires a running host; start `plak host` first; no embedded session was started")
    })?;
    face.place(runner, workspace)?;
    hosted::run(services, face, None, false, None)
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
    if let Some(face) = socket::SocketFace::discover(services.paths()?)? {
        return hosted::run(services, face, session_id, resume_picker, opening_note);
    }
    let _upgrade_admission = carry::upgrade::admission(services.paths()?, false)?;
    let (launch, rounds) =
        prepare_tui_launch(services, session_id, resume_picker, opening_note, attacher)?;
    supervisor::run_tui_rounds(services, launch, rounds, attacher)
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

#[allow(
    clippy::too_many_lines,
    reason = "TUI launch preparation validates terminal-facing selection, product state, catalog authority, and the initial round before starting the host."
)]
fn prepare_tui_launch(
    services: &Services,
    session_id: Option<String>,
    resume_picker: bool,
    opening_note: Option<String>,
    attacher: &impl TuiAttacher,
) -> CliResult<(supervisor::TuiLaunchContext, supervisor::TuiRoundState)> {
    // Armed before the terminal is taken, so no `SIGTERM` or `SIGHUP` can
    // meet the default disposition while the alternate screen is up.
    signals::install_interactive(tui::force_restore_terminal);
    let paths = services.paths()?.clone();
    let store = services.session_records()?;
    home::state::ensure_usable_state(&paths, &mut io::stderr().lock()).map_err(CliFailure::new)?;
    let state_roots = services.state_roots_for(&paths)?;
    let cwd = paths.workspace().map_err(CliFailure::new)?.to_path_buf();
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
    if resume {
        record = carry::intent::resume(services, Some(&cwd), record, &mut opening_note)?;
    }

    if resume
        && crate::environment::reload_epoch_requested()
        && let Some(note) = carry::apply_on_reload(services, &mut record)
    {
        opening_note = Some(match opening_note {
            Some(previous) => format!("{previous}; {note}"),
            None => note,
        });
    }

    crate::reload_request::complete_handoff(&paths, &record, resume);

    let initial_cfg = product_config::ProductConfig::load(&paths)
        .map_err(|error| CliFailure::new(format!("invalid product config: {error}")))?;
    for warning in &initial_cfg.warnings {
        let _ = writeln!(io::stderr().lock(), "warning: {warning}");
    }
    if !resume {
        seed_new_session_binding(&mut record, &initial_cfg)?;
        crate::build_features::admit_selected(
            &initial_cfg,
            record.account.as_deref(),
            record.model.as_deref(),
            record.composition,
        )?;
    }

    if resume {
        let requested = resolved_workspace_path(&cwd)?;
        let recorded = resolved_workspace_path(&record.workspace_root)?;
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

    // Re-attach loop: profile Use now and `/attach` ask the supervisor to retry
    // the attach, which re-resolves OAuth and spawns the live agent without the
    // operator quitting plak.
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
        supervisor::TuiLaunchContext {
            paths,
            store,
            state_roots,
            cwd,
            authoritative_catalog_admission,
        },
        supervisor::TuiRoundState {
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

/// What a reload sets on the build it starts when that build applies changed
/// orchestration settings; a process-local handoff marker, never session
/// authority (spec 02 §5.1).
const RELOAD_EPOCH_ENV: &str = "PLAK_RELOAD_APPLY_ORCHESTRATION";

/// A workspace path as the workspace probe resolves it, or as recorded when
/// it does not resolve. A probe that cannot answer refuses the resume rather
/// than compare a path that only looks unresolvable.
fn resolved_workspace_path(path: &Path) -> CliResult<PathBuf> {
    let probe: &dyn plakous::tools::WorkspaceProbe = &plakous::tools::LocalWorkspaceProbe;
    Ok(probe
        .canonicalize(path)
        .map_err(|error| CliFailure::new(format!("workspace could not be inspected: {error}")))?
        .unwrap_or_else(|| path.to_path_buf()))
}

#[cfg(test)]
mod tests;
