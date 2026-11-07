//! The two bounded in-process mailboxes between a driver and the hosted
//! session, with the semantics a product surface depends on.
//!
//! Intents ([`intent_channel`]): a bounded FIFO with a coalescing urgent lane
//! for `Interrupt`, an independently coalesced `Quit` observed only after
//! accepted ordinary work, and one-in-flight admission for the non-repeatable
//! session mutations (`NewSession`, `Compact`, `ForkSession`) released by an
//! RAII acknowledgement. An async consumer waits on [`IntentReceiver::ready`].
//!
//! Observations ([`observation_channel`]): a bounded queue that never blocks
//! the producer. Assistant deltas coalesce, latest-state snapshots supersede
//! their own older value, and at saturation final/state observations evict a
//! replaceable transcript/delta/progress observation; a dropped transcript
//! observation is remembered until a transcript replacement lands so the host
//! can resnapshot. A consumer that waits somewhere other than the mailbox
//! (a terminal's `poll`) installs a notifier, which the queue calls on each
//! empty to non-empty edge and when the last producer leaves.

use std::collections::{BTreeSet, VecDeque};
use std::fmt;
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Poll, Waker};
use std::time::{Duration, Instant};

use crate::intent::{Core, ObservationExtension, ProtocolExtension, SessionIntent};
use crate::limits::{COALESCED_DELTA_BYTES, INTENT_QUEUE_CAPACITY, OBSERVATION_QUEUE_CAPACITY};
use crate::observation::SessionObservation;
use crate::views::transcript::{NoticeTone, TranscriptRow, TranscriptScope, TranscriptStreamDelta};

mod stops;

const AGENT_OBSERVATION_SHARE: usize = OBSERVATION_QUEUE_CAPACITY / 4;

fn agent_observation<P: ProtocolExtension>(event: &SessionObservation<P>) -> bool {
    let scope = match event {
        SessionObservation::TranscriptPatch(patch) => &patch.scope,
        SessionObservation::PresentationSnapshot(snapshot) => &snapshot.scope,
        SessionObservation::TranscriptStream { scope, .. } => scope,
        _ => return false,
    };
    matches!(scope, TranscriptScope::Agent(_))
}

fn append_delta(pending: &mut String, text: &str) -> bool {
    if pending.len().saturating_add(text.len()) > COALESCED_DELTA_BYTES {
        return false;
    }
    pending.push_str(text);
    true
}

fn coalesce_stream(pending: &mut TranscriptStreamDelta, delta: &TranscriptStreamDelta) -> bool {
    match (pending, delta) {
        (TranscriptStreamDelta::Assistant(pending), TranscriptStreamDelta::Assistant(text)) => {
            append_delta(pending, text)
        }
        (
            TranscriptStreamDelta::Reasoning {
                location_key: pending_key,
                text: pending,
            },
            TranscriptStreamDelta::Reasoning { location_key, text },
        ) if pending_key == location_key => append_delta(pending, text),
        (TranscriptStreamDelta::Tool(pending), TranscriptStreamDelta::Tool(tool))
            if pending.call_id == tool.call_id =>
        {
            pending.clone_from(tool);
            true
        }
        _ => false,
    }
}

fn coalesce_delta<P: ProtocolExtension>(
    pending: Option<&mut SessionObservation<P>>,
    event: &SessionObservation<P>,
) -> bool {
    match (pending, event) {
        (
            Some(SessionObservation::AssistantDelta(pending)),
            SessionObservation::AssistantDelta(text),
        ) => append_delta(pending, text),
        (
            Some(SessionObservation::ReasoningDelta {
                location_key: pending_key,
                text: pending,
            }),
            SessionObservation::ReasoningDelta { location_key, text },
        ) if pending_key == location_key => append_delta(pending, text),
        (
            Some(SessionObservation::TranscriptStream {
                scope: pending_scope,
                delta: pending,
            }),
            SessionObservation::TranscriptStream { scope, delta },
        ) if pending_scope == scope => coalesce_stream(pending, delta),
        _ => false,
    }
}

/// Where a hosted session publishes its observations.
///
/// Implemented by [`ObservationSender`]; a product may fan one session out to
/// several sinks or bridge to a wire face.
pub trait ObservationSink<P: ProtocolExtension = Core> {
    /// Publish one observation without waiting. The refused observation comes
    /// back when bounded capacity cannot admit it.
    ///
    /// # Errors
    /// Returns the original replaceable observation at saturation.
    #[allow(
        clippy::result_large_err,
        reason = "typed backpressure returns ownership of the exact refused observation"
    )]
    fn observe(&self, observation: SessionObservation<P>) -> Result<(), SessionObservation<P>>;

    /// Whether a transcript-building observation was dropped since the latest
    /// successfully queued transcript replacement.
    fn transcript_gap(&self) -> bool;
}

impl<P: ProtocolExtension> ObservationSink<P> for ObservationSender<P> {
    fn observe(&self, observation: SessionObservation<P>) -> Result<(), SessionObservation<P>> {
        self.send(observation)
    }

    fn transcript_gap(&self) -> bool {
        self.transcript_resnapshot_required()
    }
}

struct IntentQueue<P: ProtocolExtension> {
    promptable: AtomicBool,
    abandon_and_release: AtomicBool,
    planned_handoff: AtomicBool,
    queue: Mutex<VecDeque<QueuedIntent<P>>>,
    urgent: AtomicU8,
    shutdown_requested: AtomicBool,
    nonrepeatable: AtomicU8,
    nonrepeatable_generations: [AtomicU64; 3],
    next_generation: AtomicU64,
    senders: AtomicUsize,
    receiver_alive: AtomicBool,
    ready: Condvar,
    /// The task awaiting [`IntentReceiver::ready`], woken wherever `ready`
    /// is notified.
    waiter: Mutex<Option<Waker>>,
    /// Run once when shutdown is first requested or the last sender drops;
    /// `None` once they ran.
    shutdown_hooks: Mutex<Option<Vec<ShutdownHook>>>,
    stops: Mutex<StopBook>,
    /// Wakes connections waiting on a stop's answer. The settle itself is
    /// not an observation, so nothing else would.
    stop_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

/// One stop a sender asked the host for ([`IntentSender::request_stop`]),
/// which the host settles once the stop is durable.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct StopTicket(u64);

/// The stops asked for and the outcomes the host settled, until each asker
/// takes its own.
#[derive(Default)]
struct StopBook {
    issued: u64,
    settled: VecDeque<(StopTicket, Result<(), String>)>,
}

/// How many settled stops wait for their askers; an asker gone for good
/// leaves its outcome to be dropped behind the newer ones.
const SETTLED_STOPS: usize = 64;

type ShutdownHook = Box<dyn FnOnce() + Send>;

struct QueuedIntent<P: ProtocolExtension> {
    action: SessionIntent<P>,
    admission: Option<NonrepeatableAdmission>,
    correlation: Option<String>,
    origin: Option<Arc<str>>,
}

#[derive(Clone, Copy)]
struct NonrepeatableAdmission {
    bit: u8,
    generation: u64,
}

const URGENT_INTERRUPT: u8 = 1;
const PENDING_QUIT: u8 = 1 << 1;
const NONREPEATABLE_NEW: u8 = 1;
const NONREPEATABLE_COMPACT: u8 = 1 << 1;
const NONREPEATABLE_FORK: u8 = 1 << 2;

const fn nonrepeatable_bit<P: ProtocolExtension>(action: &SessionIntent<P>) -> Option<u8> {
    match action {
        SessionIntent::NewSession => Some(NONREPEATABLE_NEW),
        SessionIntent::Compact => Some(NONREPEATABLE_COMPACT),
        SessionIntent::ForkSession { .. } => Some(NONREPEATABLE_FORK),
        _ => None,
    }
}

const fn nonrepeatable_index(bit: u8) -> usize {
    bit.trailing_zeros() as usize
}

impl<P: ProtocolExtension> IntentQueue<P> {
    fn shutting_down(&self) -> bool {
        self.shutdown_requested.load(Ordering::Acquire) || self.senders.load(Ordering::Acquire) == 0
    }

