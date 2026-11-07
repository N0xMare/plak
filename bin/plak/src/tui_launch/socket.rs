//! The TUI's local-socket face: directory actions belong to the client,
//! while session observations and intents use the ordinary wire client.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use plakous_host_protocol::client::{self, WireReceiver, WireSender};
use plakous_host_protocol::views::transcript::{NoticeScope, TranscriptRow};
use plakous_host_protocol::wire::{
    Answer, Hello, Op, Outcome, RequestId, SessionRow, WIRE_VERSION,
};

use crate::surface_protocol::{HostEvent, PlakExtension, UserAction, UserActionSendError};
use crate::{CliFailure, CliResult, build_identity, product_paths};

const DEADLINE: Duration = Duration::from_secs(30);

mod discovery;

/// The client-side directory and session attachment of one terminal.
pub struct SocketFace {
    sender: WireSender<PlakExtension>,
    receiver: WireReceiver<PlakExtension>,
    workspace: String,
    paths: product_paths::PlakPaths,
    host_binary: Option<String>,
    state: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    creation: Option<CreationSelection>,
    host_notice_probe_started: bool,
    host_notice: Option<String>,
    session: Option<String>,
    pending: BTreeMap<RequestId, (Option<UserAction>, Instant)>,
    attachments: BTreeMap<RequestId, (String, Instant)>,
    new_request: Option<String>,
    events: VecDeque<HostEvent>,
    unsent: VecDeque<client::SendError<PlakExtension>>,
    listener: Option<(String, crate::reload_request::Listener)>,
}

#[derive(Clone)]
struct CreationSelection {
    runner: plakous_host_protocol::wire::RunnerLabel,
    workspace: String,
    profile: Option<String>,
}

