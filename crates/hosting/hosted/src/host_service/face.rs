//! An in-process face as an attachment of its session's service: its
//! intents pass through the service as that attachment's, sent with the
//! session's own mailbox sender tagged with it, and it takes its
//! observations from the service, in the receiver's own shape.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};

use plakous_host_protocol::views::ProcessView;
use plakous_host_protocol::views::transcript::TranscriptScope;
use plakous_host_protocol::wire::{AttachmentId, NodeAddress};
use plakous_host_protocol::{
    IntentSendError, IntentSender, ObservationReceiver, ProtocolExtension, SessionIntent,
    SessionObservation,
};

use super::wire::Face;
use super::{HostService, Outbound, origin};

/// What a face sends with: the session's mailbox sender, tagged with the
/// face's attachment, whose intents the service takes as that attachment's
/// before they reach the mailbox, with [`IntentSender`]'s methods.
///
/// It is one of the session's senders, so the session sees its last sender
/// leave when the last face sender and attachment are gone.
pub struct FaceSender<P: ProtocolExtension> {
    actions: IntentSender<P>,
    service: HostService<P>,
    attachment: AttachmentId,
}

impl<P: ProtocolExtension> Clone for FaceSender<P> {
    fn clone(&self) -> Self {
        Self {
            actions: self.actions.clone(),
            service: self.service.clone(),
            attachment: self.attachment,
        }
    }
}

impl<P: ProtocolExtension> FaceSender<P> {
    /// `actions` as the sender of a face no service serves: its intents
    /// reach the mailbox as sent. For tests of a face's callers.
    #[doc(hidden)]
    #[must_use]
    pub fn unattached(actions: IntentSender<P>) -> Self {
        Self {
            actions,
            service: HostService::serving(None, unnamed(), 1),
            attachment: 0,
        }
    }

    /// As [`IntentSender::send`], through the service.
    ///
    /// # Errors
    /// The mailbox's refusal, or [`IntentSendError::Refused`] when the
    /// service refused it for the face's role.
    #[allow(
        clippy::result_large_err,
        reason = "the refusal hands the rejected action back so the caller can retry it verbatim"
    )]
    pub fn send(&self, action: SessionIntent<P>) -> Result<(), IntentSendError<P>> {
        self.send_correlated(action, None)
    }

    /// As [`IntentSender::send_correlated`], through the service.
    ///
    /// # Errors
    /// As [`Self::send`].
    #[allow(
        clippy::result_large_err,
        reason = "the refusal hands the rejected action back so the caller can retry it verbatim"
    )]
    pub fn send_correlated(
        &self,
        action: SessionIntent<P>,
        correlation: Option<String>,
    ) -> Result<(), IntentSendError<P>> {
        self.service
            .face_intent(self.attachment, &self.actions, action, correlation)
    }

    /// As [`IntentSender::background_processes`].
    #[must_use]
    pub fn background_processes(&self) -> usize {
        self.actions.background_processes()
    }

    /// As [`IntentSender::processes`].
    #[must_use]
    pub fn processes(&self) -> Arc<Vec<ProcessView>> {
        self.actions.processes()
    }

    /// As [`IntentSender::mark_promptable`].
    pub fn mark_promptable(&self) {
        self.actions.mark_promptable();
    }

    /// As [`IntentSender::request_abandon_and_release`].
    pub fn request_abandon_and_release(&self) {
        self.actions.request_abandon_and_release();
    }
}

/// What a face receives: the session's observations as its attachment
/// takes them from the service, with [`ObservationReceiver`]'s methods.
///
/// The service runs on the face's own reads: each observation the session
/// published is offered to it and the attachment's queue is taken. The
/// face's queue is unbounded, so it receives what the service routed to it,
/// in order, with every `IntentStarted` it owns carrying only its own
/// correlation; the session's mailbox stays the bound a slow face meets.
/// The attachment ends with the receiver.
pub struct FaceReceiver<P: ProtocolExtension> {
    events: ObservationReceiver<P>,
    service: HostService<P>,
    attachment: AttachmentId,
    taken: Mutex<VecDeque<SessionObservation<P>>>,
}

/// How far a later attachment of a face's session may fall behind before it
/// is dropped.
const SESSION_CAPACITY: usize = 4096;

