//! Bounded cursors over the parent hub; no independent queue or task.

use super::super::projection::AgentViewSnapshot;
use super::super::types::{
    AgentEvent, AgentEventKind, EventCursor, EventDelivery, EventDeliveryClass, EventLossKind,
    EventLostRange, EventScope, EventSequence, ViewRequest,
};
use super::{EventHubInner, EventHubState, lock};
use crate::AgentError;
use plakous_model::TurnId;
use std::fmt;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::watch;

/// Terminal state of a nonblocking event subscription.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum SubscriptionEnd {
    /// Retention or subscriber debt limits overtook this consumer.
    #[error("event subscriber lagged behind bounded retention")]
    SubscriberLagged {
        /// Last sequence incorporated by the consumer or its atomic snapshot.
        last_delivered: EventCursor,
        /// Earliest sequence which may still be available for recovery.
        next_available: EventSequence,
    },
    /// The owning agent event source closed.
    #[error("agent event source is closed")]
    AgentClosed {
        /// Last sequence incorporated by the consumer or its atomic snapshot.
        last_delivered: EventCursor,
    },
}

/// Typed attachment/resume failure.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum ViewError {
    /// Cursor scope, schema, projection, or visibility differs.
    #[error("event cursor is incompatible with the requested view")]
    IncompatibleCursor,
    /// Cursor points beyond the current event head.
    #[error("event cursor is ahead of the current event head")]
    CursorAhead,
    /// The bounded subscriber registry is full.
    #[error("agent event subscriber limit reached")]
    SubscriberLimit,
    /// Event identity or sequence allocation failed.
    #[error("agent event identity or sequence allocation failed")]
    Identity,
    /// The event source no longer accepts publication.
    #[error("agent event source is closed")]
    Closed,
    /// The event kind and its authoritative projection payload disagree.
    #[error("event payload is inconsistent with its event kind")]
    InvalidEvent,
    /// A monotonic lifetime fence cannot safely accept another identity.
    #[error("event lifetime fence requires a new session")]
    FenceSaturated,
    /// The live projection cannot safely continue from the supplied fence state.
    #[error("event lifetime fence is unavailable; attach a fresh observation epoch")]
    FenceStateUnavailable,
}

/// An atomic snapshot and subscription whose first event is strictly after its cursor.
pub struct ViewAttachment {
    pub(super) snapshot: AgentViewSnapshot,
    pub(super) events: EventSubscription,
}

impl ViewAttachment {
    /// Returns the atomic snapshot.
    #[must_use]
    pub const fn snapshot(&self) -> &AgentViewSnapshot {
        &self.snapshot
    }

    /// Splits the attachment into independent presentation values.
    #[must_use]
    pub fn into_parts(self) -> (AgentViewSnapshot, EventSubscription) {
        (self.snapshot, self.events)
    }
}

impl fmt::Debug for ViewAttachment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ViewAttachment")
            .field("snapshot", &self.snapshot)
            .field("events", &self.events)
            .finish()
    }
}

/// Result of reconnecting after a previously observed cursor.
#[non_exhaustive]
pub enum ResumeOutcome {
    /// The requested retained tail is available.
    Replaying(EventSubscription),
    /// The range was evicted; use this fresh atomic snapshot/subscription pair.
    RangeUnavailable(ViewAttachment),
    /// The range is retained but exceeds the requested replay debt limits; use
    /// this fresh atomic snapshot/subscription pair.
    ReplayLimitExceeded(ViewAttachment),
}

impl fmt::Debug for ResumeOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Replaying(events) => formatter.debug_tuple("Replaying").field(events).finish(),
            Self::RangeUnavailable(attachment) => formatter
                .debug_tuple("RangeUnavailable")
                .field(attachment)
                .finish(),
            Self::ReplayLimitExceeded(attachment) => formatter
                .debug_tuple("ReplayLimitExceeded")
                .field(attachment)
                .finish(),
        }
    }
}

/// Independent bounded cursor over shared retained events.
pub struct EventSubscription {
    pub(super) inner: Arc<EventHubInner>,
    pub(super) changed: watch::Receiver<u64>,
    pub(super) next_sequence: u64,
    pub(super) last_delivered: EventCursor,
    pub(super) request: ViewRequest,
    pub(super) turn_filter: Option<TurnId>,
    pub(super) terminal: Option<SubscriptionEnd>,
    pub(super) registered: bool,
    pub(super) subscribed: bool,
}

