//! The two bounded in-process mailboxes between a driver and the hosted
//! session, with the semantics a product surface depends on.
//!
//! Intents ([`intent_channel`]): a bounded FIFO with a coalescing urgent lane
//! for `Interrupt`, an independently coalesced `Quit` observed only after
//! accepted ordinary work, and one-in-flight admission for the non-repeatable
//! session mutations (`ClearSession`, `Compact`, `ForkSession`) released by an
//! RAII acknowledgement.
//!
//! Observations ([`observation_channel`]): a bounded queue that never blocks
//! the producer. Assistant deltas coalesce, latest-state snapshots supersede
//! their own older value, and at saturation final/state observations evict a
//! replaceable transcript/delta/progress observation; a dropped transcript
//! observation is remembered until a transcript replacement lands so the host
//! can resnapshot.

use std::collections::VecDeque;
use std::fmt;
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::intent::{Core, ObservationExtension, ProtocolExtension, SessionIntent};
use crate::limits::{COALESCED_DELTA_BYTES, INTENT_QUEUE_CAPACITY, OBSERVATION_QUEUE_CAPACITY};
use crate::observation::SessionObservation;
use crate::views::transcript::{NoticeScope, NoticeTone, TranscriptRow};

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
    background_processes: AtomicUsize,
    processes: Mutex<Arc<Vec<crate::views::ProcessView>>>,
    promptable: AtomicBool,
    abandon_and_release: AtomicBool,
    queue: Mutex<VecDeque<QueuedIntent<P>>>,
    urgent: AtomicU8,
    nonrepeatable: AtomicU8,
    nonrepeatable_generations: [AtomicU64; 3],
    next_generation: AtomicU64,
    senders: AtomicUsize,
    receiver_alive: AtomicBool,
    ready: Condvar,
}

struct QueuedIntent<P: ProtocolExtension> {
    action: SessionIntent<P>,
    admission: Option<NonrepeatableAdmission>,
    correlation: Option<String>,
}

#[derive(Clone, Copy)]
struct NonrepeatableAdmission {
    bit: u8,
    generation: u64,
}

const URGENT_INTERRUPT: u8 = 1;
const PENDING_QUIT: u8 = 1 << 1;
const NONREPEATABLE_CLEAR: u8 = 1;
const NONREPEATABLE_COMPACT: u8 = 1 << 1;
const NONREPEATABLE_FORK: u8 = 1 << 2;

const fn nonrepeatable_bit<P: ProtocolExtension>(action: &SessionIntent<P>) -> Option<u8> {
    match action {
        SessionIntent::ClearSession => Some(NONREPEATABLE_CLEAR),
        SessionIntent::Compact => Some(NONREPEATABLE_COMPACT),
        SessionIntent::ForkSession { .. } => Some(NONREPEATABLE_FORK),
        _ => None,
    }
}

const fn nonrepeatable_index(bit: u8) -> usize {
    bit.trailing_zeros() as usize
}

impl<P: ProtocolExtension> IntentQueue<P> {
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
}

impl<P: ProtocolExtension> IntentSendError<P> {
    /// Return the action that was not accepted.
    #[must_use]
    pub fn into_action(self) -> SessionIntent<P> {
        match self {
            Self::Saturated(action) | Self::Disconnected(action) | Self::Duplicate(action) => {
                action
            }
        }
    }
}

/// Nonblocking producer for the bounded UI → host control queue.
pub struct IntentSender<P: ProtocolExtension = Core>(Arc<IntentQueue<P>>);

/// Single host consumer for the bounded control queue.
pub struct IntentReceiver<P: ProtocolExtension = Core>(Arc<IntentQueue<P>>);

/// One exact action delivery. Its generation-bound acknowledgement is held
/// until this value, or the guard returned by [`Self::into_parts`], is dropped.
pub struct IntentDelivery<P: ProtocolExtension = Core> {
    action: SessionIntent<P>,
    acknowledgement: IntentAcknowledgement<P>,
    correlation: Option<String>,
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
        }
    }

    fn immediate(queue: &Arc<IntentQueue<P>>, action: SessionIntent<P>) -> Self {
        Self::new(
            queue,
            QueuedIntent {
                action,
                admission: None,
                correlation: None,
            },
        )
    }

    /// The boundary a host announces when it starts handling this action, so
    /// a surface can attribute the answers that follow to the command that
    /// asked. `None` for the urgent lane (interrupt, quit), which answers
    /// within whatever is already running.
    #[must_use]
    pub fn started(&self) -> Option<SessionObservation<P>> {
        (!matches!(self.action, SessionIntent::Interrupt | SessionIntent::Quit))
            .then(|| SessionObservation::IntentStarted(self.correlation.clone()))
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
        Self(Arc::clone(&self.0))
    }
}

impl<P: ProtocolExtension> Drop for IntentSender<P> {
    fn drop(&mut self) {
        if self.0.senders.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.ready.notify_one();
        }
    }
}