    /// Runs the shutdown hooks once.
    fn run_shutdown_hooks(&self) {
        let hooks = self
            .shutdown_hooks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        for hook in hooks.into_iter().flatten() {
            hook();
        }
    }

    fn wake_waiter(&self) {
        let waiter = self
            .waiter
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(waiter) = waiter {
            waiter.wake();
        }
    }

    fn reserve_nonrepeatable(&self, bit: u8) -> Option<NonrepeatableAdmission> {
        if self.nonrepeatable.fetch_or(bit, Ordering::AcqRel) & bit != 0 {
            return None;
        }
        let mut generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        while generation == 0 {
            generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        }
        self.nonrepeatable_generations[nonrepeatable_index(bit)]
            .store(generation, Ordering::Release);
        Some(NonrepeatableAdmission { bit, generation })
    }

    fn release_nonrepeatable(&self, admission: NonrepeatableAdmission) {
        let generation = &self.nonrepeatable_generations[nonrepeatable_index(admission.bit)];
        if generation
            .compare_exchange(admission.generation, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.nonrepeatable
                .fetch_and(!admission.bit, Ordering::AcqRel);
        }
    }
}

/// Why a non-urgent UI action was refused by the bounded host mailbox.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IntentSendError<P: ProtocolExtension = Core> {
    /// The bounded host mailbox had no free slot.
    Saturated(SessionIntent<P>),
    /// The host endpoint has exited; no consumer can admit the action.
    Disconnected(SessionIntent<P>),
    /// The same non-repeatable mutation is queued or executing.
    Duplicate(SessionIntent<P>),
    /// The host service refused it for the sending attachment's role: it
    /// never reached the mailbox.
    Refused(SessionIntent<P>),
}

impl<P: ProtocolExtension> IntentSendError<P> {
    /// Return the action that was not accepted.
    #[must_use]
    pub fn into_action(self) -> SessionIntent<P> {
        match self {
            Self::Saturated(action)
            | Self::Disconnected(action)
            | Self::Duplicate(action)
            | Self::Refused(action) => action,
        }
    }
}

/// Nonblocking producer for the bounded UI → host control queue.
///
/// A sender may carry an origin tag ([`Self::tagged`]) that every intent it
/// sends carries beside its correlation, so the host's `IntentStarted` names
/// the attachment that sent it; everything else is the shared queue's.
pub struct IntentSender<P: ProtocolExtension = Core>(Arc<IntentQueue<P>>, Option<Arc<str>>);

/// A handle on an intent queue that is not one of its senders: holding it
/// does not keep the host from seeing the last sender leave.
pub struct WeakIntentSender<P: ProtocolExtension = Core>(Arc<IntentQueue<P>>);

/// Single host consumer for the bounded control queue.
pub struct IntentReceiver<P: ProtocolExtension = Core>(Arc<IntentQueue<P>>);

/// One exact action delivery. Its generation-bound acknowledgement is held
/// until this value, or the guard returned by [`Self::into_parts`], is dropped.
pub struct IntentDelivery<P: ProtocolExtension = Core> {
    action: SessionIntent<P>,
    acknowledgement: IntentAcknowledgement<P>,
    correlation: Option<String>,
    origin: Option<Arc<str>>,
}

/// RAII acknowledgement for one exact mailbox delivery.
pub struct IntentAcknowledgement<P: ProtocolExtension = Core> {
    queue: Arc<IntentQueue<P>>,
    admission: Option<NonrepeatableAdmission>,
}

impl<P: ProtocolExtension> Drop for IntentAcknowledgement<P> {
    fn drop(&mut self) {
        if let Some(admission) = self.admission.take() {
            self.queue.release_nonrepeatable(admission);
        }
    }
}

impl<P: ProtocolExtension> IntentDelivery<P> {
    fn new(queue: &Arc<IntentQueue<P>>, queued: QueuedIntent<P>) -> Self {
        Self {
            action: queued.action,
            acknowledgement: IntentAcknowledgement {
                queue: Arc::clone(queue),
                admission: queued.admission,
            },
            correlation: queued.correlation,
            origin: queued.origin,
        }
    }

    fn immediate(queue: &Arc<IntentQueue<P>>, action: SessionIntent<P>) -> Self {
        Self::new(
            queue,
            QueuedIntent {
                action,
                admission: None,
                correlation: None,
                origin: None,
            },
        )
    }

    /// The boundary a host announces when it starts handling this action, so
    /// a surface can attribute the answers that follow to the command that
    /// asked. `None` for the urgent lane (interrupt, quit), which answers
    /// within whatever is already running.
    #[must_use]
    pub fn started(&self) -> Option<crate::ContentObservation<P>> {
        (!matches!(self.action, SessionIntent::Interrupt | SessionIntent::Quit)).then(|| {
            let started = match (&self.origin, &self.correlation) {
                (Some(origin), Some(correlation)) => Some(format!("{origin}:{correlation}")),
                (Some(origin), None) => Some(origin.to_string()),
                (None, correlation) => correlation.clone(),
            };
            crate::ContentObservation::IntentStarted(started)
        })
    }

    /// Whether the sender gave this action its own correlation: a command
    /// whose surface attributes the answers to it. A sender's origin tag
    /// alone is not one.
    #[must_use]
    pub fn correlated(&self) -> bool {
        self.correlation.is_some()
    }

    /// Split the action from its acknowledgement guard. Keep the guard alive
    /// for the complete host handling scope.
    #[must_use]
    pub fn into_parts(self) -> (SessionIntent<P>, IntentAcknowledgement<P>) {
        (self.action, self.acknowledgement)
    }
}

