//! `/reload`: the installed-build probe runs on the session's host, off its
//! thread.
//!
//! Asking the installed `plak` its state key spawns a process that may take
//! seconds on a first exec; the loop keeps painting and handing input while
//! the host runs it, then hands off (or prints the refusal) when the answer
//! (`reload_probed`) lands.

use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

use super::{App, ScrollBlock};

pub(super) const RELOAD_CHECKING: &str = "checking installed build…";

/// How long the host may take to answer a reload probe before the TUI
/// refuses the reload as timed out.
const RELOAD_PROBE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// The session's own reload requests, as the host hands them over: a
/// request waiting for the next idle boundary, the one a running probe
/// performs, and the answers the host still has to take.
#[derive(Default)]
pub(super) struct ReloadRequests {
    /// The product home, for a refusal no host is left to record.
    home: Option<crate::home::paths::PlakPaths>,
    pending: Option<crate::reload_request::ReloadRequest>,
    active: Option<crate::reload_request::ReloadRequest>,
    /// Requests taken from the host's offer and not yet acknowledged to it.
    accepts: Vec<String>,
    /// Requests whose acknowledgement this face's host took: only these
    /// are its to record.
    handed_over: std::collections::BTreeSet<String>,
    /// `(request id, reason)` per refusal not yet sent to the host.
    refusals: Vec<(String, String)>,
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

/// A reload probe's answer: the target, and the host build's refusal of it.
pub(super) type ReloadProbed = (Option<crate::carry::ReloadTarget>, Option<String>);

impl App {
    /// Asks the session's host for the reload target unless a check is
    /// already running; its answer arrives as `reload_probed`.
    pub(super) fn start_host_reload_probe(
        &mut self,
        actions: Option<&crate::tui::FaceActionSender>,
    ) {
        if self.reload_probe.is_some() {
            return;
        }
        let sent = actions.is_some_and(|actions| {
            actions
                .send(crate::tui::UserAction::Extension(
                    crate::tui::SurfaceIntent::ProbeReload,
                ))
                .is_ok()
        });
        if !sent {
            self.push_block(ScrollBlock::System(
                "reload refused: could not start the installed-build check".to_owned(),
            ));
            self.mark_dirty();
            return;
        }
        self.reload_probe_answer = Some(self.begin_reload_probe());
        self.reload_probe_asked_at = Some(self.now());
    }

    /// The host's answer to the reload probe; `None` when the check failed.
    pub(super) fn apply_reload_probed(&mut self, probed: Option<ReloadProbed>) {
        if let Some(answer) = self.reload_probe_answer.take()
            && let Some(probed) = probed
        {
            let _ = answer.send(probed);
        }
        self.mark_dirty();
    }

    /// Starts `probe` on its own thread unless one is already running, as
    /// the host's would.
    #[cfg(test)]
    pub(super) fn start_reload_probe(
        &mut self,
        probe: impl FnOnce() -> Option<crate::carry::ReloadTarget> + Send + 'static,
    ) {
        if self.reload_probe.is_some() {
            return;
        }
        let sender = self.begin_reload_probe();
        std::thread::spawn(move || {
            let target = probe();
            let refusal = target
                .as_ref()
                .and_then(|target| target.refusal(&crate::home::state::state_key()));
            let _ = sender.send((target, refusal));
        });
    }