impl<P: ProtocolExtension> IntentSender<P> {
    /// Process-local host observation for reload/quit admission, not a wire intent.
    #[must_use]
    pub fn background_processes(&self) -> usize {
        self.0.background_processes.load(Ordering::Acquire)
    }
    /// Latest live and recent-terminal process view published by the host.
    #[must_use]
    pub fn processes(&self) -> Arc<Vec<crate::views::ProcessView>> {
        Arc::clone(
            &self
                .0
                .processes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
    /// Marks the first interactive frame ready for input. This local signal
    /// permits passive maintenance; it is not a session mutation or wire intent.
    pub fn mark_promptable(&self) {
        self.0.promptable.store(true, Ordering::Release);
    }
    /// Requests process-exit abandonment rather than joined host shutdown.
    /// Call only after restoring the terminal; this outranks queued product work.
    pub fn request_abandon_and_release(&self) {
        self.0.abandon_and_release.store(true, Ordering::Release);
        self.0.urgent.fetch_or(PENDING_QUIT, Ordering::Release);
        self.0.ready.notify_one();
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
        let urgent_bit = match &action {
            SessionIntent::Interrupt => Some(URGENT_INTERRUPT),
            SessionIntent::Quit => Some(PENDING_QUIT),
            _ => None,
        };
        if let Some(urgent_bit) = urgent_bit {
            self.0.urgent.fetch_or(urgent_bit, Ordering::Release);
            self.0.ready.notify_one();
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
        });
        self.0.ready.notify_one();
        Ok(())
    }
}

impl<P: ProtocolExtension> Drop for IntentReceiver<P> {
    fn drop(&mut self) {
        self.0.receiver_alive.store(false, Ordering::Release);
        self.0.ready.notify_all();
    }
}

impl<P: ProtocolExtension> IntentReceiver<P> {
    /// Replaces the observational process view without filling the event queue.
    pub fn set_processes(&self, processes: Vec<crate::views::ProcessView>) {
        self.set_background_processes(processes.iter().filter(|row| row.status.is_live()).count());
        *self
            .0
            .processes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::new(processes);
    }
    /// Publishes the host's running-process count to its attached shell.
    pub fn set_background_processes(&self, count: usize) {
        self.0.background_processes.store(count, Ordering::Release);
    }
    /// Whether the producer requested the bounded process-exit release path.
    #[must_use]
    pub fn abandon_and_release_requested(&self) -> bool {
        self.0.abandon_and_release.load(Ordering::Acquire)
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
        promptable: AtomicBool::new(false),
        background_processes: AtomicUsize::new(0),
        processes: Mutex::new(Arc::new(Vec::new())),
        abandon_and_release: AtomicBool::new(false),
        queue: Mutex::new(VecDeque::with_capacity(INTENT_QUEUE_CAPACITY)),
        urgent: AtomicU8::new(0),
        nonrepeatable: AtomicU8::new(0),
        nonrepeatable_generations: std::array::from_fn(|_| AtomicU64::new(0)),
        next_generation: AtomicU64::new(1),
        senders: AtomicUsize::new(1),
        receiver_alive: AtomicBool::new(true),
        ready: Condvar::new(),
    });
    (IntentSender(Arc::clone(&shared)), IntentReceiver(shared))
}

struct ObservationQueue<P: ProtocolExtension> {
    queue: Mutex<VecDeque<SessionObservation<P>>>,
    ready: Condvar,
    transcript_gap: AtomicBool,
    correlation_gap: AtomicBool,
    senders: AtomicUsize,
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
        }
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
        transcript_gap: AtomicBool::new(false),
        correlation_gap: AtomicBool::new(false),
        senders: AtomicUsize::new(1),
    });
    (
        ObservationSender(Arc::clone(&shared)),
        ObservationReceiver(shared),
    )
}

fn latest_state_event<P: ProtocolExtension>(event: &SessionObservation<P>) -> bool {
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
            | SessionObservation::AgentTranscript(_)
            | SessionObservation::TopologyNavigatorUnavailable { .. }
            | SessionObservation::PendingContinuation { .. }
            | SessionObservation::LastUserPromptAt(_)
    )
}

/// The marker a coalesced child stream carries in place of dropped rows: a
/// `tree` notice in the child's live view (spec 02 §5.4), so the operator
/// sees that rows are missing. It describes the live stream, not the child's
/// conversation, so it is never persisted.
pub(crate) fn child_gap_notice<P: ProtocolExtension>() -> SessionObservation<P> {
    SessionObservation::Row(TranscriptRow::info(
        NoticeScope::Tree,
        "earlier activity not shown",
    ))
}