impl EventSubscription {
    /// Waits for the next matching event without ever blocking its producer.
    ///
    /// # Errors
    ///
    /// Returns an out-of-band lag range or closed status. After termination no
    /// later event, including a turn terminal, is claimed as delivered.
    pub async fn recv(&mut self) -> Result<Arc<AgentEvent>, SubscriptionEnd> {
        loop {
            match self.receive(false).await? {
                EventDelivery::Event(event) => return Ok(event),
                EventDelivery::Lost(_) => {}
            }
        }
    }

    /// Waits for the next event or an exact droppable range omitted under this
    /// subscriber's item/byte pressure.
    ///
    /// Structural saturation still returns [`SubscriptionEnd::SubscriberLagged`].
    ///
    /// # Errors
    ///
    /// Returns an out-of-band structural lag range or closed status.
    pub async fn recv_delivery(&mut self) -> Result<EventDelivery, SubscriptionEnd> {
        self.receive(true).await
    }

    async fn receive(&mut self, delivery_aware: bool) -> Result<EventDelivery, SubscriptionEnd> {
        loop {
            if let Some(terminal) = self.terminal {
                return Err(terminal);
            }
            let decision = {
                let state = lock(&self.inner.state);
                self.read_locked(&state, delivery_aware)
            };
            match decision {
                SubscriptionDecision::Event(event) => {
                    self.next_sequence = event.branch_sequence.0.saturating_add(1);
                    let scope = self.turn_filter.map_or(EventScope::Agent, EventScope::Turn);
                    let projected = event.projected(self.request.visibility, scope);
                    self.last_delivered = projected.cursor();
                    if matches!(projected.kind(), AgentEventKind::AgentClosed) {
                        self.release_subscription();
                    }
                    return Ok(EventDelivery::Event(projected));
                }
                SubscriptionDecision::Lost(lost) => {
                    self.next_sequence = lost.through.0.saturating_add(1);
                    self.last_delivered = lost.cursor();
                    return Ok(EventDelivery::Lost(lost));
                }
                SubscriptionDecision::Advanced(next) => {
                    self.next_sequence = next;
                }
                SubscriptionDecision::Terminal(terminal) => {
                    self.terminal = Some(terminal);
                    self.release_subscription();
                    return Err(terminal);
                }
                SubscriptionDecision::Wait => {}
            }
            if self.changed.changed().await.is_err() {
                let terminal = SubscriptionEnd::AgentClosed {
                    last_delivered: self.last_delivered,
                };
                self.terminal = Some(terminal);
                self.release_subscription();
                return Err(terminal);
            }
        }
    }

    /// Returns the last incorporated cursor, including the initial snapshot boundary.
    #[must_use]
    pub const fn last_delivered(&self) -> EventCursor {
        self.last_delivered
    }