impl<P: ProtocolExtension> Deref for IntentDelivery<P> {
    type Target = SessionIntent<P>;

    fn deref(&self) -> &Self::Target {
        &self.action
    }
}

impl<P: ProtocolExtension> fmt::Debug for IntentDelivery<P> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.action.fmt(formatter)
    }
}

impl<P: ProtocolExtension> PartialEq<SessionIntent<P>> for IntentDelivery<P> {
    fn eq(&self, other: &SessionIntent<P>) -> bool {
        self.action == *other
    }
}

impl<P: ProtocolExtension> PartialEq for IntentDelivery<P> {
    fn eq(&self, other: &Self) -> bool {
        self.action == other.action
    }
}

impl<P: ProtocolExtension> Eq for IntentDelivery<P> {}

impl<P: ProtocolExtension> Clone for IntentSender<P> {
    fn clone(&self) -> Self {
        self.0.senders.fetch_add(1, Ordering::Relaxed);
        Self(Arc::clone(&self.0), self.1.clone())
    }
}

impl<P: ProtocolExtension> Drop for IntentSender<P> {
    fn drop(&mut self) {
        if self.0.senders.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.ready.notify_one();
            self.0.wake_waiter();
            self.0.run_shutdown_hooks();
        }
    }
}

impl<P: ProtocolExtension> IntentSender<P> {
    /// Requests process-exit abandonment rather than joined host shutdown.
    /// Call only after restoring the terminal; this outranks queued product work.
    /// It is never a planned handoff: it disarms one requested earlier.
    pub fn request_abandon_and_release(&self) {
        self.0.planned_handoff.store(false, Ordering::Release);
        self.request_release();
    }
    /// [`Self::request_abandon_and_release`] as a planned handoff (`/reload`,
    /// a host restart or an upgrade): the host records each node with work
    /// in flight as handed off before it releases, so the next host
    /// continues it once by itself. The flag is armed with the release
    /// request itself and the host consumes it.
    pub fn request_handoff_and_release(&self) {
        self.0.planned_handoff.store(true, Ordering::Release);
        self.request_release();
    }
    /// Asks the host to stop the session's work: a host's `stop_session`.
    /// The stop enters the mailbox in arrival order as
    /// [`SessionIntent::StopSession`] under a fresh ticket, and the host
    /// settles the ticket once the stop is durable ([`Self::stop_settled`]).
    ///
    /// # Errors
    /// Returns a typed refusal when the stop cannot enter the queue.
    #[allow(
        clippy::result_large_err,
        reason = "the refusal hands the rejected action back, as send's does"
    )]
    pub fn request_stop(&self) -> Result<StopTicket, IntentSendError<P>> {
        self.request_ticket(None)
    }

    /// Runs `hook` when the host settles a stop on this queue. A wire
    /// session uses it to wake connections waiting on the answer; the
    /// settle is not itself an observation.
    pub fn on_stop_settled(&self, hook: impl Fn() + Send + Sync + 'static) {
        *self
            .0
            .stop_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(hook));
    }

    /// Takes what the host settled the stop `ticket` to: `Ok` once it is
    /// durable, the host's reason when it could not record it, or a reason
    /// of its own when the host ended before it settled the stop. `None`
    /// while the stop is still under way.
    #[must_use]
    pub fn stop_settled(&self, ticket: StopTicket) -> Option<Result<(), String>> {
        let mut book = self
            .0
            .stops
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(at) = book
            .settled
            .iter()
            .position(|(settled, _)| *settled == ticket)
        {
            return book.settled.remove(at).map(|(_, outcome)| outcome);
        }
        (!self.0.receiver_alive.load(Ordering::Acquire))
            .then(|| Err("the session ended before the stop was answered".to_owned()))
    }

    fn request_release(&self) {
        self.0.shutdown_requested.store(true, Ordering::Release);
        self.0.abandon_and_release.store(true, Ordering::Release);
        self.0.urgent.fetch_or(PENDING_QUIT, Ordering::Release);
        self.0.ready.notify_one();
        self.0.wake_waiter();
        self.0.run_shutdown_hooks();
    }
    /// Enqueue through the bounded mailbox. Interrupt uses a coalescing urgent
    /// lane. Quit is independently coalesced and observed only after accepted
    /// ordinary work, so a saturated or briefly contended queue cannot strand
    /// terminal restoration.
    ///
    /// # Errors
    /// Returns a typed refusal when an ordinary action cannot enter the queue.
    #[allow(
        clippy::result_large_err,
        reason = "the refusal hands the rejected action back so the caller can retry it verbatim"
    )]
    pub fn send(&self, action: SessionIntent<P>) -> Result<(), IntentSendError<P>> {
        self.send_correlated(action, None)
    }

    /// This sender, its intents tagged with the origin `tag`: the
    /// `IntentStarted` of an intent sent with correlation `c` carries
    /// `<tag>:c`, and of one sent with none `<tag>`. It stays the same
    /// sender of the same queue.
    #[must_use]
    pub fn tagged(self, tag: impl Into<Arc<str>>) -> Self {
        let queue = Arc::clone(&self.0);
        // Count the new handle before the old one goes, so the queue never
        // sees its last sender leave.
        self.0.senders.fetch_add(1, Ordering::Relaxed);
        drop(self);
        Self(queue, Some(tag.into()))
    }

    /// This sender as a handle on its queue that is not one of its senders
    /// ([`WeakIntentSender`]): when it was the last, the host sees the last
    /// sender leave.
    #[must_use]
    pub fn downgrade(self) -> WeakIntentSender<P> {
        let weak = WeakIntentSender(Arc::clone(&self.0));
        drop(self);
        weak
    }

    /// [`Self::send`] with an opaque correlation the host echoes in
    /// [`SessionObservation::IntentStarted`] when it starts the action. Urgent
    /// actions (interrupt, quit) carry none.
    ///
    /// # Errors
    /// Returns a typed refusal when an ordinary action cannot enter the queue.
    #[allow(
        clippy::result_large_err,
        reason = "the refusal hands the rejected action back so the caller can retry it verbatim"
    )]
    pub fn send_correlated(
        &self,
        action: SessionIntent<P>,
        correlation: Option<String>,
    ) -> Result<(), IntentSendError<P>> {
        if !self.0.receiver_alive.load(Ordering::Acquire) {
            return Err(IntentSendError::Disconnected(action));
        }
        // A flag the host reads, never queued work: nothing answers it.
        if matches!(action, SessionIntent::Promptable) {
            self.0.promptable.store(true, Ordering::Release);
            return Ok(());
        }
        let urgent_bit = match &action {
            SessionIntent::Interrupt => Some(URGENT_INTERRUPT),
            SessionIntent::Quit => Some(PENDING_QUIT),
            _ => None,
        };
        if let Some(urgent_bit) = urgent_bit {
            if urgent_bit == PENDING_QUIT {
                self.0.shutdown_requested.store(true, Ordering::Release);
            }
            self.0.urgent.fetch_or(urgent_bit, Ordering::Release);
            self.0.ready.notify_one();
            self.0.wake_waiter();
            if urgent_bit == PENDING_QUIT {
                self.0.run_shutdown_hooks();
            }
            return Ok(());
        }
        let nonrepeatable = nonrepeatable_bit(&action);
        let admission = match nonrepeatable {
            Some(bit) => match self.0.reserve_nonrepeatable(bit) {
                Some(admission) => Some(admission),
                None => return Err(IntentSendError::Duplicate(action)),
            },
            None => None,
        };
        let Ok(mut queue) = self.0.queue.lock() else {
            if let Some(admission) = admission {
                self.0.release_nonrepeatable(admission);
            }
            return Err(IntentSendError::Disconnected(action));
        };
        if !self.0.receiver_alive.load(Ordering::Acquire) {
            if let Some(admission) = admission {
                self.0.release_nonrepeatable(admission);
            }
            return Err(IntentSendError::Disconnected(action));
        }
        if queue.len() >= INTENT_QUEUE_CAPACITY {
            if let Some(admission) = admission {
                self.0.release_nonrepeatable(admission);
            }
            return Err(IntentSendError::Saturated(action));
        }
        queue.push_back(QueuedIntent {
            action,
            admission,
            correlation,
            origin: self.1.clone(),
        });
        self.0.ready.notify_one();
        self.0.wake_waiter();
        Ok(())
    }
}

