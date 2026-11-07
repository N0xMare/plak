//! Terminal ownership for a socket client. No session lease or handoff is
//! acquired here: closing or replacing this process only drops its face.

use std::io::Write as _;
use std::sync::Arc;

use super::socket::SocketFace;
use crate::shell::services::Services;
use crate::surface_protocol::tui_channel::{TuiActionSender, TuiEventReceiver};
use crate::{CliFailure, CliResult, ProcessOutcome, tui};

pub(super) fn run(
    services: &Services,
    mut face: Arc<SocketFace>,
    mut session: Option<String>,
    picker: bool,
    mut note: Option<String>,
) -> CliResult<ProcessOutcome> {
    crate::signals::install_interactive(tui::force_restore_terminal);
    let paths = services.paths()?;
    note = refuse_reload_handoff(paths, &face).or(note);
    if let Some(requested) = session.as_deref() {
        let store = services.session_records()?;
        let workspace = paths.workspace().map_err(CliFailure::new)?;
        // Cross-build import is a CLI act that creates a new directory row;
        // it never opens the resident session the host already owns.
        if super::names_no_local_session(store.as_ref(), workspace, requested)
            && let Some((imported, imported_note)) =
                crate::carry::carry_foreign_session(services, paths, workspace, requested)?
        {
            session = Some(imported);
            note = Some(imported_note);
        }
    }
    if !picker || session.is_some() {
        face.select(session.as_deref())?;
    } else {
        face.list_sessions()?;
    }
    let mut cfg = load_config(paths)?;
    crate::build_identity::prepare_binary_id(None);
    tui::warm_highlighting();
    let mut terminal = tui::PersistentTerminal::open(cfg.tui_mouse)
        .map_err(|error| CliFailure::new(error.to_string()))?;
    let mut picker = picker;
    loop {
        let launch = launch(paths, &cfg, &face, std::mem::take(&mut picker), note.take())?;
        let result = terminal.run(
            launch,
            Some(TuiActionSender::Socket(Arc::clone(&face))),
            Some(TuiEventReceiver::Socket(Arc::clone(&face))),
        );
        match result {
            Ok(tui::TuiOutcome::Exited { attached_session }) => {
                let errors = terminal.close();
                if !errors.is_empty() {
                    return Err(CliFailure::new(errors.join("; ")));
                }
                if let Some(session) = attached_session.or_else(|| face.session()) {
                    let _ = writeln!(
                        std::io::stdout(),
                        "Detached. Resume this session with: plak resume {session}"
                    );
                }
                let _ = std::io::stdout().flush();
                return Ok(if crate::signals::stop_reason().is_some() {
                    ProcessOutcome::Interrupted
                } else {
                    ProcessOutcome::Success
                });
            }
            Ok(tui::TuiOutcome::Request {
                request,
                attached_session,
            }) => match request {
                tui::SessionRequest::ResumeSession { id, .. } => {
                    face = reconnect(paths, &face)?;
                    face.select(Some(&id))?;
                }
                tui::SessionRequest::RetryAttach {
                    declined_resume_picker,
                } => {
                    let selected = face.session();
                    face = reconnect(paths, &face)?;
                    picker = retry_selection(&face, selected.as_deref(), declined_resume_picker)?;
                    cfg = load_config(paths)?;
                }
                tui::SessionRequest::EditConfig => {
                    let errors = terminal.close();
                    if !errors.is_empty() {
                        return Err(CliFailure::new(errors.join("; ")));
                    }
                    crate::commands::support::edit_config_file(paths)?;
                    cfg = load_config(paths)?;
                    let selected = face.session();
                    face = reconnect(paths, &face)?;
                    picker = select_or_list(&face, selected.as_deref())?;
                    terminal = tui::PersistentTerminal::open(cfg.tui_mouse)
                        .map_err(|error| CliFailure::new(error.to_string()))?;
                }
                tui::SessionRequest::ReloadRelease { target } => {
                    let errors = terminal.close();
                    if !errors.is_empty() {
                        return Err(CliFailure::new(errors.join("; ")));
                    }
                    let session = attached_session.or_else(|| face.session());
                    drop(face);
                    return reload(target, session.as_deref());
                }
                tui::SessionRequest::Rebind { cause } => {
                    face.rebind(cause)?;
                }
            },
            Err(error) => {
                let _ = terminal.close();
                return Err(CliFailure::new(error.to_string()));
            }
        }
    }
}