    fn release_subscription(&mut self) {
        if self.subscribed {
            let mut state = lock(&self.inner.state);
            if self.registered {
                state.subscribers = state.subscribers.saturating_sub(1);
            }
            if let Some(turn_id) = self.turn_filter {
                let remove = if let Some(count) = state.turn_subscribers.get_mut(&turn_id) {
                    *count = count.saturating_sub(1);
                    *count == 0
                } else {
                    false
                };
                if remove {
                    state.turn_subscribers.remove(&turn_id);
                    if !state.view.turns.contains_key(&turn_id) {
                        state.turn_evicted_through.remove(&turn_id);
                    }
                }
            }
            self.registered = false;
            self.subscribed = false;
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "retention, debt, typed loss, and terminal decisions share one locked snapshot"
    )]
    fn read_locked(&self, state: &EventHubState, delivery_aware: bool) -> SubscriptionDecision {
        if let Some(first) = state.retained.front()
            && first.branch_sequence.0 > self.next_sequence
        {
            let matching_was_evicted = self.turn_filter.is_none_or(|turn_id| {
                state
                    .turn_evicted_through
                    .get(&turn_id)
                    .is_some_and(|sequence| *sequence >= self.next_sequence)
            });
            if matching_was_evicted {
                return SubscriptionDecision::Terminal(SubscriptionEnd::SubscriberLagged {
                    last_delivered: self.last_delivered,
                    next_available: first.branch_sequence,
                });
            }
        }
        if state.retained.is_empty() && self.next_sequence <= state.branch_sequence {
            let matching_was_evicted = self.turn_filter.is_none_or(|turn_id| {
                state
                    .turn_evicted_through
                    .get(&turn_id)
                    .is_some_and(|sequence| *sequence >= self.next_sequence)
            });
            if matching_was_evicted {
                return SubscriptionDecision::Terminal(SubscriptionEnd::SubscriberLagged {
                    last_delivered: self.last_delivered,
                    next_available: EventSequence(state.branch_sequence.saturating_add(1)),
                });
            }
        }

        let mut matching_items = 0_usize;
        let mut matching_bytes = 0_usize;
        let mut first_matching = None;
        for event in state
            .retained
            .iter()
            .filter(|event| event.branch_sequence.0 >= self.next_sequence)
        {
            if self
                .turn_filter
                .is_none_or(|turn| event.turn_id == Some(turn))
            {
                matching_items = matching_items.saturating_add(1);
                matching_bytes = matching_bytes
                    .saturating_add(event.projected_retained_bytes(self.request.visibility));
                first_matching.get_or_insert_with(|| Arc::clone(event));
            }
        }
        if matching_items > self.request.limits.max_items()
            || matching_bytes > self.request.limits.max_bytes()
        {
            if delivery_aware && self.turn_filter.is_none() {
                let mut structural_items = 0_usize;
                let mut structural_bytes = 0_usize;
                for event in state
                    .retained
                    .iter()
                    .filter(|event| event.branch_sequence.0 >= self.next_sequence)
                    .filter(|event| event.delivery_class() == EventDeliveryClass::Structural)
                {
                    structural_items = structural_items.saturating_add(1);
                    structural_bytes = structural_bytes
                        .saturating_add(event.projected_retained_bytes(self.request.visibility));
                }
                if structural_items <= self.request.limits.max_items()
                    && structural_bytes <= self.request.limits.max_bytes()
                    && let Some(first) = &first_matching
                    && first.delivery_class() != EventDeliveryClass::Structural
                {
                    let mut through = first.branch_sequence;
                    let mut kind = loss_kind(first.delivery_class());
                    for event in state.retained.iter().filter(|event| {
                        event.branch_sequence.0 > first.branch_sequence.0
                            && event.branch_sequence.0 >= self.next_sequence
                    }) {
                        let class = event.delivery_class();
                        if class == EventDeliveryClass::Structural {
                            break;
                        }
                        through = event.branch_sequence;
                        kind = merge_loss_kind(kind, loss_kind(class));
                    }
                    return SubscriptionDecision::Lost(EventLostRange {
                        after: self.last_delivered,
                        through,
                        kind,
                    });
                }
                if structural_items <= self.request.limits.max_items()
                    && structural_bytes <= self.request.limits.max_bytes()
                    && let Some(event) = first_matching
                {
                    return SubscriptionDecision::Event(event);
                }
            }
            return SubscriptionDecision::Terminal(SubscriptionEnd::SubscriberLagged {
                last_delivered: self.last_delivered,
                next_available: EventSequence(self.next_sequence),
            });
        }
        if let Some(event) = first_matching {
            return SubscriptionDecision::Event(event);
        }
        let next = state.branch_sequence.saturating_add(1);
        if state.closed {
            return SubscriptionDecision::Terminal(SubscriptionEnd::AgentClosed {
                last_delivered: self.last_delivered,
            });
        }
        if next > self.next_sequence {
            SubscriptionDecision::Advanced(next)
        } else {
            SubscriptionDecision::Wait
        }
    }
}

impl fmt::Debug for EventSubscription {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EventSubscription")
            .field("next_sequence", &self.next_sequence)
            .field("last_delivered", &self.last_delivered)
            .field("visibility", &self.request.visibility)
            .field("turn_filter", &self.turn_filter)
            .field("terminal", &self.terminal)
            .field("registered", &self.registered)
            .field("subscribed", &self.subscribed)
            .finish_non_exhaustive()
    }
}

impl Drop for EventSubscription {
    fn drop(&mut self) {
        self.release_subscription();
    }
}

enum SubscriptionDecision {
    Event(Arc<AgentEvent>),
    Lost(EventLostRange),
    Advanced(u64),
    Terminal(SubscriptionEnd),
    Wait,
}

fn loss_kind(class: EventDeliveryClass) -> EventLossKind {
    match class {
        EventDeliveryClass::ModelDelta => EventLossKind::ModelDelta,
        EventDeliveryClass::ToolProgress => EventLossKind::ToolProgress,
        EventDeliveryClass::Structural => {
            unreachable!("structural events cannot enter a droppable loss range")
        }
    }
}

const fn merge_loss_kind(left: EventLossKind, right: EventLossKind) -> EventLossKind {
    match (left, right) {
        (EventLossKind::ModelDelta, EventLossKind::ModelDelta) => EventLossKind::ModelDelta,
        (EventLossKind::ToolProgress, EventLossKind::ToolProgress) => EventLossKind::ToolProgress,
        (EventLossKind::ModelDelta, EventLossKind::ToolProgress)
        | (EventLossKind::ToolProgress, EventLossKind::ModelDelta)
        | (EventLossKind::Mixed, _)
        | (_, EventLossKind::Mixed) => EventLossKind::Mixed,
    }
}