/// A non-error notice row: the one transcript row a full queue may replace.
fn replaceable_diagnostic_event<P: ProtocolExtension>(event: &SessionObservation<P>) -> bool {
    matches!(event, SessionObservation::Row(TranscriptRow::Notice(notice)) if notice.tone != NoticeTone::Error)
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

fn transcript_observation<P: ProtocolExtension>(event: &SessionObservation<P>) -> bool {
    match event {
        SessionObservation::Row(TranscriptRow::Notice(_)) => false,
        SessionObservation::Row(_)
        | SessionObservation::UserMessage { .. }
        | SessionObservation::AssistantDelta(_) => true,
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
        (SessionObservation::Extension(queued), SessionObservation::Extension(event)) => {
            queued.same_kind(event)
        }
        _ => std::mem::discriminant(queued) == std::mem::discriminant(event),
    }
}

fn state_eviction<P: ProtocolExtension>(queue: &VecDeque<SessionObservation<P>>) -> Option<usize> {
    let terminal_error = queue.iter().rposition(terminal_error_event);
    queue.iter().enumerate().find_map(|(index, event)| {
        (!latest_state_event(event)
            && Some(index) != terminal_error
            && !matches!(
                event,
                SessionObservation::ModelRefusal { .. }
                    | SessionObservation::TurnLifecycle(_)
                    | SessionObservation::JobStarted { .. }
                    | SessionObservation::JobPromoted { .. }
                    | SessionObservation::JobTerminal { .. }
            ))
        .then_some(index)
    })
}

/// Picks the queue slot a saturated admission may evict for `event`, or `None`
/// when nothing queued is expendable enough to make room. Refusals and
/// lifecycle receipts are never eviction candidates: they are the terminal
/// evidence a dropped observation must not take with it.
fn eviction_index<P: ProtocolExtension>(
    event: &SessionObservation<P>,
    queue: &VecDeque<SessionObservation<P>>,
) -> Option<usize> {
    if latest_state_event(event) {
        state_eviction(queue)
    } else if matches!(event, SessionObservation::AssistantDone) {
        queue.iter().position(|queued| {
            !latest_state_event(queued)
                && !matches!(
                    queued,
                    SessionObservation::AssistantDone
                        | SessionObservation::ModelRefusal { .. }
                        | SessionObservation::TurnLifecycle(_)
                        | SessionObservation::JobStarted { .. }
                        | SessionObservation::JobPromoted { .. }
                        | SessionObservation::JobTerminal { .. }
                )
        })
    } else if terminal_error_event(event) {
        queue
            .iter()
            .position(replaceable_diagnostic_event)
            .or_else(|| {
                queue.iter().position(|queued| {
                    !latest_state_event(queued)
                        && !matches!(
                            queued,
                            SessionObservation::ModelRefusal { .. }
                                | SessionObservation::TurnLifecycle(_)
                                | SessionObservation::JobStarted { .. }
                                | SessionObservation::JobPromoted { .. }
                                | SessionObservation::JobTerminal { .. }
                        )
                })
            })
    } else if replaceable_diagnostic_event(event) {
        queue
            .iter()
            .position(replaceable_diagnostic_event)
            .or_else(|| {
                queue.iter().position(|queued| {
                    !latest_state_event(queued)
                        && !replaceable_diagnostic_event(queued)
                        && !matches!(
                            queued,
                            SessionObservation::AssistantDone
                                | SessionObservation::ModelRefusal { .. }
                                | SessionObservation::TurnLifecycle(_)
                                | SessionObservation::JobStarted { .. }
                                | SessionObservation::JobPromoted { .. }
                                | SessionObservation::JobTerminal { .. }
                        )
                })
            })
    } else if admission_event(event) {
        queue
            .iter()
            .position(|queued| !admission_event(queued) && !terminal_error_event(queued))
    } else {
        None
    }
}

fn coalesce_child_live<P: ProtocolExtension>(
    event: &mut SessionObservation<P>,
    queue: &mut VecDeque<SessionObservation<P>>,
) -> bool {
    let SessionObservation::ChildLive {
        agent_id,
        turn_id,
        op,
    } = event
    else {
        return false;
    };
    if let SessionObservation::AssistantDelta(delta) = op.as_mut()
        && let Some(SessionObservation::ChildLive {
            agent_id: prior_id,
            turn_id: prior_turn,
            op: prior,
        }) = queue.back_mut()
        && prior_id == agent_id
        && prior_turn == turn_id
        && let SessionObservation::AssistantDelta(pending) = prior.as_mut()
        && pending.len().saturating_add(delta.len()) <= COALESCED_DELTA_BYTES
    {
        pending.push_str(delta);
        return true;
    }
    let child_count = queue
        .iter()
        .filter(|queued| matches!(queued, SessionObservation::ChildLive { .. }))
        .count();
    if child_count < 120 {
        return false;
    }
    let same_child = |queued: &SessionObservation<P>| matches!(queued, SessionObservation::ChildLive {agent_id: id, turn_id: turn, ..} if id == agent_id && turn == turn_id);
    if queue.iter().any(|queued| same_child(queued) && matches!(queued, SessionObservation::ChildLive {op, ..} if op.as_ref() == &child_gap_notice())) { return true; }
    let replace = (child_count >= 128)
        .then(|| {
            queue.iter().rposition(same_child).or_else(|| {
                queue
                    .iter()
                    .position(|queued| matches!(queued, SessionObservation::ChildLive { .. }))
            })
        })
        .flatten();
    // Reserve eight addressed gap slots inside the child's total queue share.
    **op = child_gap_notice();
    if let Some(index) = replace {
        queue.remove(index);
        queue.push_back(event.clone());
        return true;
    }
    false
}

impl<P: ProtocolExtension> ObservationSender<P> {
    /// Whether a transcript-building observation was dropped since the latest
    /// successfully queued transcript replacement.
    #[must_use]
    pub fn transcript_resnapshot_required(&self) -> bool {
        self.0.transcript_gap.load(Ordering::Acquire)
    }

    /// Bounded delivery that never waits for renderer capacity. Mutex ownership
    /// is short; at saturation required final/state events evict only a
    /// replaceable transcript/delta/progress observation. Latest-state variants
    /// supersede only their own older value, and the newest error remains until
    /// the renderer can observe it alongside the terminal state.
    ///
    /// # Errors
    /// Returns the original replaceable event if bounded capacity cannot admit it.
    #[allow(
        clippy::result_large_err,
        reason = "typed backpressure returns ownership of the exact refused observation"
    )]
    pub fn send(&self, mut event: SessionObservation<P>) -> Result<(), SessionObservation<P>> {
        let mut queue = self.0.queue.lock().map_err(|_| event.clone())?;
        if coalesce_child_live(&mut event, &mut queue) {
            self.0.ready.notify_one();
            return Ok(());
        }
        if let SessionObservation::AssistantDelta(delta) = &mut event
            && let Some(SessionObservation::AssistantDelta(pending)) = queue.back_mut()
            && pending.len().saturating_add(delta.len()) <= COALESCED_DELTA_BYTES
        {
            pending.push_str(delta);
            return Ok(());
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
        }
        if queue.len() >= OBSERVATION_QUEUE_CAPACITY {
            let Some(index) = eviction_index(&event, &queue) else {
                if transcript_observation(&event) {
                    self.0.transcript_gap.store(true, Ordering::Release);
                }
                if correlation_receipt(&event) {
                    self.0.correlation_gap.store(true, Ordering::Release);
                    self.0.ready.notify_one();
                }
                return Err(event);
            };
            if let Some(evicted) = queue.remove(index) {
                if transcript_observation(&evicted) {
                    self.0.transcript_gap.store(true, Ordering::Release);
                }
                if correlation_receipt(&evicted) {
                    self.0.correlation_gap.store(true, Ordering::Release);
                }
            }
        }
        let replaces_transcript = matches!(event, SessionObservation::PresentationSnapshot(_));
        queue.push_back(event);
        if replaces_transcript {
            self.0.transcript_gap.store(false, Ordering::Release);
        }
        self.0.ready.notify_one();
        Ok(())
    }
}