    fn begin_reload_probe(&mut self) -> Sender<ReloadProbed> {
        let (sender, receiver) = mpsc::channel();
        self.reload_source = Some(ReloadSource::snapshot(self));
        self.reload_probe = Some(receiver);
        self.push_block(ScrollBlock::System(RELOAD_CHECKING.to_owned()));
        self.mark_dirty();
        sender
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
                self.reload_requests
                    .refusals
                    .push((request.request_id, reason));
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

    /// The reload request the host offers this session: taken (and
    /// acknowledged) once, it runs at the next idle boundary. The host
    /// offers it again until it hears the acknowledgement.
    pub(super) fn apply_reload_request(&mut self, request: crate::reload_request::ReloadRequest) {
        let known = [&self.reload_requests.pending, &self.reload_requests.active]
            .into_iter()
            .flatten()
            .any(|held| held.request_id == request.request_id);
        if known {
            self.reload_requests.accepts.push(request.request_id);
            return;
        }
        if self.session_id.as_deref() != Some(request.session_id.as_str()) {
            return;
        }
        self.push_block(ScrollBlock::System(format!(
            "reload requested onto build {}; it runs when this session is idle",
            request.expect_binary
        )));
        self.mark_dirty();
        self.reload_requests
            .accepts
            .push(request.request_id.clone());
        self.reload_requests.pending = Some(request);
    }

    /// Hands the host the acknowledgements and refusals it records for
    /// their requesters. A full mailbox keeps them for the next frame; with
    /// no host left, a refusal is recorded here.
    pub(super) fn send_reload_answers(&mut self, actions: Option<&crate::tui::FaceActionSender>) {
        use crate::tui::{SurfaceIntent, UserAction, UserActionSendError};
        let mut accepts = std::mem::take(&mut self.reload_requests.accepts).into_iter();
        while let Some(request_id) = accepts.next() {
            let Some(actions) = actions else {
                continue;
            };
            let accept = SurfaceIntent::AcceptReloadRequest {
                request_id: request_id.clone(),
            };
            match actions.send(UserAction::Extension(accept)) {
                Ok(()) => {
                    self.reload_requests.handed_over.insert(request_id);
                }
                Err(UserActionSendError::Saturated(_)) => {
                    self.reload_requests.accepts.push(request_id);
                    self.reload_requests.accepts.extend(accepts);
                    break;
                }
                Err(_) => {}
            }
        }
        // A refusal never overtakes its own acceptance: the host records only
        // a request it handed over.
        if !self.reload_requests.accepts.is_empty() {
            return;
        }
        let mut refusals = std::mem::take(&mut self.reload_requests.refusals).into_iter();
        while let Some((request_id, reason)) = refusals.next() {
            let sent = match actions {
                Some(actions) if self.reload_requests.handed_over.contains(&request_id) => {
                    let refuse = SurfaceIntent::RefuseReloadRequest {
                        request_id: request_id.clone(),
                        reason: reason.clone(),
                    };
                    match actions.send(UserAction::Extension(refuse)) {
                        Ok(()) => true,
                        Err(
                            UserActionSendError::Saturated(_) | UserActionSendError::Duplicate(_),
                        ) => {
                            self.reload_requests.refusals.push((request_id, reason));
                            self.reload_requests.refusals.extend(refusals);
                            return;
                        }
                        Err(_) => false,
                    }
                }
                _ => false,
            };
            self.reload_requests.handed_over.remove(&request_id);
            // No host that handed this request over is left to record it.
            if !sent && let Some(paths) = self.reload_requests.home.as_ref() {
                crate::reload_request::record(
                    paths,
                    &request_id,
                    &crate::reload_request::ReloadOutcome::Refused { reason },
                );
            }
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
        self.reload_requests
            .refusals
            .push((request.request_id.clone(), reason.to_owned()));
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
        if self.reload_probe_answer.is_some()
            && self.reload_probe_asked_at.is_some_and(|asked| {
                self.now().saturating_duration_since(asked) >= RELOAD_PROBE_DEADLINE
            })
        {
            self.reload_probe = None;
            self.reload_probe_answer = None;
            self.reload_probe_asked_at = None;
            self.push_block(ScrollBlock::System(
                "reload refused: the installed-build check timed out".to_owned(),
            ));
            self.mark_dirty();
            return Some(ReloadOutcome::Refused);
        }
        let receiver: &Receiver<ReloadProbed> = self.reload_probe.as_ref()?;
        let (target, host_refusal) = match receiver.try_recv() {
            Ok(probed) => probed,
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
        let refusal = self.reload_refusal().map(str::to_owned).or(host_refusal);
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests;