impl<P: ProtocolExtension> Clone for WeakIntentSender<P> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<P: ProtocolExtension> WeakIntentSender<P> {
    /// An untagged sender of the queue, or `None` once its last sender
    /// left: a queue whose host saw that stays closed.
    #[must_use]
    pub fn upgrade(&self) -> Option<IntentSender<P>> {
        self.0
            .senders
            .try_update(Ordering::AcqRel, Ordering::Acquire, |senders| {
                (senders > 0).then_some(senders + 1)
            })
            .ok()?;
        Some(IntentSender(Arc::clone(&self.0), None))
    }
}

impl<P: ProtocolExtension> Drop for IntentReceiver<P> {
    fn drop(&mut self) {
        self.0.receiver_alive.store(false, Ordering::Release);
        self.0.ready.notify_all();
        self.0
            .waiter
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }
}

impl<P: ProtocolExtension> IntentReceiver<P> {
    /// A read-only interrupt/shutdown signal for the attached root's storage
    /// owner. Never bind this to children sharing its stores; they use their
    /// own cancellation and [`Self::on_shutdown`]. This never
    /// consumes an intent or acknowledges cancellation.
    #[must_use]
    pub fn storage_interrupt_probe(&self) -> Arc<dyn Fn() -> bool + Send + Sync> {
        let queue = Arc::clone(&self.0);
        Arc::new(move || {
            queue.urgent.load(Ordering::Acquire) & URGENT_INTERRUPT != 0
                || queue.shutdown_requested.load(Ordering::Acquire)
                || queue.senders.load(Ordering::Acquire) == 0
        })
    }

    /// Runs `hook` once, when shutdown is first requested or the last sender
    /// drops, and at once when that already happened. Process shutdown only:
    /// an interrupt of the root must not abandon a child's retained
    /// publication.
    pub fn on_shutdown(&self, hook: impl FnOnce() + Send + 'static) {
        let mut hooks = self
            .0
            .shutdown_hooks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match hooks.as_mut() {
            Some(pending) if !self.0.shutting_down() => pending.push(Box::new(hook)),
            _ => {
                drop(hooks);
                hook();
            }
        }
    }

    /// Whether the producer requested the bounded process-exit release path.
    #[must_use]
    pub fn abandon_and_release_requested(&self) -> bool {
        self.0.abandon_and_release.load(Ordering::Acquire)
    }

    /// Takes a planned handoff the release request armed: `true` once, for
    /// the release that records it.
    #[must_use]
    pub fn take_planned_handoff(&self) -> bool {
        self.0.planned_handoff.swap(false, Ordering::AcqRel)
    }

    /// Whether a planned handoff is armed, without taking it: the running
    /// turn's quit reads it to choose its cancel grace.
    #[must_use]
    pub fn planned_handoff(&self) -> bool {
        self.0.planned_handoff.load(Ordering::Acquire)
    }

    /// Settles the stop `ticket` to `outcome`: `Ok` once the stop is
    /// durable. Its asker takes it ([`IntentSender::stop_settled`]). Wakes
    /// the mailbox and whatever [`IntentSender::on_stop_settled`] registered,
    /// so the answer does not wait for a later frame.
    pub fn settle_stop(&self, ticket: StopTicket, outcome: Result<(), String>) {
        let hook = {
            let mut book = self
                .0
                .stops
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if book.settled.len() >= SETTLED_STOPS {
                book.settled.pop_front();
            }
            book.settled.push_back((ticket, outcome));
            self.0
                .stop_hook
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        };
        self.0.ready.notify_all();
        self.0.wake_waiter();
        if let Some(hook) = hook {
            hook();
        }
    }

    /// Whether the product has painted its first interactive frame.
    #[must_use]
    pub fn is_promptable(&self) -> bool {
        self.0.promptable.load(Ordering::Acquire)
    }

    fn take_urgent(&self) -> Option<SessionIntent<P>> {
        if self.abandon_and_release_requested()
            && let Some(quit) = self.take_pending_quit()
        {
            return Some(quit);
        }
        loop {
            let pending = self.0.urgent.load(Ordering::Acquire);
            let (bit, action) = if pending & URGENT_INTERRUPT != 0 {
                (URGENT_INTERRUPT, SessionIntent::Interrupt)
            } else {
                return None;
            };
            if self
                .0
                .urgent
                .compare_exchange(pending, pending & !bit, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(action);
            }
        }
    }

    fn take_pending_quit(&self) -> Option<SessionIntent<P>> {
        let previous = self.0.urgent.fetch_and(!PENDING_QUIT, Ordering::AcqRel);
        (previous & PENDING_QUIT != 0).then_some(SessionIntent::Quit)
    }