/// Event-subscription terminal status retained for turn-oriented compatibility.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum TurnEventError {
    /// The subscriber fell behind a bounded event range.
    #[error("turn event subscriber lagged behind bounded retention")]
    Lagged {
        /// Last cursor incorporated by the subscriber.
        last_delivered: EventCursor,
        /// Earliest sequence which may still be available.
        next_available: EventSequence,
    },
    /// The optional turn-event projection failed independently of turn completion.
    #[error("turn event observation failed: {error}")]
    TerminalFailure {
        /// Redaction-safe observation failure; it carries no terminal authority.
        #[source]
        error: AgentError,
    },
    /// The turn or owning agent event source closed.
    #[error("turn event source is closed")]
    Closed,
}

/// Turn-filtered observation wrapper over the causal subscription contract.
#[derive(Debug)]
pub struct TurnEvents {
    events: EventSubscription,
    terminal_failure: Option<watch::Receiver<Option<AgentError>>>,
    terminal_seen: bool,
}

impl TurnEvents {
    pub(crate) const fn new(events: EventSubscription) -> Self {
        Self {
            events,
            terminal_failure: None,
            terminal_seen: false,
        }
    }

    pub(crate) fn with_terminal_failure(
        mut self,
        terminal_failure: watch::Receiver<Option<AgentError>>,
    ) -> Self {
        self.terminal_failure = Some(terminal_failure);
        self
    }

    pub(crate) fn closed(mut self) -> Self {
        self.events.release_subscription();
        self.terminal_seen = true;
        self
    }

    /// Waits for the next turn event.
    ///
    /// # Errors
    ///
    /// Returns explicit lag, terminal failure, or closed status without
    /// affecting the turn result.
    pub async fn recv(&mut self) -> Result<Arc<AgentEvent>, TurnEventError> {
        loop {
            if self.terminal_seen {
                return Err(TurnEventError::Closed);
            }
            let next = if let Some(terminal_failure) = &mut self.terminal_failure {
                tokio::select! {
                    biased;
                    event = self.events.recv() => TurnEventDelivery::Event(event),
                    error = wait_for_terminal_failure(terminal_failure) => {
                        TurnEventDelivery::TerminalFailure(error)
                    }
                }
            } else {
                TurnEventDelivery::Event(self.events.recv().await)
            };
            match next {
                TurnEventDelivery::Event(Ok(event)) => {
                    self.terminal_seen = matches!(
                        event.kind(),
                        AgentEventKind::TurnCompleted { .. } | AgentEventKind::TurnFailed { .. }
                    );
                    if self.terminal_seen {
                        self.events.release_subscription();
                    }
                    return Ok(event);
                }
                TurnEventDelivery::Event(Err(SubscriptionEnd::SubscriberLagged {
                    last_delivered,
                    next_available,
                })) => {
                    return Err(TurnEventError::Lagged {
                        last_delivered,
                        next_available,
                    });
                }
                TurnEventDelivery::Event(Err(SubscriptionEnd::AgentClosed { .. })) => {
                    if let Some(error) = self
                        .terminal_failure
                        .as_ref()
                        .and_then(|failure| failure.borrow().clone())
                    {
                        self.terminal_seen = true;
                        self.events.release_subscription();
                        return Err(TurnEventError::TerminalFailure { error });
                    }
                    return Err(TurnEventError::Closed);
                }
                TurnEventDelivery::TerminalFailure(Some(error)) => {
                    self.terminal_seen = true;
                    self.events.release_subscription();
                    return Err(TurnEventError::TerminalFailure { error });
                }
                TurnEventDelivery::TerminalFailure(None) => {
                    self.terminal_failure = None;
                }
            }
        }
    }

    /// Returns the last incorporated cursor.
    #[must_use]
    pub const fn last_delivered(&self) -> EventCursor {
        self.events.last_delivered()
    }
}

enum TurnEventDelivery {
    Event(Result<Arc<AgentEvent>, SubscriptionEnd>),
    TerminalFailure(Option<AgentError>),
}

async fn wait_for_terminal_failure(
    terminal_failure: &mut watch::Receiver<Option<AgentError>>,
) -> Option<AgentError> {
    loop {
        if let Some(error) = terminal_failure.borrow().clone() {
            return Some(error);
        }
        if terminal_failure.changed().await.is_err() {
            return None;
        }
    }
}
