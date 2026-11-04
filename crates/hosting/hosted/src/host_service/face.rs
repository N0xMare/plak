//! An in-process face as an attachment: it sends its intents through the
//! session's own mailbox sender, tagged with its attachment, and takes its
//! observations from the service, in the receiver's own shape.

use std::collections::VecDeque;
use std::sync::{Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};

use plakous_host_protocol::views::transcript::TranscriptScope;
use plakous_host_protocol::wire::{AttachmentId, NodeAddress};
use plakous_host_protocol::{
    IntentSender, ObservationReceiver, ProtocolExtension, SessionObservation,
};

use super::{HostService, Outbound, origin};

/// What a face receives: the session's observations as its attachment
/// takes them from a host service of its own, with
/// [`ObservationReceiver`]'s methods.
///
/// The service runs on the face's own reads: each observation the session
/// published is offered to it and the attachment's queue is taken. The face
/// holds every transcript scope and its queue is unbounded, so it receives
/// what the session sent, in order, with every `IntentStarted` it owns
/// carrying only its own correlation; the session's mailbox stays the bound
/// a slow face meets.
pub struct FaceReceiver<P: ProtocolExtension> {
    events: ObservationReceiver<P>,
    service: HostService<P>,
    attachment: AttachmentId,
    taken: Mutex<VecDeque<SessionObservation<P>>>,
}

/// Attaches the face that holds `actions` and `events`, the session's own
/// mailbox ends: `actions` comes back tagged with its attachment, so the
/// answers to its intents are addressed to it. Every other property of the
/// sender (its lanes, its admission, its last-drop quit) is unchanged.
#[must_use]
pub fn face<P: ProtocolExtension>(
    actions: IntentSender<P>,
    events: ObservationReceiver<P>,
) -> (IntentSender<P>, FaceReceiver<P>) {
    let receiver = FaceReceiver::new(events);
    (actions.tagged(origin(receiver.attachment)), receiver)
}

impl<P: ProtocolExtension> FaceReceiver<P> {
    /// `events` as the one attachment of a service of their own.
    #[must_use]
    pub fn new(events: ObservationReceiver<P>) -> Self {
        let service = HostService::serving(
            None,
            NodeAddress {
                tree_id: String::new(),
                agent_id: String::new(),
            },
            usize::MAX,
        );
        let (attachment, _) = service.admit();
        Self {
            events,
            service,
            attachment,
            taken: Mutex::new(VecDeque::new()),
        }
    }

    fn next_taken(&self) -> Option<SessionObservation<P>> {
        self.taken
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
    }

    fn offer(&self, observation: SessionObservation<P>) {
        self.service.observe(observation);
        let taken = self.service.take(self.attachment);
        self.taken
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(taken.into_iter().filter_map(|outbound| match outbound {
                Outbound::Event { observation, .. } => Some(observation),
                // An unbounded queue is never dropped.
                Outbound::ResyncRequired => None,
            }));
    }

    /// As [`ObservationReceiver::transcript_resnapshot_required_for`].
    #[must_use]
    pub fn transcript_resnapshot_required_for(&self, scope: &TranscriptScope) -> bool {
        self.events.transcript_resnapshot_required_for(scope)
    }

    /// As [`ObservationReceiver::set_notify`]: the hook is the session
    /// mailbox's, so it wakes the face for what the session published.
    pub fn set_notify(&self, notify: impl Fn() + Send + Sync + 'static) {
        self.events.set_notify(notify);
    }

    /// As [`ObservationReceiver::try_recv`].
    ///
    /// # Errors
    /// Returns `Empty` when none is ready or `Disconnected` once the session
    /// is gone.
    pub fn try_recv(&self) -> Result<SessionObservation<P>, mpsc::TryRecvError> {
        loop {
            if let Some(observation) = self.next_taken() {
                return Ok(observation);
            }
            self.offer(self.events.try_recv()?);
        }
    }

    /// As [`ObservationReceiver::recv_timeout`].
    ///
    /// # Errors
    /// Returns `Timeout` when the deadline expires or `Disconnected` once the
    /// session is gone.
    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<SessionObservation<P>, mpsc::RecvTimeoutError> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(observation) = self.next_taken() {
                return Ok(observation);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            self.offer(self.events.recv_timeout(remaining)?);
        }
    }

    /// As [`ObservationReceiver::try_iter`].
    pub fn try_iter(&self) -> impl Iterator<Item = SessionObservation<P>> + '_ {
        std::iter::from_fn(|| self.try_recv().ok())
    }
}