    /// Resolves once [`Self::try_recv`] has something to report: an ordinary
    /// action, the urgent lane, a pending quit, or the last producer gone.
    /// The waiting task registers before it looks, so an action that arrives
    /// between the look and the wait still wakes it. One task waits at a
    /// time: a second concurrent `ready` takes the registration over.
    pub fn ready(&self) -> impl Future<Output = ()> + '_ {
        std::future::poll_fn(move |context| {
            {
                let mut waiter = self
                    .0
                    .waiter
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if !waiter
                    .as_ref()
                    .is_some_and(|waiter| waiter.will_wake(context.waker()))
                {
                    *waiter = Some(context.waker().clone());
                }
            }
            if self.has_ready() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
    }

    fn has_ready(&self) -> bool {
        self.0.urgent.load(Ordering::Acquire) != 0
            || self.0.senders.load(Ordering::Acquire) == 0
            || !self.0.queue.lock().is_ok_and(|queue| queue.is_empty())
    }

    /// Pop one action without waiting.
    ///
    /// # Errors
    /// Returns `Empty` when no action is ready or `Disconnected` on poison.
    pub fn try_recv(&self) -> Result<IntentDelivery<P>, mpsc::TryRecvError> {
        if let Some(action) = self.take_urgent() {
            return Ok(IntentDelivery::immediate(&self.0, action));
        }
        let action = self
            .0
            .queue
            .lock()
            .map_err(|_| mpsc::TryRecvError::Disconnected)?
            .pop_front();
        if let Some(action) = action {
            return Ok(IntentDelivery::new(&self.0, action));
        }
        if let Some(action) = self.take_pending_quit() {
            return Ok(IntentDelivery::immediate(&self.0, action));
        }
        Err(if self.0.senders.load(Ordering::Acquire) == 0 {
            mpsc::TryRecvError::Disconnected
        } else {
            mpsc::TryRecvError::Empty
        })
    }

    /// Wait for the next action.
    ///
    /// # Errors
    /// Returns `RecvError` if the mailbox mutex is poisoned.
    pub fn recv(&self) -> Result<IntentDelivery<P>, mpsc::RecvError> {
        if let Some(action) = self.take_urgent() {
            return Ok(IntentDelivery::immediate(&self.0, action));
        }
        let mut queue = self.0.queue.lock().map_err(|_| mpsc::RecvError)?;
        loop {
            if let Some(action) = self.take_urgent() {
                return Ok(IntentDelivery::immediate(&self.0, action));
            }
            if let Some(action) = queue.pop_front() {
                return Ok(IntentDelivery::new(&self.0, action));
            }
            if let Some(action) = self.take_pending_quit() {
                return Ok(IntentDelivery::immediate(&self.0, action));
            }
            if self.0.senders.load(Ordering::Acquire) == 0 {
                return Err(mpsc::RecvError);
            }
            queue = self
                .0
                .ready
                .wait_timeout(queue, Duration::from_millis(25))
                .map_err(|_| mpsc::RecvError)?
                .0;
        }
    }

    /// Wait up to `timeout` for the next action.
    ///
    /// This preserves the urgent lane and disconnection semantics while letting
    /// a product host service independent completion sources without polling.
    ///
    /// # Errors
    /// Returns `Timeout` when no action arrives before the deadline and
    /// `Disconnected` when the last producer is gone or the mailbox is poisoned.
    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<IntentDelivery<P>, mpsc::RecvTimeoutError> {
        if let Some(action) = self.take_urgent() {
            return Ok(IntentDelivery::immediate(&self.0, action));
        }
        let deadline = Instant::now() + timeout;
        let mut queue = self
            .0
            .queue
            .lock()
            .map_err(|_| mpsc::RecvTimeoutError::Disconnected)?;
        loop {
            if let Some(action) = self.take_urgent() {
                return Ok(IntentDelivery::immediate(&self.0, action));
            }
            if let Some(action) = queue.pop_front() {
                return Ok(IntentDelivery::new(&self.0, action));
            }
            if let Some(action) = self.take_pending_quit() {
                return Ok(IntentDelivery::immediate(&self.0, action));
            }
            if self.0.senders.load(Ordering::Acquire) == 0 {
                return Err(mpsc::RecvTimeoutError::Disconnected);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(mpsc::RecvTimeoutError::Timeout);
            }
            let remaining = deadline.saturating_duration_since(now);
            let waited = self
                .0
                .ready
                .wait_timeout(queue, remaining)
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected)?;
            queue = waited.0;
            if waited.1.timed_out() {
                return Err(mpsc::RecvTimeoutError::Timeout);
            }
        }
    }
}

/// Construct the bounded product control channel.
#[must_use]
pub fn intent_channel<P: ProtocolExtension>() -> (IntentSender<P>, IntentReceiver<P>) {
    let shared = Arc::new(IntentQueue {
        shutdown_requested: AtomicBool::new(false),
        promptable: AtomicBool::new(false),
        abandon_and_release: AtomicBool::new(false),
        planned_handoff: AtomicBool::new(false),
        queue: Mutex::new(VecDeque::with_capacity(INTENT_QUEUE_CAPACITY)),
        urgent: AtomicU8::new(0),
        nonrepeatable: AtomicU8::new(0),
        nonrepeatable_generations: std::array::from_fn(|_| AtomicU64::new(0)),
        next_generation: AtomicU64::new(1),
        senders: AtomicUsize::new(1),
        receiver_alive: AtomicBool::new(true),
        ready: Condvar::new(),
        waiter: Mutex::new(None),
        shutdown_hooks: Mutex::new(Some(Vec::new())),
        stops: Mutex::new(StopBook::default()),
        stop_hook: Mutex::new(None),
    });
    (
        IntentSender(Arc::clone(&shared), None),
        IntentReceiver(shared),
    )
}

type Notifier = Arc<dyn Fn() + Send + Sync>;

struct ObservationQueue<P: ProtocolExtension> {
    queue: Mutex<VecDeque<SessionObservation<P>>>,
    ready: Condvar,
    transcript_gaps: Mutex<BTreeSet<TranscriptScope>>,
    correlation_gap: AtomicBool,
    presentation_revision: AtomicU64,
    senders: AtomicUsize,
    notify: Mutex<Option<Notifier>>,
    /// The notifier has fired since the receiver last found the queue empty.
    /// Set and cleared only under the `queue` lock, so no edge is lost.
    notified: AtomicBool,
}

impl<P: ProtocolExtension> ObservationQueue<P> {
    /// Claims this push's edge; the caller holds the `queue` lock and calls
    /// [`Self::fire`] after releasing it when this returns true.
    fn claim_edge(&self) -> bool {
        !self.notified.swap(true, Ordering::AcqRel)
    }

    fn fire(&self) {
        let notify = self
            .notify
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(notify) = notify {
            notify();
        }
    }
}

/// Bounded nonblocking producer for observational TUI events.
pub struct ObservationSender<P: ProtocolExtension = Core>(Arc<ObservationQueue<P>>);

/// Bounded consumer for observational TUI events.
pub struct ObservationReceiver<P: ProtocolExtension = Core>(Arc<ObservationQueue<P>>);

impl<P: ProtocolExtension> Clone for ObservationSender<P> {
    fn clone(&self) -> Self {
        self.0.senders.fetch_add(1, Ordering::Relaxed);
        Self(Arc::clone(&self.0))
    }
}

impl<P: ProtocolExtension> Drop for ObservationSender<P> {
    fn drop(&mut self) {
        if self.0.senders.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.ready.notify_all();
            // The receiver learns of the disconnection only by looking.
            self.0.fire();
        }
    }
}

