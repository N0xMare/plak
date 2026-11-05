//! The installed-build check and the session's reload requests, on the host
//! side of the product surface (spec 02 §5.1): a face asks on its own
//! cadence, and the host reads the installed build and the request channel.

use std::collections::BTreeSet;
use std::sync::{Mutex, PoisonError};

use plakous_hosted::runtime::Sender;

use crate::home::paths::PlakPaths;
use crate::reload_request::{Listener, ReloadOutcome, ReloadRequest};
use crate::surface_protocol::{ContentEvent, PlakExtension, SurfaceObservation};

/// The served session's place on the reload-request channel while the
/// surface's host runs: its announcement, the request it offered the face
/// and the requests the face accepted and has not settled.
pub(super) struct ReloadChannel {
    paths: PlakPaths,
    state: Mutex<Channel>,
}

#[derive(Default)]
struct Channel {
    listener: Option<(String, Listener)>,
    /// Taken from the channel and offered on every check until the face
    /// accepts it.
    offered: Option<ReloadRequest>,
    accepted: BTreeSet<String>,
}

impl ReloadChannel {
    pub(super) fn new(paths: PlakPaths) -> Self {
        Self {
            paths,
            state: Mutex::default(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Channel> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Answers a build check: whether a newer `plak` is installed, and the
    /// request waiting for `session`, the session this host serves, which it
    /// announces (again, when its announcement vanished or another instance
    /// replaced it). A request is offered on every check until the face
    /// accepts it, so a lost answer loses nothing.
    pub(super) fn check(&self, session: Option<&str>, events: &Sender<PlakExtension>) {
        let _ = events.send(ContentEvent::Extension(SurfaceObservation::BuildStatus {
            newer_installed: crate::build_identity::newer_build_installed(),
        }));
        let Some(session) = session else {
            return;
        };
        let mut channel = self.lock();
        if channel
            .listener
            .as_ref()
            .is_none_or(|(id, listener)| id != session || !listener.is_current())
        {
            if channel
                .listener
                .as_ref()
                .is_some_and(|(id, _)| id != session)
                && let Some(offered) = channel.offered.take()
            {
                not_taken(
                    &self.paths,
                    &offered,
                    "the session's host moved to another session",
                );
            }
            // A missing or replaced announcement is re-published, so a pruned
            // or overwritten file never leaves the channel silently dead.
            channel.listener = Listener::announce(&self.paths, session)
                .map(|announced| (session.to_owned(), announced));
        }
        if channel.offered.is_none() {
            let taken = channel
                .listener
                .as_ref()
                .and_then(|(_, announced)| announced.take(&self.paths, session));
            channel.offered = taken;
        }
        if let Some(offered) = channel.offered.clone() {
            let _ = events.send(ContentEvent::Extension(
                SurfaceObservation::ReloadRequested(offered),
            ));
        }
    }

    /// The face took the offered request `request_id`; it settles it.
    pub(super) fn accept(&self, request_id: &str) {
        let mut channel = self.lock();
        if channel
            .offered
            .as_ref()
            .is_some_and(|offered| offered.request_id == request_id)
        {
            channel.offered = None;
            channel.accepted.insert(request_id.to_owned());
        }
    }

    /// The host leaves `session` for another session (spec 02 §5.1): a
    /// reload request belongs to its session, so every request for it this
    /// host holds (offered, accepted and not yet settled, or still waiting in
    /// the channel) is settled `not taken` now and none reaches the next
    /// session. The announcement is withdrawn; the next check announces the
    /// session the host serves then. An outcome that cannot be written is
    /// logged and the request dropped.
    pub(super) fn leave(&self, session: &str) {
        let mut channel = self.lock();
        if channel
            .listener
            .as_ref()
            .is_none_or(|(id, _)| id != session)
        {
            return;
        }
        let waiting = channel
            .listener
            .as_ref()
            .and_then(|(_, listener)| listener.take(&self.paths, session));
        channel.listener = None;
        let mut settled: Vec<String> = std::mem::take(&mut channel.accepted).into_iter().collect();
        settled.extend(channel.offered.take().map(|offered| offered.request_id));
        settled.extend(waiting.map(|waiting| waiting.request_id));
        drop(channel);
        for request_id in settled {
            crate::reload_request::settle_switched(&self.paths, &request_id);
        }
    }

    /// Records the face's refusal of `request_id` for its requester, when
    /// this host handed that request over.
    pub(super) fn refuse(&self, request_id: &str, reason: String) {
        if self.lock().accepted.remove(request_id) {
            crate::reload_request::record(
                &self.paths,
                request_id,
                &ReloadOutcome::Refused { reason },
            );
        }
    }
}

impl Drop for ReloadChannel {
    /// A request offered and never accepted reached no face: its requester
    /// is told rather than left to time out.
    fn drop(&mut self) {
        let offered = self.lock().offered.take();
        if let Some(offered) = offered {
            not_taken(&self.paths, &offered, "the session's host ended first");
        }
    }
}

/// Records that no face took `request`.
fn not_taken(paths: &PlakPaths, request: &ReloadRequest, why: &str) {
    crate::reload_request::record(
        paths,
        &request.request_id,
        &ReloadOutcome::Refused {
            reason: format!("not taken: {why}"),
        },
    );
}

/// Answers a reload probe for `session`, the session this host serves, off
/// the host's thread: asking the installed `plak` can take seconds. The
/// answer always comes, as `reload_probe_failed` when the check could not
/// run, and as a refusal when the host serves no session.
pub(super) fn probe(
    paths: &PlakPaths,
    session: Option<plakous_hosted::runtime::SurfaceSession>,
    phase: plakous_hosted::runtime::SurfacePhase,
    events: &Sender<PlakExtension>,
) {
    let answer = ProbeAnswer(Some(events.answering()));
    let Some(session) = session else {
        answer.send(SurfaceObservation::ReloadProbed {
            target: None,
            refusal: Some(NO_SESSION.to_owned()),
        });
        return;
    };
    // The work in flight now, which only a reload resuming this session
    // hands off.
    let in_flight = if phase == plakous_hosted::runtime::SurfacePhase::Busy {
        Some(crate::carry::CARRY_REFUSED_TURN)
    } else if session.live_children {
        Some(crate::carry::CARRY_REFUSED_CHILDREN)
    } else {
        None
    };
    let paths = paths.clone();
    let _ = std::thread::Builder::new()
        .name("plak-reload-probe".to_owned())
        .spawn(move || {
            let (target, refusal) = crate::carry::probe_reload(
                &paths,
                &session.workspace,
                Some(&session.id),
                in_flight,
            );
            answer.send(SurfaceObservation::ReloadProbed {
                target: target.map(Box::new),
                refusal,
            });
        });
}

/// The probe's refusal when the host serves no session.
const NO_SESSION: &str = "reload refused: no session to reload";

/// A reload probe's answer, sent once: dropped unanswered (the check could
/// not run or panicked), it answers that the check failed.
struct ProbeAnswer(Option<Sender<PlakExtension>>);

impl ProbeAnswer {
    fn send(mut self, observation: SurfaceObservation) {
        if let Some(events) = self.0.take() {
            let _ = events.send(ContentEvent::Extension(observation));
        }
    }
}

impl Drop for ProbeAnswer {
    fn drop(&mut self) {
        if let Some(events) = self.0.take() {
            let _ = events.send(ContentEvent::Extension(
                SurfaceObservation::ReloadProbeFailed,
            ));
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests;