impl<P: ProtocolExtension> ObservationReceiver<P> {
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
mod tests {
    #[test]
    fn child_live_deltas_coalesce_and_flood_leaves_room_for_root() {
        let (sender, receiver) = observation_channel::<Core>();
        for _ in 0..(128 * crate::limits::COALESCED_DELTA_BYTES / 4096 + 256) {
            let _ = sender.send(SessionObservation::ChildLive {
                agent_id: "c1".into(),
                turn_id: "t1".into(),
                op: Box::new(SessionObservation::AssistantDelta("x".repeat(4096))),
            });
        }
        sender
            .send(SessionObservation::Row(TranscriptRow::assistant("root")))
            .unwrap();
        let events = receiver.try_iter().collect::<Vec<_>>();
        assert!(events.len() <= 130);
        assert_eq!(events.iter().filter(|event| matches!(event, SessionObservation::ChildLive {op, ..} if op.as_ref() == &super::child_gap_notice())).count(), 1);
        let SessionObservation::Row(TranscriptRow::Notice(gap)) = super::child_gap_notice::<Core>()
        else {
            panic!("the gap marker is a notice row");
        };
        assert_eq!(
            gap.placement(),
            crate::views::transcript::NoticePlacement::NoticeRow,
            "the operator must see that child rows were dropped"
        );
        let (sender, receiver) = observation_channel::<Core>();
        for text in ["a", "b"] {
            sender
                .send(SessionObservation::ChildLive {
                    agent_id: "c1".into(),
                    turn_id: "t1".into(),
                    op: Box::new(SessionObservation::AssistantDelta(text.into())),
                })
                .unwrap();
        }
        assert!(
            matches!(receiver.try_recv().unwrap(), SessionObservation::ChildLive {op, ..} if matches!(*op, SessionObservation::AssistantDelta(ref text) if text == "ab"))
        );
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn job_lifecycle_is_not_evicted_at_saturation() {
        let (sender, receiver) = observation_channel::<Core>();
        sender
            .send(SessionObservation::JobTerminal {
                job_id: "p1".into(),
                owner: "root".into(),
                status: "exited".into(),
                exit_code: Some(0),
                stop_reason: None,
                elapsed_ms: 1,
                output: String::new(),
            })
            .unwrap();
        for _ in 1..OBSERVATION_QUEUE_CAPACITY {
            sender
                .send(SessionObservation::Row(TranscriptRow::assistant("fill")))
                .unwrap();
        }
        sender.send(SessionObservation::AssistantDone).unwrap();
        assert!(matches!(
            receiver.try_recv().unwrap(),
            SessionObservation::JobTerminal { .. }
        ));
    }

    use super::{IntentSendError, intent_channel, observation_channel};
    use crate::intent::{Core, SessionIntent};
    use crate::limits::{INTENT_QUEUE_CAPACITY, OBSERVATION_QUEUE_CAPACITY};
    use crate::observation::{
        ContextCapacityRecovery, QueueDrainState, QueueItem, SessionObservation, TurnLifecycle,
        TurnPhase,
    };
    use crate::views::transcript::{NoticeScope, TranscriptRow};
    use std::sync::mpsc;

    #[test]
    fn running_process_count_is_shared_without_enqueuing_an_intent() {
        let (sender, receiver) = intent_channel::<Core>();
        assert_eq!(sender.background_processes(), 0);
        receiver.set_background_processes(2);
        assert_eq!(sender.clone().background_processes(), 2);
        assert!(receiver.try_recv().is_err());
        receiver.set_background_processes(0);
        assert_eq!(sender.background_processes(), 0);
    }

    #[test]
    fn context_capacity_snapshots_supersede_their_own_older_value() {
        let (sender, receiver) = observation_channel::<Core>();
        sender
            .send(SessionObservation::ContextCapacityRecovery(Some(
                ContextCapacityRecovery::Compactable,
            )))
            .unwrap();
        sender
            .send(SessionObservation::ContextCapacityRecovery(None))
            .unwrap();
        assert_eq!(
            receiver.try_iter().collect::<Vec<_>>(),
            vec![SessionObservation::ContextCapacityRecovery(None)]
        );
    }

    #[test]
    fn promptable_is_a_local_signal_and_abandonment_outranks_queued_work() {
        let (sender, receiver) = intent_channel::<Core>();
        assert!(!receiver.is_promptable());
        sender.mark_promptable();
        assert!(receiver.is_promptable());
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        sender.send(SessionIntent::Interrupt).unwrap();
        sender.request_abandon_and_release();
        assert!(receiver.abandon_and_release_requested());
        assert_eq!(*receiver.try_recv().unwrap(), SessionIntent::Quit);
    }

    #[test]
    fn correlation_receipt_loss_resets_before_remaining_observations() {
        for receipt in [
            SessionObservation::<Core>::PromptAdmitted(QueueItem {
                rejected: false,
                id: "q".into(),
                revision: 1,
                text: "same".into(),
            }),
            SessionObservation::PromptStarted(None),
            SessionObservation::ToolInvocationOrigin {
                call_id: "call".into(),
                command_id: "iv".into(),
            },
            SessionObservation::ClearView,
            SessionObservation::DraftRejected(SessionIntent::Prompt("same".into())),
        ] {
            // Refusal evicts a non-state receipt at saturation.
            let (tx, rx) = observation_channel();
            tx.send(receipt.clone()).unwrap();
            for index in 1..OBSERVATION_QUEUE_CAPACITY {
                tx.send(SessionObservation::Row(TranscriptRow::assistant(format!(
                    "line {index}"
                ))))
                .unwrap();
            }
            tx.send(SessionObservation::ModelRefusal {
                detail: Some("refused".into()),
            })
            .unwrap();
            assert!(matches!(
                rx.try_recv().unwrap(),
                SessionObservation::PromptCorrelationReset
            ));
            assert!(!matches!(
                rx.try_recv().unwrap(),
                SessionObservation::PromptCorrelationReset
            ));

            // A mailbox of admission receipts refuses another receipt without
            // growing; the independent reset marker still wakes the receiver.
            let (tx, rx) = observation_channel();
            for _ in 0..OBSERVATION_QUEUE_CAPACITY {
                tx.send(SessionObservation::DraftAccepted(SessionIntent::Prompt(
                    "fill".into(),
                )))
                .unwrap();
            }
            assert!(tx.send(receipt).is_err());
            assert!(matches!(
                rx.recv_timeout(std::time::Duration::ZERO).unwrap(),
                SessionObservation::PromptCorrelationReset
            ));
            assert_eq!(rx.try_iter().count(), OBSERVATION_QUEUE_CAPACITY);
        }
    }

    #[test]
    fn saturated_action_queue_preserves_interrupt_liveness() {
        let (sender, receiver) = intent_channel::<Core>();
        for index in 0..INTENT_QUEUE_CAPACITY {
            sender
                .send(SessionIntent::Prompt(index.to_string()))
                .unwrap();
        }
        sender.send(SessionIntent::Interrupt).unwrap();
        sender.send(SessionIntent::Interrupt).unwrap();
        let actions = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(actions.len(), INTENT_QUEUE_CAPACITY + 1);
        assert_eq!(
            actions
                .iter()
                .filter(|action| **action == SessionIntent::Interrupt)
                .count(),
            1
        );
    }

    #[test]
    fn saturated_action_queue_typed_refusals_preserve_user_actions() {
        let (sender, _receiver) = intent_channel::<Core>();
        for index in 0..INTENT_QUEUE_CAPACITY {
            sender
                .send(SessionIntent::Prompt(index.to_string()))
                .unwrap();
        }
        for action in [
            SessionIntent::Prompt("draft".into()),
            SessionIntent::EditQueued {
                id: "q-1".into(),
                revision: 7,
                text: "edited".into(),
            },
            SessionIntent::ResumeSession {
                id: Some("session-1".into()),
            },
        ] {
            assert_eq!(
                sender.send(action.clone()),
                Err(IntentSendError::Saturated(action))
            );
        }
    }

    #[test]
    fn nonrepeatable_session_mutations_are_refused_until_host_acknowledgement() {
        for action in [
            SessionIntent::ClearSession,
            SessionIntent::Compact,
            SessionIntent::ForkSession {
                account: None,
                compact: false,
            },
        ] {
            let (sender, receiver) = intent_channel::<Core>();
            sender.send(action.clone()).unwrap();
            assert_eq!(
                sender.send(action.clone()),
                Err(IntentSendError::Duplicate(action.clone()))
            );

            let delivery = receiver.try_recv().unwrap();
            let (delivered_action, acknowledgement) = delivery.into_parts();
            assert_eq!(delivered_action, action);
            assert_eq!(
                sender.send(action.clone()),
                Err(IntentSendError::Duplicate(action.clone()))
            );

            assert_eq!(receiver.try_recv(), Err(mpsc::TryRecvError::Empty));
            assert_eq!(
                sender.send(action.clone()),
                Err(IntentSendError::Duplicate(action.clone()))
            );
            drop(acknowledgement);
            sender.send(action.clone()).unwrap();
            assert_eq!(*receiver.try_recv().unwrap(), action);
        }
    }

    #[test]
    fn timeout_and_interrupt_do_not_acknowledge_active_compaction() {
        use std::time::Duration;

        let (sender, receiver) = intent_channel::<Core>();
        sender.send(SessionIntent::Compact).unwrap();
        let (action, acknowledgement) = receiver.try_recv().unwrap().into_parts();
        assert_eq!(action, SessionIntent::Compact);

        assert_eq!(receiver.try_recv(), Err(mpsc::TryRecvError::Empty));
        assert_eq!(
            receiver.recv_timeout(Duration::ZERO),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        sender.send(SessionIntent::Interrupt).unwrap();
        assert_eq!(*receiver.try_recv().unwrap(), SessionIntent::Interrupt);
        assert_eq!(
            sender.send(SessionIntent::Compact),
            Err(IntentSendError::Duplicate(SessionIntent::Compact))
        );

        drop(acknowledgement);
        sender.send(SessionIntent::Compact).unwrap();
    }

    #[test]
    #[allow(
        clippy::result_large_err,
        reason = "the refusal hands the rejected action back verbatim"
    )]
    fn concurrent_senders_admit_one_nonrepeatable_generation() {
        use std::sync::{Arc, Barrier};

        fn send(sender: &super::IntentSender<Core>) -> Result<(), IntentSendError> {
            sender.send(SessionIntent::Compact)
        }
        let (sender, _receiver) = intent_channel::<Core>();
        let other = sender.clone();
        let barrier = Arc::new(Barrier::new(3));
        let first_barrier = Arc::clone(&barrier);
        let first = std::thread::spawn(move || {
            first_barrier.wait();
            send(&sender)
        });
        let second_barrier = Arc::clone(&barrier);
        let second = std::thread::spawn(move || {
            second_barrier.wait();
            send(&other)
        });
        barrier.wait();

        let results = [first.join().unwrap(), second.join().unwrap()];
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(IntentSendError::Duplicate(_))))
                .count(),
            1
        );
    }

    #[test]
    fn stale_acknowledgement_cannot_release_later_generation() {
        let (sender, receiver) = intent_channel::<Core>();
        sender.send(SessionIntent::Compact).unwrap();
        let (_, first_acknowledgement) = receiver.try_recv().unwrap().into_parts();
        let stale = first_acknowledgement.admission.unwrap();
        drop(first_acknowledgement);

        sender.send(SessionIntent::Compact).unwrap();
        let (_, second_acknowledgement) = receiver.try_recv().unwrap().into_parts();
        receiver.0.release_nonrepeatable(stale);
        assert_eq!(
            sender.send(SessionIntent::Compact),
            Err(IntentSendError::Duplicate(SessionIntent::Compact))
        );

        drop(second_acknowledgement);
        sender.send(SessionIntent::Compact).unwrap();
    }

    #[test]
    fn interrupt_preempts_and_quit_follows_the_accepted_ordinary_queue() {
        let (sender, receiver) = intent_channel::<Core>();
        sender.send(SessionIntent::Prompt("draft".into())).unwrap();
        sender.send(SessionIntent::Quit).unwrap();
        sender.send(SessionIntent::Quit).unwrap();
        sender.send(SessionIntent::Interrupt).unwrap();
        assert_eq!(*receiver.try_recv().unwrap(), SessionIntent::Interrupt);
        assert_eq!(
            *receiver.try_recv().unwrap(),
            SessionIntent::Prompt("draft".into())
        );
        assert_eq!(*receiver.try_recv().unwrap(), SessionIntent::Quit);
        assert_eq!(receiver.try_recv(), Err(mpsc::TryRecvError::Empty));
    }

    #[test]
    fn quit_is_ordered_after_already_accepted_ordinary_actions() {
        let (sender, receiver) = intent_channel::<Core>();
        for index in 0..INTENT_QUEUE_CAPACITY - 1 {
            sender
                .send(SessionIntent::Prompt(index.to_string()))
                .unwrap();
        }
        sender.send(SessionIntent::Quit).unwrap();
        let actions = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(actions.len(), INTENT_QUEUE_CAPACITY);
        assert_eq!(
            actions.last().map(std::ops::Deref::deref),
            Some(&SessionIntent::Quit)
        );
    }

    #[test]
    fn accepted_ordinary_action_is_drained_before_sender_disconnect() {
        let (sender, receiver) = intent_channel::<Core>();
        sender
            .send(SessionIntent::ResumeSession {
                id: Some("session-1".into()),
            })
            .unwrap();
        drop(sender);
        assert_eq!(
            *receiver.recv().unwrap(),
            SessionIntent::ResumeSession {
                id: Some("session-1".into())
            }
        );
        assert_eq!(receiver.recv(), Err(mpsc::RecvError));
    }

    #[test]
    fn timed_action_receive_preserves_urgent_timeout_and_disconnect_semantics() {
        use std::time::Duration;

        let (sender, receiver) = intent_channel::<Core>();
        assert_eq!(
            receiver.recv_timeout(Duration::ZERO),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        sender.send(SessionIntent::Prompt("queued".into())).unwrap();
        sender.send(SessionIntent::Interrupt).unwrap();
        assert_eq!(
            *receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
            SessionIntent::Interrupt
        );
        assert_eq!(
            *receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
            SessionIntent::Prompt("queued".into())
        );
        drop(sender);
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(1)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        );
    }

    #[test]
    fn action_sender_refuses_immediately_after_host_receiver_exits() {
        let (sender, receiver) = intent_channel::<Core>();
        drop(receiver);
        let action = SessionIntent::Prompt("preserve me".into());
        assert_eq!(
            sender.send(action.clone()),
            Err(IntentSendError::Disconnected(action))
        );
    }

    #[test]
    fn observation_mailbox_coalesces_deltas_and_preserves_final_state() {
        let (sender, receiver) = observation_channel::<Core>();
        for _ in 0..1_000 {
            let _ = sender.send(SessionObservation::AssistantDelta("x".into()));
        }
        sender.send(SessionObservation::SetBusy(false)).unwrap();
        sender
            .send(SessionObservation::Status {
                phase: "idle".into(),
                queue_len: 3,
                detail: Some("done".into()),
            })
            .unwrap();
        let events = receiver.try_iter().collect::<Vec<_>>();
        assert!(events.len() < 10);
        assert!(events.contains(&SessionObservation::SetBusy(false)));
        assert!(events.iter().any(|event| matches!(
            event,
            SessionObservation::Status { phase, queue_len: 3, .. } if phase == "idle"
        )));
    }

    #[test]
    fn observation_receiver_reports_host_disconnect_after_draining() {
        let (sender, receiver) = observation_channel::<Core>();
        sender.send(SessionObservation::SetBusy(false)).unwrap();
        drop(sender);
        assert_eq!(receiver.try_recv(), Ok(SessionObservation::SetBusy(false)));
        assert_eq!(receiver.try_recv(), Err(mpsc::TryRecvError::Disconnected));
    }

    #[test]
    fn transcript_replacement_clears_saturation_gap() {
        let (sender, receiver) = observation_channel::<Core>();
        for index in 0..OBSERVATION_QUEUE_CAPACITY {
            sender
                .send(SessionObservation::Row(TranscriptRow::error(
                    NoticeScope::Host,
                    format!("diagnostic-{index}"),
                )))
                .unwrap();
        }
        assert_eq!(
            sender.send(SessionObservation::AssistantDelta("dropped".into())),
            Err(SessionObservation::AssistantDelta("dropped".into()))
        );
        assert!(sender.transcript_resnapshot_required());

        let mut authoritative = crate::views::PresentationSnapshot::empty(1);
        authoritative.rows = vec![
            TranscriptRow::user("prompt"),
            TranscriptRow::assistant("complete"),
        ];
        sender
            .send(SessionObservation::PresentationSnapshot(
                authoritative.clone(),
            ))
            .unwrap();
        assert!(!sender.transcript_resnapshot_required());
        assert!(
            receiver
                .try_iter()
                .any(|event| event
                    == SessionObservation::PresentationSnapshot(authoritative.clone()))
        );
    }

    #[test]
    fn typed_presentation_snapshot_clears_saturation_gap() {
        let (sender, _receiver) = observation_channel::<Core>();
        for index in 0..OBSERVATION_QUEUE_CAPACITY {
            assert!(
                sender
                    .send(SessionObservation::Row(TranscriptRow::error(
                        NoticeScope::Host,
                        format!("diagnostic-{index}")
                    )))
                    .is_ok()
            );
        }
        assert!(
            sender
                .send(SessionObservation::AssistantDelta("dropped".into()))
                .is_err()
        );
        assert!(sender.transcript_resnapshot_required());
        assert!(
            sender
                .send(SessionObservation::PresentationSnapshot(
                    crate::views::PresentationSnapshot::empty(1),
                ))
                .is_ok()
        );
        assert!(!sender.transcript_resnapshot_required());
    }

    #[test]
    fn final_state_displaces_saturated_diagnostics() {
        let (sender, receiver) = observation_channel::<Core>();
        for index in 0..OBSERVATION_QUEUE_CAPACITY {
            sender
                .send(SessionObservation::Row(TranscriptRow::error(
                    NoticeScope::Host,
                    format!("diagnostic-{index}"),
                )))
                .unwrap();
        }
        sender
            .send(SessionObservation::QueueSnapshot {
                items: vec![QueueItem {
                    rejected: false,
                    id: "q-final".into(),
                    revision: 1,
                    text: "pending".into(),
                }],
                drain: QueueDrainState::RestoredPaused,
            })
            .unwrap();
        sender.send(SessionObservation::SetBusy(false)).unwrap();
        sender
            .send(SessionObservation::Status {
                phase: "idle".into(),
                queue_len: 1,
                detail: Some("final".into()),
            })
            .unwrap();
        let events = receiver.try_iter().collect::<Vec<_>>();
        assert!(events.contains(&SessionObservation::SetBusy(false)));
        assert!(events.iter().any(|event| matches!(
            event,
            SessionObservation::QueueSnapshot { items, drain: QueueDrainState::RestoredPaused }
                if items.first().is_some_and(|item| item.id == "q-final")
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            SessionObservation::Status { phase, queue_len: 1, detail: Some(detail) }
                if phase == "idle" && detail == "final"
        )));
    }

    #[test]
    fn terminal_error_survives_saturation_and_final_recovery_state() {
        let (sender, receiver) = observation_channel::<Core>();
        for _ in 0..OBSERVATION_QUEUE_CAPACITY {
            sender.send(SessionObservation::AssistantDone).unwrap();
        }
        let terminal = SessionObservation::Row(TranscriptRow::error(
            NoticeScope::Host,
            "provider terminal failure",
        ));
        sender.send(terminal.clone()).unwrap();
        sender
            .send(SessionObservation::QueueSnapshot {
                items: vec![QueueItem {
                    rejected: false,
                    id: "q-recovery".into(),
                    revision: 1,
                    text: "pending".into(),
                }],
                drain: QueueDrainState::RestoredPaused,
            })
            .unwrap();
        sender.send(SessionObservation::SetBusy(false)).unwrap();
        sender
            .send(SessionObservation::Status {
                phase: "error".into(),
                queue_len: 1,
                detail: Some("recovery required".into()),
            })
            .unwrap();

        let events = receiver.try_iter().collect::<Vec<_>>();
        assert!(events.contains(&terminal));
        assert!(events.contains(&SessionObservation::SetBusy(false)));
        assert!(events.iter().any(|event| matches!(
            event,
            SessionObservation::QueueSnapshot {
                drain: QueueDrainState::RestoredPaused,
                ..
            }
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            SessionObservation::Status { phase, detail: Some(detail), .. }
                if phase == "error" && detail == "recovery required"
        )));
    }

    #[test]
    fn model_refusal_survives_saturation_and_final_idle_state() {
        let (sender, receiver) = observation_channel::<Core>();
        for _ in 0..OBSERVATION_QUEUE_CAPACITY {
            sender.send(SessionObservation::AssistantDone).unwrap();
        }
        sender
            .send(SessionObservation::ModelRefusal { detail: None })
            .unwrap();
        assert!(
            sender
                .send(SessionObservation::Row(TranscriptRow::info(
                    NoticeScope::Host,
                    "later note"
                )))
                .is_err()
        );
        sender
            .send(SessionObservation::Row(TranscriptRow::error(
                NoticeScope::Host,
                "later diagnostic",
            )))
            .unwrap();
        let _ = sender.send(SessionObservation::AssistantDone);
        sender
            .send(SessionObservation::DraftRejected(SessionIntent::Prompt(
                "draft retained".into(),
            )))
            .unwrap();
        sender.send(SessionObservation::SetBusy(false)).unwrap();
        sender
            .send(SessionObservation::Status {
                phase: "idle".into(),
                queue_len: 0,
                detail: None,
            })
            .unwrap();
        let events = receiver.try_iter().collect::<Vec<_>>();
        assert!(events.contains(&SessionObservation::ModelRefusal { detail: None }));
        let lines = events
            .iter()
            .filter_map(|event| {
                crate::jsonl::project_host_event(
                    event,
                    crate::envelope::JsonlCorrelation::default(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.contains("\"kind\":\"model_refusal\""))
                .count(),
            1
        );

        assert!(events.contains(&SessionObservation::SetBusy(false)));
    }

    #[test]
    fn turn_lifecycle_survives_replaceable_saturation() {
        let (sender, receiver) = observation_channel::<Core>();
        for _ in 0..OBSERVATION_QUEUE_CAPACITY {
            sender.send(SessionObservation::AssistantDone).unwrap();
        }
        let lifecycle = SessionObservation::TurnLifecycle(TurnLifecycle {
            turn_id: "00000000-0000-4000-8000-0000000000cc".into(),
            phase: TurnPhase::Completed,
            model_error_kind: None,
            http_status: None,
        });
        sender.send(lifecycle.clone()).unwrap();
        // Replaceable transcript/delta/progress traffic cannot displace the
        // retained terminal marker, and neither can a later final state.
        assert!(sender.send(SessionObservation::AssistantDone).is_err());
        assert!(
            sender
                .send(SessionObservation::AssistantDelta("more".into()))
                .is_err()
        );
        sender.send(SessionObservation::SetBusy(false)).unwrap();
        sender
            .send(SessionObservation::Status {
                phase: "idle".into(),
                queue_len: 0,
                detail: None,
            })
            .unwrap();
        let events = receiver.try_iter().collect::<Vec<_>>();
        assert!(events.contains(&lifecycle));
        let lines = events
            .iter()
            .filter_map(|event| {
                crate::jsonl::project_host_event(
                    event,
                    crate::envelope::JsonlCorrelation::default(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.contains("\"kind\":\"turn_lifecycle\""))
                .count(),
            1
        );
    }

    #[test]
    fn terminal_usage_observation_remains_bounded_at_saturation() {
        let (sender, receiver) = observation_channel::<Core>();
        for index in 0..OBSERVATION_QUEUE_CAPACITY {
            sender
                .send(SessionObservation::Row(TranscriptRow::info(
                    NoticeScope::Host,
                    format!("diagnostic-{index}"),
                )))
                .unwrap();
        }
        let usage = SessionObservation::TurnUsage {
            input_tokens: 10,
            output_tokens: 4,
            reasoning_tokens: 2,
            cached_input_tokens: 3,
            cache_write_tokens: 0,
            model_operations: Some(2),
            reporting_model_operations: Some(1),
            peak_prepared_input_tokens: None,
            terminal: true,
        };
        sender.send(usage.clone()).unwrap();
        let events = receiver.try_iter().collect::<Vec<_>>();
        assert_eq!(events.len(), OBSERVATION_QUEUE_CAPACITY);
        assert!(events.contains(&usage));
    }
}