impl<P: ProtocolExtension> Drop for ObservationReceiver<P> {
    fn drop(&mut self) {
        self.0
            .notify
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }
}

/// Construct the product observation channel. Saturation drops replaceable
/// deltas, while latest status/snapshots evict older observations.
#[must_use]
pub fn observation_channel<P: ProtocolExtension>() -> (ObservationSender<P>, ObservationReceiver<P>)
{
    let shared = Arc::new(ObservationQueue {
        queue: Mutex::new(VecDeque::with_capacity(OBSERVATION_QUEUE_CAPACITY)),
        ready: Condvar::new(),
        transcript_gaps: Mutex::default(),
        correlation_gap: AtomicBool::new(false),
        presentation_revision: AtomicU64::new(0),
        senders: AtomicUsize::new(1),
        notify: Mutex::new(None),
        notified: AtomicBool::new(false),
    });
    (
        ObservationSender(Arc::clone(&shared)),
        ObservationReceiver(shared),
    )
}

pub(crate) fn latest_state_event<P: ProtocolExtension>(event: &SessionObservation<P>) -> bool {
    if let SessionObservation::Extension(extension) = event {
        return extension.latest_state();
    }
    matches!(
        event,
        SessionObservation::PresentationSnapshot(_)
            | SessionObservation::ProviderSnapshot { .. }
            | SessionObservation::Activity { .. }
            | SessionObservation::AgentAttached(_)
            | SessionObservation::Status { .. }
            | SessionObservation::SetBusy(_)
            // Recovery replaces the terminal context-capacity presentation.
            | SessionObservation::ContextCapacityRecovery(_)
            | SessionObservation::Binding { .. }
            | SessionObservation::QueueSnapshot { .. }
            | SessionObservation::PromptHistorySnapshot(_)
            | SessionObservation::ResumeSessions(_)
            | SessionObservation::TemplateReadiness(_)
            | SessionObservation::Usage(_)
            | SessionObservation::TurnUsage { .. }
            | SessionObservation::SubscriptionUsage(_)
            | SessionObservation::UsageLimits(_)
            | SessionObservation::SessionInfo(_)
            | SessionObservation::ReattachReady
            | SessionObservation::SessionOpening
            | SessionObservation::SessionOpened { .. }
            | SessionObservation::TopologyNavigatorSnapshot(_)
            | SessionObservation::TopologyNavigatorUnavailable { .. }
            | SessionObservation::PendingContinuation { .. }
            | SessionObservation::LastUserPromptAt(_)
            | SessionObservation::Processes { .. }
    )
}

/// A non-error notice row: the one transcript row a full queue may replace.
fn replaceable_diagnostic_event<P: ProtocolExtension>(event: &SessionObservation<P>) -> bool {
    matches!(
        event,
        SessionObservation::RestoredPause(_) | SessionObservation::StorageAdmissionRefused(_)
    ) || matches!(event, SessionObservation::Row(TranscriptRow::Notice(notice)) if notice.tone != NoticeTone::Error)
}

/// An error row or a published refusal: what eviction must never drop.
fn terminal_error_event<P: ProtocolExtension>(event: &SessionObservation<P>) -> bool {
    match event {
        SessionObservation::Row(row) => row.is_error(),
        SessionObservation::ModelRefusal { .. } => true,
        _ => false,
    }
}

fn correlation_receipt<P: ProtocolExtension>(event: &SessionObservation<P>) -> bool {
    matches!(
        event,
        SessionObservation::ToolInvocationOrigin { .. }
            | SessionObservation::IntentStarted(_)
            | SessionObservation::PromptAdmitted(_)
            | SessionObservation::PromptStarted(_)
            | SessionObservation::DraftRejected(_)
            | SessionObservation::ClearView
            | SessionObservation::SessionOpened { .. }
            | SessionObservation::PromptCorrelationReset
            | SessionObservation::TurnLifecycle(_)
    )
}

fn admission_event<P: ProtocolExtension>(event: &SessionObservation<P>) -> bool {
    matches!(
        event,
        SessionObservation::ToolInvocationOrigin { .. }
            | SessionObservation::IntentStarted(_)
            | SessionObservation::PromptAdmitted(_)
            | SessionObservation::PromptStarted(_)
            | SessionObservation::PromptCorrelationReset
            | SessionObservation::DraftAccepted(_)
            | SessionObservation::DraftRejected(_)
            | SessionObservation::DraftRestored(_)
            | SessionObservation::QueueEditApplied { .. }
            | SessionObservation::QueueEditRefused { .. }
            | SessionObservation::TurnLifecycle(_)
            | SessionObservation::JobStarted { .. }
            | SessionObservation::JobPromoted { .. }
            | SessionObservation::JobTerminal { .. }
    )
}

pub(crate) fn transcript_observation<P: ProtocolExtension>(event: &SessionObservation<P>) -> bool {
    match event {
        SessionObservation::Row(TranscriptRow::Notice(_)) => false,
        SessionObservation::Row(_)
        | SessionObservation::UserMessage { .. }
        | SessionObservation::AssistantDelta(_)
        | SessionObservation::AssistantDone
        | SessionObservation::OutputDiscarded { .. }
        | SessionObservation::ReasoningDelta { .. }
        | SessionObservation::ReasoningSealed { .. }
        | SessionObservation::ToolLine { .. }
        | SessionObservation::TranscriptPatch(_)
        | SessionObservation::TranscriptStream { .. }
        | SessionObservation::PresentationSnapshot(_) => true,
        _ => false,
    }
}

/// Whether two observations are the same latest-state kind, so the newer one
/// supersedes the older in the mailbox.
fn same_kind<P: ProtocolExtension>(
    queued: &SessionObservation<P>,
    event: &SessionObservation<P>,
) -> bool {
    match (queued, event) {
        (
            SessionObservation::PresentationSnapshot(queued),
            SessionObservation::PresentationSnapshot(event),
        ) => queued.scope == event.scope,
        (SessionObservation::Extension(queued), SessionObservation::Extension(event)) => {
            queued.same_kind(event)
        }
        _ => std::mem::discriminant(queued) == std::mem::discriminant(event),
    }
}

/// Park, clear, lifecycle, and job edges are the record a face must not lose.
fn retained_edge<P: ProtocolExtension>(event: &SessionObservation<P>) -> bool {
    matches!(
        event,
        SessionObservation::ModelRefusal { .. }
            | SessionObservation::TurnLifecycle(_)
            | SessionObservation::JobStarted { .. }
            | SessionObservation::JobPromoted { .. }
            | SessionObservation::JobTerminal { .. }
            | SessionObservation::CredentialWait { .. }
            | SessionObservation::OutputDiscarded { .. }
    )
}

