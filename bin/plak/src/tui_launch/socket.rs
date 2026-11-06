//! The TUI's local-socket face: directory actions belong to the client,
//! while session observations and intents use the ordinary wire client.

use std::collections::{BTreeMap, VecDeque};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use plakous_host_protocol::client::{self, Transport, WireReceiver, WireSender};
use plakous_host_protocol::views::transcript::{NoticeScope, TranscriptRow};
use plakous_host_protocol::wire::{
    Answer, Hello, Op, Outcome, RequestId, SessionRow, WIRE_VERSION,
};

use crate::surface_protocol::{HostEvent, PlakExtension, UserAction, UserActionSendError};
use crate::{CliFailure, CliResult, build_identity, product_paths};

const DEADLINE: Duration = Duration::from_secs(30);

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
    session: Option<String>,
    pending: BTreeMap<RequestId, (Option<UserAction>, Instant)>,
    events: VecDeque<HostEvent>,
    unsent: VecDeque<client::SendError<PlakExtension>>,
    listener: Option<(String, crate::reload_request::Listener)>,
}

impl SocketFace {
    pub(super) fn discover(paths: &product_paths::PlakPaths) -> CliResult<Option<Arc<Self>>> {
        let stream = match UnixStream::connect(paths.host_socket()) {
            Ok(stream) => stream,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
            {
                return Ok(None);
            }
            Err(error) => {
                return Err(CliFailure::new(format!(
                    "the host socket could not be reached: {error}"
                )));
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(io_failure)?;
        let reader = stream.try_clone().map_err(io_failure)?;
        let writer = stream.try_clone().map_err(io_failure)?;
        let hello = Hello {
            proto: WIRE_VERSION,
            binary_id: build_identity::known_binary_id().unwrap_or_else(|| "unknown".into()),
            caps: Vec::new(),
        };
        let (sender, receiver, welcome) = client::connect_directory::<PlakExtension>(Transport::new(reader, writer), hello)
            .map_err(|error| CliFailure::new(format!("the host handshake failed: {error}; stop the host with `plak host stop`, or run its build; no embedded session was started")))?;
        if welcome.proto != WIRE_VERSION {
            return Err(CliFailure::new(
                "the host uses another wire version; stop it with `plak host stop`, or run its build; no embedded session was started",
            ));
        }
        stream.set_read_timeout(None).map_err(io_failure)?;
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

    pub(super) fn select(&self, requested: Option<&str>) -> CliResult<()> {
        let answer = if let Some(requested) = requested {
            let Answer::Sessions { rows } = self.call(self.list())? else {
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
            self.attach(row.session.clone(), row.node.clone())?;
            return Ok(());
        } else {
            self.call(self.new_session())?
        };
        if let Answer::Created { session, node, .. } = answer {
            self.attach(session, node)?;
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

    fn list(&self) -> Op<PlakExtension> {
        Op::ListSessions {
            workspace: Some(self.workspace.clone()),
        }
    }

    fn new_session(&self) -> Op<PlakExtension> {
        Op::NewSession {
            workspace: self.workspace.clone(),
            template: None,
            request_id: uuid::Uuid::new_v4().to_string(),
        }
    }

    fn attach(
        &self,
        session: String,
        node: plakous_host_protocol::wire::NodeAddress,
    ) -> CliResult<()> {
        self.sender.attach_to(node).map_err(io_failure)?;
        self.state().session = Some(session);
        Ok(())
    }

    pub(crate) fn session(&self) -> Option<String> {
        self.state().session.clone()
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

    #[allow(clippy::result_large_err, reason = "return the user's unsent action")]
    pub(crate) fn send(&self, action: UserAction) -> Result<(), UserActionSendError> {
        let op = match &action {
            UserAction::NewSession => Some(self.new_session()),
            UserAction::ResumeSession { .. } => Some(self.list()),
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
            let mut state = self.state();
            let Some(id) = self.sender.request_operation(op) else {
                return Err(UserActionSendError::Disconnected(action));
            };
            state.pending.insert(id, (Some(action), Instant::now()));
            return Ok(());
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
                if target.state_key.as_deref() != Some(key.as_str()) {
                    Some("reload refused: the installed client reads another state key; stop the host or run its build".to_owned())
                } else {
                    target.refusal(&key)
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
            let Some((action, _)) = self.state().pending.remove(&id) else {
                continue;
            };
            let Some(action) = action else {
                let text = match outcome {
                    Outcome::Ok(Answer::Stopped) => "session work stopped".to_owned(),
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
                    if self.attach(session, node).is_err() {
                        self.state()
                            .unsent
                            .push_back(client::SendError::OutcomeUnknown(action));
                    }
                }
                Outcome::Ok(Answer::Sessions { rows }) => self.sessions(action, rows),
                Outcome::Ok(_) => {}
                Outcome::Refused(_) => self.state().unsent.push_back(client::SendError::Refused(
                    UserActionSendError::Refused(action),
                )),
            }
        }
    }

    fn refuse_requested_reload(&self) {
        let mut state = self.state();
        let Some(session) = state.session.clone() else {
            return;
        };
        if state
            .listener
            .as_ref()
            .is_none_or(|(id, listener)| *id != session || !listener.is_current())
        {
            state.listener = crate::reload_request::Listener::announce(&self.paths, &session)
                .map(|listener| (session.clone(), listener));
        }
        let request = state
            .listener
            .as_ref()
            .and_then(|(_, listener)| listener.take(&self.paths, &session));
        if let Some(request) = request {
            let build = self
                .host_binary
                .as_ref()
                .map_or_else(String::new, |binary| format!(" (build {binary})"));
            let reason = format!(
                "this session runs in plak host{build}; restarting the host moves it onto the installed build (plak host stop, or restart its unit)"
            );
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

    fn sessions(&self, action: UserAction, rows: Vec<SessionRow>) {
        if let UserAction::ResumeSession {
            id: Some(requested),
        } = &action
        {
            let ids: Vec<&str> = rows.iter().map(|row| row.session.as_str()).collect();
            if let Ok(id) = crate::commands::session::match_session_id_prefix(requested, &ids)
                && let Some(row) = rows.iter().find(|row| row.session == id)
                && self.attach(row.session.clone(), row.node.clone()).is_ok()
            {
                return;
            }
            self.state().unsent.push_back(client::SendError::Refused(
                UserActionSendError::Refused(action),
            ));
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
            if let Some((Some(action), _)) = state.pending.remove(&id) {
                state
                    .unsent
                    .push_back(client::SendError::OutcomeUnknown(action));
            }
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

fn io_failure(error: std::io::Error) -> CliFailure {
    CliFailure::new(format!("the host socket failed: {error}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "socket fixture")]
mod tests;
