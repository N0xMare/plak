//! The session a wire connection serves, shared by every connection of it,
//! and its switch to another. A session's opening comes before the binding
//! that names it, so what follows an opening is held until that binding
//! says whose it is: the served session's, or the next one's. A switch ends
//! every attachment with `moved` naming the next session's root, and each
//! moved connection's next root attachment gets the opening's events an
//! attach snapshot does not carry, after its `attached` answer.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use plakous_host_protocol::views::transcript::TranscriptScope;
use plakous_host_protocol::wire::{AttachmentId, NodeAddress};
use plakous_host_protocol::{ProtocolExtension, SessionObservation};

use super::super::{ChildNodes, HostService, TreeNodes, kept_for_attach};
use super::ATTACHMENT_CAPACITY;

/// Finds a session's root node by its id.
pub(super) type SessionRoots = Box<dyn Fn(&str) -> Option<NodeAddress> + Send + Sync>;

/// The served session, its service and the switch in progress.
pub(super) struct ServedSession<P: ProtocolExtension> {
    session: String,
    roots: SessionRoots,
    /// A handle at the served session's root; a connection whose handle is
    /// of an earlier generation takes a clone of it.
    service: HostService<P>,
    /// How many switches the service went through.
    generation: u64,
    /// A session opening held until the binding that names its session.
    held: Option<Vec<SessionObservation<P>>>,
    /// After a switch, the next session's events an attach snapshot does
    /// not carry, kept for every attachment a switch moved until its
    /// connection attaches the root again or leaves; the next switch
    /// replaces it.
    replay: Vec<SessionObservation<P>>,
    /// The moved attachments still owed the replay.
    owed: BTreeSet<AttachmentId>,
    /// The replay outgrew the attachment capacity and was given up: an
    /// owed attachment's connection resyncs instead of getting a silent
    /// gap.
    replay_lost: bool,
}

/// The served session behind its lock; the pump and every connection hold
/// it, so an attach and its replay are one step against the observations.
pub(super) type Shared<P> = Arc<Mutex<ServedSession<P>>>;

pub(super) fn lock<P: ProtocolExtension>(shared: &Shared<P>) -> MutexGuard<'_, ServedSession<P>> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

impl<P: ProtocolExtension> ServedSession<P> {
    pub(super) fn shared(
        session: String,
        roots: SessionRoots,
        service: HostService<P>,
    ) -> Shared<P> {
        Arc::new(Mutex::new(Self {
            session,
            roots,
            service,
            generation: 0,
            held: None,
            replay: Vec::new(),
            owed: BTreeSet::new(),
            replay_lost: false,
        }))
    }

    pub(super) fn session(&self) -> &str {
        &self.session
    }

    pub(super) const fn root(&self) -> &NodeAddress {
        self.service.root()
    }

    pub(super) const fn generation(&self) -> u64 {
        self.generation
    }

    pub(super) fn service(&self) -> HostService<P> {
        self.service.clone()
    }

    /// Takes one observation, moving the service to the session it switched
    /// to; `false` when the switch's root cannot be found and the service
    /// must end.
    pub(super) fn observe(&mut self, event: SessionObservation<P>) -> bool {
        if !self.owed.is_empty() && !self.replay_lost && !kept_for_attach(&event) {
            self.replay.push(event.clone());
            self.limit_replay();
        }
        let opening = matches!(event, SessionObservation::SessionOpening);
        let binding = match &event {
            SessionObservation::Binding { session_id, .. } => Some(session_id.clone()),
            _ => None,
        };
        let Some(held) = self.held.as_mut() else {
            if opening {
                self.held = Some(vec![event]);
                return true;
            }
            if let Some(next) = binding.flatten() {
                return self.switch(next, vec![event]);
            }
            self.service.observe(event);
            return true;
        };
        held.push(event);
        if binding.is_none() && held.len() <= ATTACHMENT_CAPACITY {
            return true;
        }
        let overflowed = binding.is_none();
        let held = self.held.take().unwrap_or_default();
        if let Some(next) = binding.flatten() {
            return self.switch(next, held);
        }
        if overflowed {
            // Too long an opening to hold: it may be the next session's,
            // which the served session's attachments must not be shown, so
            // they end and attach again for a fresh snapshot.
            self.service.resync_attachments();
        }
        self.pass(held);
        true
    }