fn state_eviction<P: ProtocolExtension>(queue: &VecDeque<SessionObservation<P>>) -> Option<usize> {
    let terminal_error = queue.iter().rposition(terminal_error_event);
    queue.iter().enumerate().find_map(|(index, event)| {
        (!latest_state_event(event) && Some(index) != terminal_error && !retained_edge(event))
            .then_some(index)
    })
}

fn streaming_delta<P: ProtocolExtension>(event: &SessionObservation<P>) -> bool {
    matches!(
        event,
        SessionObservation::AssistantDelta(_)
            | SessionObservation::ReasoningDelta { .. }
            | SessionObservation::TranscriptStream { .. }
    )
}

/// A transcript row other than a notice, or a transcript patch. Neither has a
/// JSONL v1 line, and `note_transcript_gap` already repairs the loss. A notice
/// is not spent: it would disappear with no resnapshot.
fn resnapshot_repaired<P: ProtocolExtension>(event: &SessionObservation<P>) -> bool {
    match event {
        SessionObservation::Row(row) => !matches!(row, TranscriptRow::Notice(_)),
        SessionObservation::TranscriptPatch(_) => true,
        _ => false,
    }
}

/// A credential park and its clear both have to land. Spend a streaming delta
/// first, then a transcript row other than a notice or a transcript patch. A
/// tool line is a JSONL v1 `tool` record the face never gets back, so it is
/// spent only when neither of those is queued. A latest-state snapshot and a
/// retained edge are never victims.
fn credential_wait_victim<P: ProtocolExtension>(
    queue: &VecDeque<SessionObservation<P>>,
) -> Option<usize> {
    queue
        .iter()
        .position(streaming_delta)
        .or_else(|| queue.iter().position(resnapshot_repaired))
        .or_else(|| {
            queue
                .iter()
                .position(|queued| matches!(queued, SessionObservation::ToolLine { .. }))
        })
}

/// Picks the queue slot a saturated admission may evict for `event`, or `None`
/// when nothing queued is expendable enough to make room. Refusals, lifecycle
/// receipts, and credential waits are never eviction candidates: they are the
/// record a dropped observation must not take with it.
fn eviction_index<P: ProtocolExtension>(
    event: &SessionObservation<P>,
    queue: &VecDeque<SessionObservation<P>>,
) -> Option<usize> {
    if latest_state_event(event) {
        state_eviction(queue)
    } else if matches!(
        event,
        SessionObservation::AssistantDone | SessionObservation::OutputDiscarded { .. }
    ) {
        queue.iter().position(|queued| {
            !latest_state_event(queued)
                && !retained_edge(queued)
                && !matches!(queued, SessionObservation::AssistantDone)
        })
    } else if terminal_error_event(event) {
        queue
            .iter()
            .position(replaceable_diagnostic_event)
            .or_else(|| {
                queue
                    .iter()
                    .position(|queued| !latest_state_event(queued) && !retained_edge(queued))
            })
    } else if replaceable_diagnostic_event(event) {
        queue
            .iter()
            .position(replaceable_diagnostic_event)
            .or_else(|| {
                queue.iter().position(|queued| {
                    !latest_state_event(queued)
                        && !replaceable_diagnostic_event(queued)
                        && !retained_edge(queued)
                        && !matches!(queued, SessionObservation::AssistantDone)
                })
            })
    } else if admission_event(event) {
        queue.iter().position(|queued| {
            !admission_event(queued)
                && !terminal_error_event(queued)
                && !latest_state_event(queued)
                && !retained_edge(queued)
        })
    } else if matches!(event, SessionObservation::CredentialWait { .. }) {
        credential_wait_victim(queue)
    } else {
        None
    }
}