fn load_config(
    paths: &crate::product_paths::PlakPaths,
) -> CliResult<crate::product_config::ProductConfig> {
    crate::product_config::ProductConfig::load(paths).map_err(CliFailure::new)
}

fn refuse_reload_handoff(
    paths: &crate::product_paths::PlakPaths,
    face: &SocketFace,
) -> Option<String> {
    let request = crate::reload_request::take_handoff()?;
    let reason = face.reload_refusal(true);
    crate::reload_request::record(
        paths,
        &request.request_id,
        &crate::reload_request::ReloadOutcome::Refused {
            reason: reason.clone(),
        },
    );
    Some(format!("requested reload refused: {reason}"))
}

fn launch(
    paths: &crate::product_paths::PlakPaths,
    cfg: &crate::product_config::ProductConfig,
    face: &SocketFace,
    picker: bool,
    note: Option<String>,
) -> CliResult<tui::TuiLaunch> {
    let workspace = paths.workspace().map_err(CliFailure::new)?.to_path_buf();
    let settings = crate::shell::settings::settings_view(cfg);
    Ok(tui::TuiLaunch {
        host_mode: true,
        version: plakous::VERSION,
        session_id: face.session(),
        workspace: workspace.clone(),
        workspace_chrome: crate::surface_protocol::WorkspaceChrome {
            path: workspace,
            home: crate::home::paths::user_home(),
            ..Default::default()
        },
        open_resume_picker: picker,
        agent_enabled: face.session().is_some(),
        mouse_enabled: cfg.tui_mouse,
        theme: settings.tui.theme,
        settings: Some(settings),
        usage_decoration: Some(crate::config::UsageDecorationReader::new(
            paths.clone(),
            crate::local_time_zone(),
            tui::wake::wake,
        )),
        time_zone: crate::local_time_zone(),
        reload_home: Some(paths.clone()),
        opening_note: note,
        ..Default::default()
    })
}

fn retry_selection(
    face: &SocketFace,
    selected: Option<&str>,
    declined_resume_picker: bool,
) -> CliResult<bool> {
    if declined_resume_picker && selected.is_none() {
        face.select(None)?;
        return Ok(false);
    }
    select_or_list(face, selected)
}

fn select_or_list(face: &SocketFace, selected: Option<&str>) -> CliResult<bool> {
    if let Some(session) = selected {
        face.reattach(session)?;
        return Ok(false);
    }
    face.list_sessions()?;
    Ok(true)
}

fn reconnect(
    paths: &crate::product_paths::PlakPaths,
    previous: &SocketFace,
) -> CliResult<Arc<SocketFace>> {
    let face = SocketFace::discover(paths)?.ok_or_else(|| CliFailure::new("the host disconnected; restart it, then re-attach with `plak resume`; no embedded session was started"))?;
    face.restore_new_request(previous.take_new_request());
    Ok(face)
}

fn reload(
    target: Option<crate::carry::ReloadTarget>,
    session: Option<&str>,
) -> CliResult<ProcessOutcome> {
    let _ = writeln!(std::io::stdout(), "reloading…");
    let _ = std::io::stdout().flush();
    let target =
        target.ok_or_else(|| CliFailure::new("reload: no installed plak found on PATH"))?;
    let mut command = std::process::Command::new(&target.executable);
    std::os::unix::process::CommandExt::arg0(
        &mut command,
        crate::build_identity::invocation_argv0(),
    );
    command.env_remove(super::RELOAD_EPOCH_ENV);
    if let Some(session) = session {
        command.arg("--resume").arg(session);
    }
    let error = std::os::unix::process::CommandExt::exec(&mut command);
    Err(CliFailure::new(format!(
        "reload: could not start {}: {error}",
        target.executable.display()
    )))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "socket fixture")]
mod tests;
