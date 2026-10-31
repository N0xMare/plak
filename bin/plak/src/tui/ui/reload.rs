//! `/reload`: the installed-build probe runs off the UI thread.
//!
//! Asking the installed `plak` its state key spawns a process that may take
//! seconds on a first exec; the loop keeps painting and handing input while
//! it runs, then hands off (or prints the refusal) when the answer lands.

use std::sync::mpsc::{self, Receiver, TryRecvError};

use super::{App, ScrollBlock};

pub(super) const RELOAD_CHECKING: &str = "checking installed build…";

/// The session's own reload requests: its announcement, a request waiting
/// for the next idle boundary, and the one a running probe performs.
#[derive(Default)]
pub(super) struct ReloadRequests {
    /// The product home the composition root named; the channel is closed
    /// without one.
    home: Option<crate::home::paths::PlakPaths>,
    listener: Option<(String, crate::reload_request::Listener)>,
    pending: Option<crate::reload_request::ReloadRequest>,
    active: Option<crate::reload_request::ReloadRequest>,
}

impl ReloadRequests {
    pub(super) fn new(home: Option<crate::home::paths::PlakPaths>) -> Self {
        Self {
            home,
            ..Self::default()
        }
    }
}

/// What the loop does with a settled probe.
#[derive(Debug)]
pub(super) enum ReloadOutcome {
    HandOff(Option<crate::carry::ReloadTarget>),
    Refused,
}

#[derive(Eq, PartialEq)]
pub(super) struct ReloadSource {
    session: Option<String>,
    account: Option<String>,
    model: Option<String>,
    turn: Option<plakous_host_protocol::TurnLifecycle>,
    prompt: Option<std::time::Instant>,
}
impl ReloadSource {
    fn snapshot(app: &App) -> Self {
        Self {
            session: app.session_id.clone(),
            account: app.account.clone(),
            model: app.model.clone(),
            turn: app.last_turn.clone(),
            prompt: app.last_prompt_at,
        }
    }
}