impl SocketFace {
    pub(super) fn discover(paths: &product_paths::PlakPaths) -> CliResult<Option<Arc<Self>>> {
        let Some(transport) = discovery::connect(paths, Duration::from_secs(10))? else {
            return Ok(None);
        };
        let hello = Hello {
            proto: WIRE_VERSION,
            binary_id: build_identity::known_binary_id().unwrap_or_else(|| "unknown".into()),
            caps: Vec::new(),
        };
        let (sender, receiver, welcome) = client::connect_directory::<PlakExtension>(transport, hello)
            .map_err(|error| match error {
                client::ConnectError::HandshakeTimeout(_) => CliFailure::new(error.to_string()),
                other => CliFailure::new(format!("the host handshake failed: {other}; stop the host with `plak host stop`, or run its build; no embedded session was started")),
            })?;
        if welcome.proto != WIRE_VERSION {
            return Err(CliFailure::new(
                "the host uses another wire version; stop it with `plak host stop`, or run its build; no embedded session was started",
            ));
        }
        Ok(Some(Arc::new(Self {
            sender,
            receiver,
            workspace: paths
                .workspace()
                .map_err(CliFailure::new)?
                .to_string_lossy()
                .into_owned(),
            paths: paths.clone(),
            host_binary: (!welcome.binary_id.is_empty() && welcome.binary_id != "unknown")
                .then_some(welcome.binary_id),
            state: Arc::default(),
        })))
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn place(&self, runner: Option<&str>, workspace: Option<&str>) -> CliResult<()> {
        use plakous_host_protocol::wire::{WorkspaceOptions, WorkspaceProbe};
        let Answer::Options { options } = self.call(Op::SessionOptions { workspace: None })? else {
            return Err(CliFailure::new(
                "the host did not answer with session options",
            ));
        };
        let requested = runner.unwrap_or("local");
        let row = options
            .runners
            .iter()
            .find(|row| {
                row.runner.name == requested || row.aliases.iter().any(|alias| alias == requested)
            })
            .ok_or_else(|| {
                CliFailure::new(format!(
                    "runner {requested} was not returned by the host; no session was created"
                ))
            })?;
        let path = workspace
            .map(str::to_owned)
            .or_else(|| {
                row.recent_workspaces
                    .first()
                    .map(|recent| recent.path.clone())
            })
            .or_else(|| row.default_workspace.clone())
            .ok_or_else(|| {
                CliFailure::new("the runner has no default workspace; supply --workspace")
            })?;
        let Answer::Options { options: validated } = self.call(Op::SessionOptions {
            workspace: Some(WorkspaceProbe {
                runner: row.runner.clone(),
                path,
            }),
        })?
        else {
            return Err(CliFailure::new("the host did not validate the workspace"));
        };
        let canonical_path = match validated.workspace {
            Some(WorkspaceOptions::Valid { canonical_path, .. }) => canonical_path,
            Some(WorkspaceOptions::Refused { refusal }) => {
                return Err(CliFailure::new(format!(
                    "workspace refused: {refusal:?}; no session was created"
                )));
            }
            None => {
                return Err(CliFailure::new(
                    "the host omitted workspace validation; no session was created",
                ));
            }
        };
        let mut state = self.state();
        state.new_request = None;
        state.creation = Some(CreationSelection {
            runner: row.runner.clone(),
            workspace: canonical_path,
            profile: options.profiles.current,
        });
        Ok(())
    }

    pub(super) fn select(&self, requested: Option<&str>) -> CliResult<()> {
        if requested.is_some() {
            self.state().new_request = None;
        }
        self.select_or_create(requested)
    }

    pub(super) fn reattach(&self, session: &str) -> CliResult<()> {
        self.select_or_create(Some(session))
    }

    fn select_or_create(&self, requested: Option<&str>) -> CliResult<()> {
        let answer = if let Some(requested) = requested {
            let Answer::Sessions { rows } = self.call(Op::ListSessions {
                workspace: None,
                runner: None,
                include_archived: false,
            })?
            else {
                return Err(CliFailure::new(
                    "the host did not answer with its session directory",
                ));
            };
            let ids: Vec<&str> = rows.iter().map(|row| row.session.as_str()).collect();
            let id = crate::commands::session::match_session_id_prefix(requested, &ids)
                .map_err(CliFailure::new)?;
            let row = rows
                .iter()
                .find(|row| row.session == id)
                .ok_or_else(|| CliFailure::new("the selected session left the directory"))?;
            refuse_elsewhere(row)?;
            self.state().creation = Some(CreationSelection {
                runner: row.runner.clone(),
                workspace: row.workspace.clone(),
                profile: None,
            });
            self.select_root(row.session.clone(), row.node.clone())?;
            return Ok(());
        } else {
            self.call(self.new_session())?
        };
        if let Answer::Created { session, node, .. } = answer {
            self.state().new_request = None;
            self.select_root(session, node)?;
            return Ok(());
        }
        Err(CliFailure::new(
            "the host did not answer new_session with created",
        ))
    }

    fn call(&self, op: Op<PlakExtension>) -> CliResult<Answer> {
        let id = self
            .sender
            .request_operation(op)
            .ok_or_else(|| CliFailure::new("the host disconnected"))?;
        let (answered, outcome) = self
            .receiver
            .recv_response(DEADLINE)
            .map_err(|error| CliFailure::new(format!("the host did not answer: {error}")))?;
        if answered != id {
            return Err(CliFailure::new("the host answered another request"));
        }
        match outcome {
            Outcome::Ok(answer) => Ok(answer),
            Outcome::Refused(reason) => {
                Err(CliFailure::new(format!("the host refused: {reason:?}")))
            }
        }
    }

    fn select_root(
        &self,
        session: String,
        node: plakous_host_protocol::wire::NodeAddress,
    ) -> CliResult<()> {
        let id = self.sender.attach_to(node).map_err(io_failure)?;
        let (answered, outcome) = self.receiver.recv_response(DEADLINE).map_err(|error| {
            CliFailure::new(format!("the host did not answer the attach: {error}"))
        })?;
        if answered != id {
            return Err(CliFailure::new("the host answered another attach"));
        }
        match outcome {
            Outcome::Ok(Answer::Attached { .. }) => {
                self.state().session = Some(session);
                Ok(())
            }
            other => Err(CliFailure::new(format!("attach refused: {other:?}"))),
        }
    }

    fn list(&self) -> Op<PlakExtension> {
        let selected = self.state().creation.clone();
        Op::ListSessions {
            runner: Some(
                selected
                    .as_ref()
                    .map_or("local", |selection| selection.runner.runner_id.as_str())
                    .to_owned(),
            ),
            include_archived: false,
            workspace: Some(
                selected.map_or_else(|| self.workspace.clone(), |selection| selection.workspace),
            ),
        }
    }

    fn new_session(&self) -> Op<PlakExtension> {
        let mut state = self.state();
        let selected = state.creation.clone();
        let request_id = state
            .new_request
            .get_or_insert_with(|| uuid::Uuid::new_v4().to_string())
            .clone();
        Op::NewSession {
            runner: selected.as_ref().map_or_else(
                plakous_host_protocol::wire::RunnerLabel::local,
                |selection| selection.runner.clone(),
            ),
            profile: selected
                .as_ref()
                .and_then(|selection| selection.profile.clone()),
            model: None,
            workspace: selected
                .map_or_else(|| self.workspace.clone(), |selection| selection.workspace),
            template: None,
            request_id,
        }
    }

    fn attach(
        &self,
        session: String,
        node: plakous_host_protocol::wire::NodeAddress,
    ) -> CliResult<()> {
        let mut state = self.state();
        let id = self.sender.attach_to(node).map_err(io_failure)?;
        state.attachments.insert(id, (session, Instant::now()));
        Ok(())
    }

    pub(crate) fn session(&self) -> Option<String> {
        self.responses();
        self.state().session.clone()
    }

    pub(super) fn take_new_request(&self) -> Option<String> {
        self.state().new_request.take()
    }
    pub(super) fn restore_new_request(&self, request: Option<String>) {
        self.state().new_request = request;
    }
    pub(crate) fn ended(&self) -> Option<String> {
        self.receiver.ended()
    }

    pub(super) fn probe_host_notice(&self) {
        if std::mem::replace(&mut self.state().host_notice_probe_started, true) {
            return;
        }
        let paths = self.paths.clone();
        let workspace = self.workspace.clone();
        let host = self.host_binary.clone();
        let state = Arc::clone(&self.state);
        std::thread::spawn(move || {
            let installed =
                crate::carry::probe_reload(&paths, std::path::Path::new(&workspace), None, None).0;
            let installed = installed
                .as_ref()
                .and_then(|target| target.probe.as_ref())
                .and_then(|probe| probe.binary_id.as_deref());
            // The result belongs to this connection, never a later face.
            state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .host_notice = outdated_host_notice(host.as_deref(), installed);
            crate::tui::wake::wake();
        });
    }

    pub(crate) fn host_notice(&self) -> Option<String> {
        self.state().host_notice.clone()
    }

    fn note(&self, text: String) {
        self.state()
            .events
            .push_back(HostEvent::Row(TranscriptRow::warning(
                NoticeScope::Settings,
                text,
            )));
    }

    pub(super) fn rebind(&self, cause: String) -> CliResult<()> {
        self.sender
            .request_session(client::SessionChange::Rebind(cause))
            .ok_or_else(|| CliFailure::new("the host disconnected before rebind"))?;
        Ok(())
    }

    pub(crate) fn stop_session(&self, session: &str) -> bool {
        let mut state = self.state();
        let Some(id) = self.sender.request_operation(Op::StopSession {
            session: session.to_owned(),
        }) else {
            return false;
        };
        state.pending.insert(id, (None, Instant::now()));
        true
    }

    pub(super) fn list_sessions(&self) -> CliResult<()> {
        self.request(UserAction::ResumeSession { id: None }, self.list())
            .map_err(|_| CliFailure::new("the host disconnected before listing sessions"))
    }

    #[allow(clippy::result_large_err, reason = "return the user's unsent action")]
    fn request(
        &self,
        action: UserAction,
        op: Op<PlakExtension>,
    ) -> Result<(), UserActionSendError> {
        let mut state = self.state();
        let Some(id) = self.sender.request_operation(op) else {
            return Err(UserActionSendError::Disconnected(action));
        };
        state.pending.insert(id, (Some(action), Instant::now()));
        Ok(())
    }

    #[allow(clippy::result_large_err, reason = "return the user's unsent action")]
    pub(crate) fn send(&self, action: UserAction) -> Result<(), UserActionSendError> {
        let op = match &action {
            UserAction::NewSession => Some(self.new_session()),
            UserAction::ResumeSession { .. } => {
                self.state().new_request = None;
                Some(self.list())
            }
            UserAction::Extension(crate::surface_protocol::SurfaceIntent::ProbeReload) => {
                self.probe_reload();
                return Ok(());
            }
            // Build probes are client-local, never remote-face intents.
            UserAction::Extension(crate::surface_protocol::SurfaceIntent::CheckBuild) => {
                self.refuse_requested_reload();
                self.state().events.push_back(HostEvent::Extension(
                    crate::surface_protocol::SurfaceObservation::BuildStatus {
                        newer_installed: build_identity::newer_build_installed(),
                    },
                ));
                return Ok(());
            }
            _ => None,
        };
        if let Some(op) = op {
            return self.request(action, op);
        }
        self.sender.send(action).map_err(|error| match error {
            client::SendError::Refused(refused) => refused,
            other => UserActionSendError::Disconnected(other.into_intent()),
        })
    }

    fn probe_reload(&self) {
        let paths = self.paths.clone();
        let workspace = self.workspace.clone();
        let state = Arc::clone(&self.state);
        std::thread::spawn(move || {
            let target =
                crate::carry::probe_reload(&paths, std::path::Path::new(&workspace), None, None).0;
            let key = crate::home::state::state_key();
            let refusal = target.as_ref().and_then(|target| {
                if target.state_key.as_deref() == Some(key.as_str()) {
                    target.refusal(&key)
                } else {
                    Some("reload refused: the installed client reads another state key; stop the host or run its build".to_owned())
                }
            });
            state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .events
                .push_back(HostEvent::Extension(
                    crate::surface_protocol::SurfaceObservation::ReloadProbed {
                        target: target.map(Box::new),
                        refusal,
                    },
                ));
            crate::tui::wake::wake();
        });
    }

    fn responses(&self) {
        while let Some(answer) = self.receiver.try_session_answer() {
            if let Err(refusal) = answer.outcome {
                self.state()
                    .events
                    .push_back(HostEvent::Row(TranscriptRow::warning(
                        NoticeScope::Settings,
                        format!("rebind refused: {refusal:?}"),
                    )));
            }
        }
        while let Some((id, outcome)) = self.receiver.try_response() {
            let attachment = self.state().attachments.remove(&id);
            if let Some((session, _)) = attachment {
                match outcome {
                    Outcome::Ok(Answer::Attached { .. }) => self.state().session = Some(session),
                    other => self.note(format!("attach refused: {other:?}")),
                }
                continue;
            }
            let Some((action, _)) = self.state().pending.remove(&id) else {
                continue;
            };
            let Some(action) = action else {
                let text = match outcome {
                    Outcome::Ok(Answer::Stopped {
                        runner_pending: false,
                    }) => crate::tui::SESSION_STOPPED_ACK.to_owned(),
                    other => format!("stop refused: {other:?}"),
                };
                self.state()
                    .events
                    .push_back(HostEvent::Row(TranscriptRow::info(
                        NoticeScope::Settings,
                        text,
                    )));
                continue;
            };
            match outcome {
                Outcome::Ok(Answer::Created { session, node, .. }) => {
                    self.state().new_request = None;
                    if self.attach(session, node).is_err() {
                        self.state()
                            .unsent
                            .push_back(client::SendError::OutcomeUnknown(action));
                    }
                }
                Outcome::Ok(Answer::Sessions { rows }) => self.sessions(action, rows),
                Outcome::Ok(_) => {}
                Outcome::Refused(reason) => {
                    if matches!(action, UserAction::NewSession) {
                        self.state().new_request = None;
                    }
                    self.note(format!("session request refused: {reason:?}"));
                }
            }
        }
    }

    fn refuse_requested_reload(&self) {
        let mut state = self.state();
        let Some(session) = state.session.clone() else {
            return;
        };
        let request = crate::reload_request::Listener::take_remote(&self.paths, &session);
        if state
            .listener
            .as_ref()
            .is_none_or(|(id, listener)| *id != session || !listener.is_current())
        {
            state.listener =
                crate::reload_request::Listener::announce_remote(&self.paths, &session)
                    .map(|listener| (session.clone(), listener));
        }
        if let Some(request) = request {
            let reason = self.reload_refusal(false);
            crate::reload_request::record(
                &self.paths,
                &request.request_id,
                &crate::reload_request::ReloadOutcome::Refused {
                    reason: reason.clone(),
                },
            );
            state
                .events
                .push_back(HostEvent::Row(TranscriptRow::warning(
                    NoticeScope::Settings,
                    format!("requested reload refused: {reason}"),
                )));
        }
    }

    pub(super) fn reload_refusal(&self, landed: bool) -> String {
        let build = self
            .host_binary
            .as_ref()
            .map_or_else(String::new, |binary| format!(" (build {binary})"));
        let subject = if landed {
            "the session now"
        } else {
            "this session"
        };
        format!(
            "{subject} runs in plak host{build}; restarting the host moves it onto the installed build (plak host stop, or restart its unit)"
        )
    }

    fn sessions(&self, action: UserAction, rows: Vec<SessionRow>) {
        if let UserAction::ResumeSession {
            id: Some(requested),
        } = &action
        {
            let ids: Vec<&str> = rows.iter().map(|row| row.session.as_str()).collect();
            let result = crate::commands::session::match_session_id_prefix(requested, &ids)
                .map_err(CliFailure::new)
                .and_then(|id| {
                    rows.iter()
                        .find(|row| row.session == id)
                        .ok_or_else(|| CliFailure::new("the selected session left the directory"))
                })
                .and_then(|row| {
                    refuse_elsewhere(row)?;
                    self.attach(row.session.clone(), row.node.clone())
                });
            if let Err(error) = result {
                self.note(format!("resume refused: {error}"));
            }
            return;
        }
        self.state().events.push_back(HostEvent::ResumeSessions(
            rows.into_iter()
                .map(|row| plakous_host_protocol::views::ResumeSessionEntry {
                    id: row.session,
                    model: row.profile,
                    title: row.title,
                    updated_at_unix: row.updated_at,
                    in_place: true,
                    ..Default::default()
                })
                .collect(),
        ));
    }

    pub(crate) fn try_recv(&self) -> Result<HostEvent, std::sync::mpsc::TryRecvError> {
        self.responses();
        if let Some(event) = self.state().events.pop_front() {
            return Ok(event);
        }
        let event = self.receiver.try_recv()?;
        if let HostEvent::Binding {
            session_id: Some(session),
            ..
        } = &event
        {
            self.state().session = Some(session.clone());
        }
        Ok(event)
    }

    pub(crate) fn try_unsent(&self) -> Option<client::SendError<PlakExtension>> {
        self.responses();
        let mut state = self.state();
        let expired: Vec<_> = state
            .pending
            .iter()
            .filter(|(_, (_, at))| at.elapsed() >= DEADLINE || self.receiver.ended().is_some())
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            if let Some((action, _)) = state.pending.remove(&id) {
                match action {
                    Some(action) => state
                        .unsent
                        .push_back(client::SendError::OutcomeUnknown(action)),
                    None => state
                        .events
                        .push_back(HostEvent::Row(TranscriptRow::warning(
                            NoticeScope::Settings,
                            "stop outcome unknown: the host did not answer",
                        ))),
                }
            }
        }
        let expired: Vec<_> = state
            .attachments
            .iter()
            .filter(|(_, (_, at))| self.receiver.ended().is_some() || at.elapsed() >= DEADLINE)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            state.attachments.remove(&id);
            state
                .events
                .push_back(HostEvent::Row(TranscriptRow::warning(
                    NoticeScope::Settings,
                    "attach outcome unknown: the host did not answer",
                )));
        }
        state
            .unsent
            .pop_front()
            .or_else(|| self.receiver.try_unsent())
    }

    pub(crate) fn set_notify(&self, notify: impl Fn() + Send + Sync + 'static) {
        self.receiver.set_notify(notify);
    }

    pub(crate) fn transcript_resnapshot_required_for(
        &self,
        scope: &plakous_host_protocol::views::transcript::TranscriptScope,
    ) -> bool {
        self.receiver.transcript_resnapshot_required_for(scope)
    }

    pub(crate) fn settle(&self, timeout: Duration) -> bool {
        self.sender.settle(timeout)
    }
}

fn outdated_host_notice(host: Option<&str>, installed: Option<&str>) -> Option<String> {
    let (host, installed) = (host?, installed?);
    (host != installed).then(|| format!(
        "host build {host} differs from installed build {installed}; restart the host unit, or run `plak host stop` and start it again; /reload replaces only this client"
    ))
}

fn io_failure(error: std::io::Error) -> CliFailure {
    CliFailure::new(format!("the host socket failed: {error}"))
}

fn refuse_elsewhere(row: &SessionRow) -> CliResult<()> {
    if let Some(holder) = &row.hosted_elsewhere {
        return Err(CliFailure::hosted_elsewhere(format!(
            "tree_hosted_elsewhere: session {} is open in another plak process (pid {} on {}); use that process, or release the session there (release_session from its web app, or wait for its idle release), or stop that host",
            row.session, holder.pid, holder.host_name
        )));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "socket fixture")]
mod tests;