impl<P: ProtocolExtension> ObservationSender<P> {
    /// Allocates the next presentation revision for this face. Every sender
    /// wrapper over the channel shares the counter; exhaustion refuses rather
    /// than reusing a revision in the same face lifetime.
    #[must_use]
    pub fn next_presentation_revision(&self) -> Option<u64> {
        self.0
            .presentation_revision
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |revision| {
                revision.checked_add(1)
            })
            .ok()
            .and_then(|revision| revision.checked_add(1))
    }

    /// Whether a transcript-building observation was dropped since the latest
    /// successfully queued transcript replacement.
    #[must_use]
    pub fn transcript_resnapshot_required(&self) -> bool {
        self.transcript_resnapshot_required_for(&TranscriptScope::Root)
    }

    /// Whether this scope lost a content update since its latest queued
    /// baseline. A root replacement never clears a child's projection debt.
    #[must_use]
    pub fn transcript_resnapshot_required_for(&self, scope: &TranscriptScope) -> bool {
        self.0
            .transcript_gaps
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(scope)
    }

    fn note_transcript_gap(&self, event: &SessionObservation<P>) {
        if !transcript_observation(event) {
            return;
        }
        let scope = match event {
            SessionObservation::TranscriptPatch(patch) => patch.scope.clone(),
            SessionObservation::PresentationSnapshot(snapshot) => snapshot.scope.clone(),
            SessionObservation::TranscriptStream { scope, .. } => scope.clone(),
            _ => TranscriptScope::Root,
        };
        self.0
            .transcript_gaps
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(scope);
    }

    fn supersede_baseline(
        &self,
        snapshot: &crate::views::PresentationSnapshot,
        queue: &mut VecDeque<SessionObservation<P>>,
    ) {
        // A new baseline covers every older queued patch in this scope.
        queue.retain(|queued| match queued {
            SessionObservation::TranscriptPatch(patch) => patch.scope != snapshot.scope,
            SessionObservation::TranscriptStream { scope, .. } => *scope != snapshot.scope,
            _ => true,
        });
        if matches!(snapshot.scope, TranscriptScope::Agent(_)) {
            // A face has one open agent view. Replacing it retires queued
            // content and gap bookkeeping for the old agent.
            queue.retain(|queued| {
                let scope = match queued {
                    SessionObservation::PresentationSnapshot(old) => &old.scope,
                    SessionObservation::TranscriptPatch(old) => &old.scope,
                    SessionObservation::TranscriptStream { scope, .. } => scope,
                    _ => return true,
                };
                *scope == TranscriptScope::Root || *scope == snapshot.scope
            });
            self.0
                .transcript_gaps
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|scope| *scope == TranscriptScope::Root || *scope == snapshot.scope);
        }
    }

    /// Bounded delivery that never waits for renderer capacity. Mutex ownership
    /// is short; at saturation required final/state events evict only a
    /// replaceable transcript/delta/progress observation. A credential-wait
    /// park and its clear are retained the same way: saturation may drop a
    /// streaming delta, then a transcript row other than a notice or a
    /// transcript patch, and a tool line only when neither is queued. Neither
    /// wait is itself an eviction candidate.
    /// A latest-state snapshot is not spent to make room. Latest-state
    /// variants supersede only their own older value, and the newest error
    /// remains until the renderer can observe it alongside the terminal state.
    ///
    /// # Errors
    /// Returns the original replaceable event if bounded capacity cannot admit it.
    #[allow(
        clippy::result_large_err,
        reason = "typed backpressure returns ownership of the exact refused observation"
    )]
    pub fn send(
        &self,
        event: impl Into<SessionObservation<P>>,
    ) -> Result<(), SessionObservation<P>> {
        let event = event.into();
        let mut queue = self.0.queue.lock().map_err(|_| event.clone())?;
        let result = self.admit(event, &mut queue);
        let edge = (!queue.is_empty() || self.0.correlation_gap.load(Ordering::Acquire))
            && self.0.claim_edge();
        drop(queue);
        if edge {
            self.0.fire();
        }
        result
    }

    #[allow(
        clippy::result_large_err,
        reason = "typed backpressure returns ownership of the exact refused observation"
    )]
    fn admit(
        &self,
        mut event: SessionObservation<P>,
        queue: &mut VecDeque<SessionObservation<P>>,
    ) -> Result<(), SessionObservation<P>> {
        if coalesce_delta(queue.back_mut(), &event) {
            return Ok(());
        }
        if let SessionObservation::TranscriptPatch(next) = &event
            && let Some((index, previous)) = queue
                .iter()
                .enumerate()
                .rev()
                .take_while(|(_, queued)| match queued {
                    SessionObservation::TranscriptPatch(patch) => {
                        patch.scope != next.scope || patch.to_revision == next.from_revision
                    }
                    SessionObservation::PresentationSnapshot(snapshot) => {
                        snapshot.scope != next.scope
                    }
                    SessionObservation::TranscriptStream { scope, .. } => *scope != next.scope,
                    _ => false,
                })
                .find_map(|(index, queued)| {
                    let SessionObservation::TranscriptPatch(previous) = queued else {
                        return None;
                    };
                    (previous.scope == next.scope).then_some((index, previous))
                })
        {
            let Some(merged) = previous.merged(next) else {
                self.note_transcript_gap(&event);
                return Err(event);
            };
            queue.remove(index);
            event = SessionObservation::TranscriptPatch(merged);
        }
        if latest_state_event(&event) {
            let same_kind = |queued: &SessionObservation<P>| same_kind(queued, &event);
            if let Some(index) = queue.iter().position(same_kind)
                && queue
                    .remove(index)
                    .as_ref()
                    .is_some_and(correlation_receipt)
            {
                self.0.correlation_gap.store(true, Ordering::Release);
            }
            if let SessionObservation::PresentationSnapshot(snapshot) = &event {
                self.supersede_baseline(snapshot, queue);
            }
        }
        if agent_observation(&event)
            && queue
                .iter()
                .filter(|event| agent_observation(event))
                .count()
                >= AGENT_OBSERVATION_SHARE
        {
            self.note_transcript_gap(&event);
            return Err(event);
        }
        if queue.len() >= OBSERVATION_QUEUE_CAPACITY {
            let Some(index) = eviction_index(&event, queue) else {
                self.note_transcript_gap(&event);
                if correlation_receipt(&event) {
                    self.0.correlation_gap.store(true, Ordering::Release);
                    self.0.ready.notify_one();
                }
                return Err(event);
            };
            if let Some(evicted) = queue.remove(index) {
                self.note_transcript_gap(&evicted);
                if correlation_receipt(&evicted) {
                    self.0.correlation_gap.store(true, Ordering::Release);
                }
            }
        }
        let replaces_transcript = if let SessionObservation::PresentationSnapshot(snapshot) = &event
        {
            Some(snapshot.scope.clone())
        } else {
            None
        };
        queue.push_back(event);
        if let Some(scope) = replaces_transcript {
            self.0
                .transcript_gaps
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&scope);
        }
        self.0.ready.notify_one();
        Ok(())
    }
}

impl<P: ProtocolExtension> ObservationReceiver<P> {
    /// A producer lost a scoped update and has not queued its replacement
    /// yet. A consumer can request resync even if the last patch was lost and
    /// no later patch arrives to reveal a revision mismatch.
    #[must_use]
    pub fn transcript_resnapshot_required_for(&self, scope: &TranscriptScope) -> bool {
        self.0
            .transcript_gaps
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(scope)
    }

    /// Install the hook a consumer that waits outside this mailbox is woken
    /// by. It runs on the producer's thread, outside the queue lock, once per
    /// empty to non-empty edge (the edge re-arms when [`Self::try_recv`] or
    /// [`Self::recv_timeout`] finds the queue empty) and once when the last
    /// sender is dropped. It must not block. Dropping the receiver removes it.
    pub fn set_notify(&self, notify: impl Fn() + Send + Sync + 'static) {
        *self
            .0
            .notify
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(notify));
    }

    /// Pop one observation without waiting.
    ///
    /// # Errors
    /// Returns `Empty` when none is ready or `Disconnected` on poison.
    pub fn try_recv(&self) -> Result<SessionObservation<P>, mpsc::TryRecvError> {
        let mut queue = self
            .0
            .queue
            .lock()
            .map_err(|_| mpsc::TryRecvError::Disconnected)?;
        if self.0.correlation_gap.swap(false, Ordering::AcqRel) {
            return Ok(SessionObservation::PromptCorrelationReset);
        }
        let event = queue.pop_front();
        if event.is_none() {
            self.0.notified.store(false, Ordering::Release);
        }
        event.ok_or_else(|| {
            if self.0.senders.load(Ordering::Acquire) == 0 {
                mpsc::TryRecvError::Disconnected
            } else {
                mpsc::TryRecvError::Empty
            }
        })
    }

    /// Wait up to `timeout` for one observation.
    ///
    /// # Errors
    /// Returns `Timeout` when the deadline expires or `Disconnected` on poison.
    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<SessionObservation<P>, mpsc::RecvTimeoutError> {
        let deadline = Instant::now() + timeout;
        let mut queue = self
            .0
            .queue
            .lock()
            .map_err(|_| mpsc::RecvTimeoutError::Disconnected)?;
        loop {
            if self.0.correlation_gap.swap(false, Ordering::AcqRel) {
                return Ok(SessionObservation::PromptCorrelationReset);
            }
            if let Some(event) = queue.pop_front() {
                return Ok(event);
            }
            self.0.notified.store(false, Ordering::Release);
            if self.0.senders.load(Ordering::Acquire) == 0 {
                return Err(mpsc::RecvTimeoutError::Disconnected);
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err(mpsc::RecvTimeoutError::Timeout);
            };
            let (next, wait) = self
                .0
                .ready
                .wait_timeout(queue, remaining)
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected)?;
            queue = next;
            if wait.timed_out() && queue.is_empty() {
                return Err(mpsc::RecvTimeoutError::Timeout);
            }
        }
    }

    /// Drain all observations currently available.
    pub fn try_iter(&self) -> impl Iterator<Item = SessionObservation<P>> + '_ {
        std::iter::from_fn(|| self.try_recv().ok())
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests;