/// The face that holds `actions` and `events`, a session's mailbox ends, as
/// the first attachment of that session's service
/// ([`HostService::attach_face`]). The service names no root yet: an
/// in-process face attaches to the root without naming it.
#[must_use]
pub fn face<P: ProtocolExtension>(
    actions: IntentSender<P>,
    events: ObservationReceiver<P>,
) -> (FaceSender<P>, FaceReceiver<P>) {
    HostService::for_faces(actions.clone(), unnamed(), SESSION_CAPACITY)
        .attach_face(actions, events)
}

/// The address of a service that names no tree.
const fn unnamed() -> NodeAddress {
    NodeAddress {
        tree_id: String::new(),
        agent_id: String::new(),
    }
}

impl<P: ProtocolExtension> HostService<P> {
    /// Attaches the in-process face that holds `actions`, a sender of this
    /// service's session, and `events`, the session's observations, which
    /// the face's reads offer to the service. The face holds the root's
    /// transcript and the agent transcript it opens; it attaches before the
    /// session's first observation, whose root snapshot reaches it.
    #[must_use]
    pub fn attach_face(
        &self,
        actions: IntentSender<P>,
        events: ObservationReceiver<P>,
    ) -> (FaceSender<P>, FaceReceiver<P>) {
        let (attachment, _) = self.admit(None, Face::Local);
        let sender = FaceSender {
            actions: actions.tagged(origin(attachment)),
            service: self.clone(),
            attachment,
        };
        let receiver = FaceReceiver {
            events,
            service: self.clone(),
            attachment,
            taken: Mutex::new(VecDeque::new()),
        };
        (sender, receiver)
    }

    /// The face `attachment` leaves. It asks the session for nothing as it
    /// goes: a face's views are its own intents' to close.
    fn leave(&self, attachment: AttachmentId) {
        self.lock().remove(attachment, None);
    }
}

/// Publishes to a face from outside its session: a product publisher of
/// the face's process (the launch's pre-bind progress) whose observations
/// reach the face's service as the session's do, and the face on its next
/// read.
pub struct FacePublisher<P: ProtocolExtension> {
    service: HostService<P>,
}

impl<P: ProtocolExtension> Clone for FacePublisher<P> {
    fn clone(&self) -> Self {
        Self {
            service: self.service.clone(),
        }
    }
}

impl<P: ProtocolExtension> FacePublisher<P> {
    /// Publishes `observation`.
    pub fn publish(&self, observation: SessionObservation<P>) {
        self.service.observe(observation);
    }
}

impl<P: ProtocolExtension> Drop for FaceReceiver<P> {
    fn drop(&mut self) {
        self.service.leave(self.attachment);
    }
}

impl<P: ProtocolExtension> FaceReceiver<P> {
    /// `events` as the one attachment of a service of their own, with no
    /// session mailbox: it holds every transcript.
    #[must_use]
    pub fn new(events: ObservationReceiver<P>) -> Self {
        let service = HostService::serving(None, unnamed(), usize::MAX);
        let (attachment, _) = service.admit(None, Face::Local);
        Self {
            events,
            service,
            attachment,
            taken: Mutex::new(VecDeque::new()),
        }
    }

    /// A publisher of this face's service for observations from outside
    /// the session ([`FacePublisher`]).
    #[must_use]
    pub fn publisher(&self) -> FacePublisher<P> {
        FacePublisher {
            service: self.service.clone(),
        }
    }

    /// Takes what a [`FacePublisher`] queued; whether there was any.
    fn take_published(&self) -> bool {
        let published = self.service.take(self.attachment);
        if published.is_empty() {
            return false;
        }
        self.queue(published);
        true
    }

    fn queue(&self, taken: Vec<Outbound<P>>) {
        self.taken
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(taken.into_iter().filter_map(|outbound| match outbound {
                Outbound::Event { observation, .. } => Some(observation),
                // An unbounded queue is never dropped, and a face's service
                // is never moved.
                Outbound::ResyncRequired | Outbound::Moved { .. } => None,
            }));
    }

    fn next_taken(&self) -> Option<SessionObservation<P>> {
        self.taken
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
    }

    fn offer(&self, observation: SessionObservation<P>) {
        self.service.observe(observation);
        self.queue(self.service.take(self.attachment));
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
            if self.take_published() {
                continue;
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
            if self.take_published() {
                continue;
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