    /// Takes `observations`, the opening of the session `next` names. When
    /// that is not the served one, every attachment ends with `moved` naming
    /// its root, and each moved connection's next root attachment gets the
    /// opening's events after its attach snapshot. `false` when that root
    /// cannot be found.
    fn switch(&mut self, next: String, observations: Vec<SessionObservation<P>>) -> bool {
        if next == self.session {
            self.pass(observations);
            return true;
        }
        let Some(node) = (self.roots)(&next) else {
            self.pass(observations);
            return false;
        };
        // What the old session's attachments still hold goes first; the
        // switch's observations belong to the new session's attachments.
        // A connection moved before and not yet attached again is owed
        // this switch's replay instead of the last one's.
        self.owed.extend(self.service.move_attachments(&node));
        let nodes: Arc<dyn ChildNodes> = Arc::new(TreeNodes::new(node.tree_id.clone()));
        self.service.reroot(node, Some(nodes));
        self.session = next;
        self.generation += 1;
        // The service keeps what an attach re-emits; the rest of the
        // opening after its last root snapshot waits for the next attach.
        let baseline = observations.iter().rposition(|observation| {
            matches!(observation, SessionObservation::PresentationSnapshot(snapshot)
                if snapshot.scope == TranscriptScope::Root)
        });
        self.replay = observations
            .iter()
            .enumerate()
            .filter(|(index, observation)| {
                baseline.is_none_or(|baseline| *index > baseline) && !kept_for_attach(observation)
            })
            .map(|(_, observation)| observation.clone())
            .collect();
        self.replay_lost = false;
        self.limit_replay();
        self.settle();
        self.pass(observations);
        true
    }

    /// The session's observations ended: an opening no binding followed
    /// (the open failed) is the served session's.
    pub(super) fn end(&mut self) {
        let held = self.held.take().unwrap_or_default();
        self.pass(held);
    }

    /// A connection whose attachment `moved` was moved attached the root as
    /// `attachment`: the opening a switch left waiting follows its attach
    /// snapshot, after the `attached` answer. `true` when that replay was
    /// given up, so the attachment must resync, as a slow consumer does,
    /// rather than miss events.
    pub(super) fn attached(&mut self, moved: AttachmentId, attachment: AttachmentId) -> bool {
        if !self.owed.remove(&moved) {
            return false;
        }
        let lost = self.replay_lost;
        if !lost {
            self.service.replay(attachment, moved, self.replay.clone());
        }
        self.settle();
        lost
    }

    /// Whether the attachment `moved`, ended by a switch, is still owed
    /// that switch's replay.
    pub(super) fn owes(&self, moved: AttachmentId) -> bool {
        self.owed.contains(&moved)
    }

    /// The connection whose attachment `moved` was moved left without
    /// attaching again.
    pub(super) fn forget(&mut self, moved: AttachmentId) {
        if self.owed.remove(&moved) {
            self.settle();
        }
    }

    /// Drops the replay once no attachment is owed it.
    fn settle(&mut self) {
        if self.owed.is_empty() {
            self.replay = Vec::new();
            self.replay_lost = false;
        }
    }

    /// Gives the replay up once it outgrows the attachment capacity.
    fn limit_replay(&mut self) {
        if self.replay.len() > ATTACHMENT_CAPACITY {
            self.replay.clear();
            self.replay_lost = true;
        }
    }

    fn pass(&self, observations: Vec<SessionObservation<P>>) {
        for observation in observations {
            self.service.observe(observation);
        }
    }
}