impl App {
    /// Starts `probe` on its own thread unless one is already running.
    pub(super) fn start_reload_probe(
        &mut self,
        probe: impl FnOnce() -> Option<crate::carry::ReloadTarget> + Send + 'static,
    ) {
        if self.reload_probe.is_some() {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("plak-reload-probe".to_owned())
            .spawn(move || {
                let _ = sender.send(probe());
            });
        if spawned.is_err() {
            self.push_block(ScrollBlock::System(
                "reload refused: could not start the installed-build check".to_owned(),
            ));
            self.mark_dirty();
            return;
        }
        self.reload_source = Some(ReloadSource::snapshot(self));
        self.reload_probe = Some(receiver);
        self.push_block(ScrollBlock::System(RELOAD_CHECKING.to_owned()));
        self.mark_dirty();
    }

    pub(super) fn reload_source_changed(&self) -> bool {
        self.reload_source
            .as_ref()
            .is_some_and(|source| source != &ReloadSource::snapshot(self))
    }

    /// The probe's answer once it lands; `None` while it is still running.
    /// A refusal, including one for state that changed while it ran, is a
    /// transcript note and leaves the session open. A reload the session
    /// itself requested also lands on exactly the build it named, never waits
    /// on a confirmation, and reports its refusal to the requester.
    pub(super) fn poll_reload_probe(&mut self) -> Option<ReloadOutcome> {
        let outcome = self.poll_reload_probe_once();
        if self.reload_requests.active.is_none() {
            return outcome;
        }
        match outcome {
            Some(ReloadOutcome::HandOff(Some(mut target))) => {
                let request = self.reload_requests.active.take()?;
                let installed = target
                    .probe
                    .as_ref()
                    .and_then(|probe| probe.binary_id.clone());
                if installed.as_deref() != Some(request.expect_binary.as_str()) {
                    let reason = format!(
                        "the installed build is {}, not {}",
                        installed.as_deref().unwrap_or("unknown"),
                        request.expect_binary
                    );
                    self.refuse_reload_request(&request, &reason);
                    return Some(ReloadOutcome::Refused);
                }
                target.request = Some(Box::new(request));
                Some(ReloadOutcome::HandOff(Some(target)))
            }
            Some(ReloadOutcome::HandOff(None)) => {
                let request = self.reload_requests.active.take()?;
                self.refuse_reload_request(&request, "no installed plak found on PATH");
                Some(ReloadOutcome::Refused)
            }
            Some(ReloadOutcome::Refused) => {
                let request = self.reload_requests.active.take()?;
                let reason = match self.pane.blocks.last() {
                    Some(ScrollBlock::System(text)) => text.clone(),
                    _ => "the reload was refused".to_owned(),
                };
                if let Some(paths) = self.reload_requests.home.as_ref() {
                    crate::reload_request::record(
                        paths,
                        &request.request_id,
                        &crate::reload_request::ReloadOutcome::Refused { reason },
                    );
                }
                Some(ReloadOutcome::Refused)
            }
            None if self.reload_probe.is_none() && self.destructive_confirm.is_some() => {
                // A requested reload never waits on an operator's confirmation.
                self.destructive_confirm = None;
                self.reload_confirmed_target = None;
                let request = self.reload_requests.active.take()?;
                self.refuse_reload_request(
                    &request,
                    "the carried history may not fit the installed build's window; run /reload to confirm it",
                );
                Some(ReloadOutcome::Refused)
            }
            None => None,
        }
    }

    /// Announces this session as accepting reload requests (again, when its
    /// announcement vanished or another instance replaced it) and takes one,
    /// on the installed-build check's cadence.
    pub(super) fn poll_reload_requests(&mut self) {
        let Some(session) = self.session_id.clone() else {
            return;
        };
        let Some(paths) = self.reload_requests.home.clone() else {
            return;
        };
        if self
            .reload_requests
            .listener
            .as_ref()
            .is_none_or(|(id, listener)| id != &session || !listener.is_current())
        {
            // A missing or replaced announcement is re-published, so a pruned
            // or overwritten file never leaves the channel silently dead.
            self.reload_requests.listener =
                crate::reload_request::Listener::announce(&paths, &session)
                    .map(|listener| (session.clone(), listener));
        }
        let Some((_, listener)) = self.reload_requests.listener.as_ref() else {
            return;
        };
        if let Some(request) = listener.take(&paths, &session) {
            self.push_block(ScrollBlock::System(format!(
                "reload requested onto build {}; it runs when this session is idle",
                request.expect_binary
            )));
            self.mark_dirty();
            self.reload_requests.pending = Some(request);
        }
    }

    /// At an idle boundary, starts the requested reload, or refuses it when
    /// sub-agents or background processes are live: it never waits on work
    /// and never interrupts a turn.
    pub(super) fn service_reload_request(&mut self) {
        if self.busy
            || self.reload_probe.is_some()
            || self.pending_reload
            || self.reload_requests.active.is_some()
        {
            return;
        }
        let Some(request) = self.reload_requests.pending.take() else {
            return;
        };
        if let Some(reason) = self.reload_refusal() {
            let reason = reason.trim_start_matches("reload refused: ").to_owned();
            self.refuse_reload_request(&request, &reason);
            return;
        }
        self.reload_requests.active = Some(request);
        self.pending_reload = true;
    }

    fn refuse_reload_request(
        &mut self,
        request: &crate::reload_request::ReloadRequest,
        reason: &str,
    ) {
        self.push_block(ScrollBlock::System(format!(
            "requested reload refused: {reason}"
        )));
        self.mark_dirty();
        if let Some(paths) = self.reload_requests.home.as_ref() {
            crate::reload_request::record(
                paths,
                &request.request_id,
                &crate::reload_request::ReloadOutcome::Refused {
                    reason: reason.to_owned(),
                },
            );
        }
    }

    fn poll_reload_probe_once(&mut self) -> Option<ReloadOutcome> {
        if let Some(target) = self.reload_confirmed_target.take() {
            if self.reload_source_changed() {
                self.push_block(ScrollBlock::System("reload kept this session open: the conversation changed during the capacity check; retry /reload".to_owned()));
                return Some(ReloadOutcome::Refused);
            }
            if let Some(reason) = self.reload_refusal().map(str::to_owned) {
                self.push_block(ScrollBlock::System(reason));
                return Some(ReloadOutcome::Refused);
            }
            return Some(ReloadOutcome::HandOff(Some(target)));
        }
        let receiver: &Receiver<Option<crate::carry::ReloadTarget>> = self.reload_probe.as_ref()?;
        let target = match receiver.try_recv() {
            Ok(target) => target,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => {
                self.reload_probe = None;
                self.push_block(ScrollBlock::System(
                    "reload refused: the installed-build check failed".to_owned(),
                ));
                self.mark_dirty();
                return Some(ReloadOutcome::Refused);
            }
        };
        self.reload_probe = None;
        self.mark_dirty();
        if self.reload_source_changed() {
            self.push_block(ScrollBlock::System("reload kept this session open: the conversation changed during the capacity check; retry /reload".to_owned()));
            return Some(ReloadOutcome::Refused);
        }
        let refusal = self.reload_refusal().map(str::to_owned).or_else(|| {
            target
                .as_ref()
                .and_then(|target| target.refusal(&crate::home::state::state_key()))
        });
        if let Some(reason) = refusal {
            self.push_block(ScrollBlock::System(reason));
            return Some(ReloadOutcome::Refused);
        }
        if let Some(target) = target.as_ref()
            && let Some(capacity) = &target.capacity
            && let Some(warning) = capacity.warning()
        {
            self.open_reload_confirm(
                target.clone(),
                vec![
                    format!(
                        "{} messages, ~{} tokens; the first request rebuilds the prompt cache",
                        capacity.messages, capacity.estimated_tokens
                    ),
                    warning,
                ],
            );
            return None;
        }
        Some(ReloadOutcome::HandOff(target))
    }
}
