//! Atomic retention, attachment, and causal publication ownership.

mod pipeline;
pub(crate) use pipeline::EventPipeline;
pub(super) mod subscription;

use self::subscription::{EventSubscription, ResumeOutcome, TurnEvents, ViewAttachment, ViewError};
use super::projection::{
    AgentViewSnapshot, AgentViewState, ConversationItemView, ConversationProjection, LifetimeFence,
    LifetimeFenceDomain, ProjectionState, TurnViewState, apply_projection_delta,
    preflight_projection,
};
use super::types::{
    AgentEvent, AgentEventKind, EVENT_KIND_VERSION, EVENT_SCHEMA_VERSION, EffectDecisionResolution,
    EventCursor, EventScope, EventSequence, EventVisibility, PROJECTION_REVISION,
    SubscriptionLimits, ViewRequest, event_metadata,
};
use crate::UsageTotals;
use crate::feed::FeedSeq;
use plakous_model::{
    AgentId, AttemptId, BranchId, CallId, Clock, ConversationView, EventId, IdGenerator, RunId,
    SessionId, Timestamp, TurnId,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

#[allow(
    clippy::struct_field_names,
    reason = "the identity suffix distinguishes each causal scope explicitly"
)]
#[derive(Clone, Copy)]
pub(crate) struct EventContext {
    turn_id: Option<TurnId>,
    attempt_id: Option<AttemptId>,
    call_id: Option<CallId>,
    feed_seq: Option<FeedSeq>,
}

impl EventContext {
    pub(crate) const fn turn(turn_id: TurnId) -> Self {
        Self {
            turn_id: Some(turn_id),
            attempt_id: None,
            call_id: None,
            feed_seq: None,
        }
    }

    pub(crate) const fn agent() -> Self {
        Self {
            turn_id: None,
            attempt_id: None,
            call_id: None,
            feed_seq: None,
        }
    }

    pub(crate) const fn attempt(turn_id: TurnId, attempt_id: AttemptId) -> Self {
        Self {
            turn_id: Some(turn_id),
            attempt_id: Some(attempt_id),
            call_id: None,
            feed_seq: None,
        }
    }

    pub(crate) const fn call(turn_id: TurnId, attempt_id: AttemptId, call_id: CallId) -> Self {
        Self {
            turn_id: Some(turn_id),
            attempt_id: Some(attempt_id),
            call_id: Some(call_id),
            feed_seq: None,
        }
    }

    /// Names the feed record of the commit this event reports.
    pub(crate) const fn with_feed_seq(mut self, feed_seq: Option<FeedSeq>) -> Self {
        self.feed_seq = feed_seq;
        self
    }
}

#[derive(Clone, Copy)]
pub(crate) struct EventHubLimits {
    retained_items: usize,
    retained_bytes: usize,
    max_subscribers: usize,
}

impl EventHubLimits {
    pub(crate) const fn new(
        retained_items: usize,
        retained_bytes: usize,
        max_subscribers: usize,
    ) -> Self {
        Self {
            retained_items,
            retained_bytes,
            max_subscribers,
        }
    }
}

#[derive(Clone)]
pub(crate) struct EventHub {
    inner: Arc<EventHubInner>,
}

struct EventHubInner {
    agent_id: AgentId,
    session_id: SessionId,
    branch_id: BranchId,
    run_id: RunId,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGenerator>,
    close_event_id: EventId,
    limits: EventHubLimits,
    state: Mutex<EventHubState>,
    changed: watch::Sender<u64>,
}

struct EventHubState {
    branch_sequence: u64,
    last_timestamp: Timestamp,
    retained: VecDeque<Arc<AgentEvent>>,
    retained_bytes: usize,
    subscribers: usize,
    closed: bool,
    view: ProjectionState,
    turn_parents: BTreeMap<TurnId, EventId>,
    attempt_parents: BTreeMap<(TurnId, AttemptId), EventId>,
    call_parents: BTreeMap<(TurnId, AttemptId, CallId), EventId>,
    completed_turn_parents: BTreeMap<TurnId, EventId>,
    turn_evicted_through: BTreeMap<TurnId, u64>,
    turn_subscribers: BTreeMap<TurnId, usize>,
    completed_turns: VecDeque<TurnId>,
    last_event: Option<EventId>,
}

/// An event whose identity, timestamp and size accounting were fixed outside the hub lock.
struct PreparedAgentEvent {
    event_id: EventId,
    timestamp: Timestamp,
    context: EventContext,
    kind: AgentEventKind,
    retained_bytes: usize,
    metadata_retained_bytes: usize,
    content_retained_bytes: usize,
}

impl EventHub {
    #[allow(
        clippy::too_many_arguments,
        reason = "all event scope and resource bindings are explicit"
    )]
    pub(crate) fn new(
        agent_id: AgentId,
        session_id: SessionId,
        branch_id: BranchId,
        run_id: RunId,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
        close_event_id: EventId,
        initial_conversation: &ConversationView,
        limits: EventHubLimits,
    ) -> Result<Self, ViewError> {
        if limits.retained_items == 0 || limits.retained_bytes == 0 || limits.max_subscribers == 0 {
            return Err(ViewError::Identity);
        }
        let epoch = Timestamp::new(0, 0).map_err(|_| ViewError::Identity)?;
        let (changed, _) = watch::channel(0);
        let initial_items = initial_conversation.items();
        let retained_start = initial_items.len().saturating_sub(limits.retained_items);
        let conversation = ConversationProjection::from_flat_bounded(
            Arc::from(
                initial_items
                    .iter()
                    .skip(retained_start)
                    .map(ConversationItemView::from_item)
                    .collect::<Vec<_>>(),
            ),
            limits.retained_items,
            limits.retained_bytes,
        );
        Ok(Self {
            inner: Arc::new(EventHubInner {
                agent_id,
                session_id,
                branch_id,
                run_id,
                clock,
                ids,
                close_event_id,
                limits,
                state: Mutex::new(EventHubState {
                    branch_sequence: 0,
                    last_timestamp: epoch,
                    retained: VecDeque::new(),
                    retained_bytes: 0,
                    subscribers: 0,
                    closed: false,
                    view: ProjectionState {
                        lifecycle: AgentViewState::Running,
                        turns: BTreeMap::new(),
                        terminal_turn_tombstones: VecDeque::new(),
                        terminal_turn_fence: LifetimeFence::new(LifetimeFenceDomain::TerminalTurn),
                        recovery_promotable_turn: None,
                        active_attempts: BTreeSet::new(),
                        committed_conversation_items: initial_items.len(),
                        conversation,
                        retained_conversation_items: limits.retained_items,
                        retained_conversation_bytes: limits.retained_bytes,
                        pending_effect_decisions: Vec::new(),
                        tool_activity: Vec::new(),
                        tool_activity_tombstones: VecDeque::new(),
                        retired_tool_fence: LifetimeFence::new(
                            LifetimeFenceDomain::RetiredToolCall,
                        ),
                        usage: UsageTotals::default(),
                    },
                    turn_parents: BTreeMap::new(),
                    attempt_parents: BTreeMap::new(),
                    call_parents: BTreeMap::new(),
                    completed_turn_parents: BTreeMap::new(),
                    turn_evicted_through: BTreeMap::new(),
                    turn_subscribers: BTreeMap::new(),
                    completed_turns: VecDeque::new(),
                    last_event: None,
                }),
                changed,
            }),
        })
    }

    #[cfg(test)]
    pub(crate) fn publish(
        &self,
        context: EventContext,
        kind: AgentEventKind,
    ) -> Result<Arc<AgentEvent>, ViewError> {
        self.publish_with(context, |_| kind)
    }

    fn reserve_event_id(&self) -> Result<EventId, ViewError> {
        EventId::generate(self.inner.clock.as_ref(), self.inner.ids.as_ref())
            .map_err(|_| ViewError::Identity)
    }

    fn prepare_reserved(
        &self,
        context: EventContext,
        event_id: EventId,
        kind: AgentEventKind,
    ) -> Result<PreparedAgentEvent, ViewError> {
        self.prepare_reserved_with(context, event_id, kind)
    }

    #[cfg(test)]
    pub(crate) fn publish_with(
        &self,
        context: EventContext,
        create: impl FnOnce(EventId) -> AgentEventKind,
    ) -> Result<Arc<AgentEvent>, ViewError> {
        self.publish_with_state(context, create, false)
    }

    #[cfg(test)]
    fn publish_with_state(
        &self,
        context: EventContext,
        create: impl FnOnce(EventId) -> AgentEventKind,
        closes: bool,
    ) -> Result<Arc<AgentEvent>, ViewError> {
        let event_id = self.reserve_event_id()?;
        let prepared = self.prepare_reserved_with(context, event_id, create(event_id))?;
        self.publish_prepared(prepared, closes)
    }

    fn prepare_reserved_with(
        &self,
        context: EventContext,
        event_id: EventId,
        kind: AgentEventKind,
    ) -> Result<PreparedAgentEvent, ViewError> {
        if let AgentEventKind::ConversationCommitted { item_count, items } = &kind
            && items
                .as_deref()
                .is_some_and(|views| *item_count != views.len())
        {
            return Err(ViewError::InvalidEvent);
        }
        let fallback_timestamp = {
            let state = lock(&self.inner.state);
            if state.closed {
                return Err(ViewError::Closed);
            }
            state
                .branch_sequence
                .checked_add(1)
                .ok_or(ViewError::Identity)?;
            state.last_timestamp
        };
        let timestamp = std::panic::catch_unwind(AssertUnwindSafe(|| self.inner.clock.now()))
            .unwrap_or(fallback_timestamp);
        let retained_bytes = kind.retained_bytes();
        let metadata_retained_bytes = kind.projected(EventVisibility::Metadata).retained_bytes();
        let content_retained_bytes = kind.projected(EventVisibility::Content).retained_bytes();
        Ok(PreparedAgentEvent {
            event_id,
            timestamp,
            context,
            kind,
            retained_bytes,
            metadata_retained_bytes,
            content_retained_bytes,
        })
    }

    /// Validates, sequences, applies and retains one prepared event under a single hold of
    /// the hub state lock, so no other publication can interleave between its checks and its
    /// effects.
    fn publish_prepared(
        &self,
        prepared: PreparedAgentEvent,
        closes: bool,
    ) -> Result<Arc<AgentEvent>, ViewError> {
        let mut state = lock(&self.inner.state);
        if state.closed {
            return Err(ViewError::Closed);
        }
        let branch_sequence = state
            .branch_sequence
            .checked_add(1)
            .ok_or(ViewError::Identity)?;
        let context = prepared.context;
        let causal_parent = causal_parent(&state, context, &prepared.kind);
        let (origin, classification) = event_metadata(&prepared.kind);
        let event = Arc::new(AgentEvent {
            branch_sequence: EventSequence(branch_sequence),
            event_id: prepared.event_id,
            session_id: self.inner.session_id,
            branch_id: self.inner.branch_id,
            agent_id: self.inner.agent_id,
            run_id: self.inner.run_id,
            turn_id: context.turn_id,
            attempt_id: context.attempt_id,
            call_id: context.call_id,
            feed_seq: context.feed_seq,
            timestamp: prepared.timestamp,
            causal_parent,
            origin,
            classification,
            event_schema: EVENT_SCHEMA_VERSION,
            projection_revision: PROJECTION_REVISION,
            visibility: EventVisibility::Content,
            scope: EventScope::Agent,
            event_kind_version: EVENT_KIND_VERSION,
            kind: prepared.kind,
            retained_bytes: prepared.retained_bytes,
            metadata_retained_bytes: prepared.metadata_retained_bytes,
            content_retained_bytes: prepared.content_retained_bytes,
        });
        validate_causal_context(&state, &event).map_err(|()| ViewError::InvalidEvent)?;
        let delta = preflight_projection(&state.view, &event, self.inner.limits.retained_items)?;
        apply_projection_delta(&mut state.view, delta);
        state.branch_sequence = branch_sequence;
        state.last_timestamp = event.timestamp;
        update_causal_parents(&mut state, context, &event.kind, event.event_id);
        state.last_event = Some(event.event_id);
        if matches!(
            event.kind,
            AgentEventKind::TurnCompleted { .. } | AgentEventKind::TurnFailed { .. }
        ) && let Some(turn_id) = context.turn_id
        {
            complete_turn_indexes(&mut state, turn_id, self.inner.limits.retained_items);
        }
        if matches!(event.kind, AgentEventKind::AgentClosed) {
            state.turn_parents.clear();
            state.attempt_parents.clear();
            state.call_parents.clear();
            state.completed_turn_parents.clear();
        }
        state.retained_bytes = state.retained_bytes.saturating_add(event.retained_bytes());
        state.retained.push_back(Arc::clone(&event));
        while state.retained.len() > self.inner.limits.retained_items
            || state.retained_bytes > self.inner.limits.retained_bytes
        {
            let Some(evicted) = state.retained.pop_front() else {
                break;
            };
            if let Some(turn_id) = evicted.turn_id
                && (state.view.turns.contains_key(&turn_id)
                    || state.turn_subscribers.contains_key(&turn_id))
            {
                state
                    .turn_evicted_through
                    .entry(turn_id)
                    .and_modify(|sequence| *sequence = (*sequence).max(evicted.branch_sequence.0))
                    .or_insert(evicted.branch_sequence.0);
            }
            state.retained_bytes = state
                .retained_bytes
                .saturating_sub(evicted.retained_bytes());
        }
        if closes || matches!(event.kind, AgentEventKind::AgentClosed) {
            state.closed = true;
        }
        drop(state);
        self.inner.changed.send_replace(branch_sequence);
        Ok(event)
    }

    pub(crate) fn attach(&self, request: ViewRequest) -> Result<ViewAttachment, ViewError> {
        self.attach_filtered(request, None)
    }

    pub(crate) fn turn_events(
        &self,
        turn_id: TurnId,
        limits: SubscriptionLimits,
    ) -> Result<TurnEvents, ViewError> {
        let request = ViewRequest::new(EventVisibility::Content, limits);
        let mut state = lock(&self.inner.state);
        let cursor = cursor_for(
            &self.inner,
            state.branch_sequence,
            request.visibility,
            Some(turn_id),
        );
        let events = self.subscribe_locked(&mut state, request, Some(turn_id), cursor, false)?;
        Ok(TurnEvents::new(events))
    }

    pub(crate) fn subscribe_turn_tail(
        &self,
        turn_id: TurnId,
        limits: SubscriptionLimits,
    ) -> Result<TurnEvents, ViewError> {
        let request = ViewRequest::new(EventVisibility::Content, limits);
        let mut state = lock(&self.inner.state);
        let cursor = cursor_for(
            &self.inner,
            state.branch_sequence,
            request.visibility,
            Some(turn_id),
        );
        let events = self.subscribe_locked(&mut state, request, Some(turn_id), cursor, true)?;
        Ok(TurnEvents::new(events))
    }

    pub(crate) fn resume(
        &self,
        cursor: EventCursor,
        request: ViewRequest,
    ) -> Result<ResumeOutcome, ViewError> {
        if !cursor_compatible(&self.inner, cursor, request) {
            return Err(ViewError::IncompatibleCursor);
        }
        let mut state = lock(&self.inner.state);
        if cursor.sequence.0 > state.branch_sequence {
            return Err(ViewError::CursorAhead);
        }
        let requested = cursor.sequence.0.saturating_add(1);
        let first = state.retained.front().map(|event| event.branch_sequence.0);
        let unavailable =
            requested <= state.branch_sequence && first.is_none_or(|first| requested < first);
        let replay_exceeds_limits = !unavailable
            && replay_debt_exceeds(&state, requested, request.visibility, request.limits);
        if unavailable {
            let attachment = self.attachment_locked(&mut state, request, None)?;
            return Ok(ResumeOutcome::RangeUnavailable(attachment));
        }
        if replay_exceeds_limits {
            let attachment = self.attachment_locked(&mut state, request, None)?;
            return Ok(ResumeOutcome::ReplayLimitExceeded(attachment));
        }
        let events = self.subscribe_locked(&mut state, request, None, cursor, true)?;
        Ok(ResumeOutcome::Replaying(events))
    }

    /// Last-resort liveness boundary used only when the actor supervisor
    /// catches an unwind while normal ordered close publication is impossible.
    pub(crate) fn force_close(&self) {
        let mut state = lock(&self.inner.state);
        state.closed = true;
        let sequence = state.branch_sequence;
        drop(state);
        self.inner.changed.send_replace(sequence);
    }

    fn attach_filtered(
        &self,
        request: ViewRequest,
        turn_filter: Option<TurnId>,
    ) -> Result<ViewAttachment, ViewError> {
        let mut state = lock(&self.inner.state);
        self.attachment_locked(&mut state, request, turn_filter)
    }

    fn attachment_locked(
        &self,
        state: &mut EventHubState,
        request: ViewRequest,
        turn_filter: Option<TurnId>,
    ) -> Result<ViewAttachment, ViewError> {
        let cursor = cursor_for(
            &self.inner,
            state.branch_sequence,
            request.visibility,
            turn_filter,
        );
        let snapshot = snapshot_from(
            state,
            cursor,
            request.visibility,
            self.inner.limits.retained_items,
        );
        let events = self.subscribe_locked(state, request, turn_filter, cursor, true)?;
        Ok(ViewAttachment { snapshot, events })
    }

    fn subscribe_locked(
        &self,
        state: &mut EventHubState,
        request: ViewRequest,
        turn_filter: Option<TurnId>,
        cursor: EventCursor,
        registered: bool,
    ) -> Result<EventSubscription, ViewError> {
        if registered && state.subscribers >= self.inner.limits.max_subscribers {
            return Err(ViewError::SubscriberLimit);
        }
        if registered {
            state.subscribers = state.subscribers.saturating_add(1);
        }
        if let Some(turn_id) = turn_filter {
            state
                .turn_subscribers
                .entry(turn_id)
                .and_modify(|count| *count = count.saturating_add(1))
                .or_insert(1);
        }
        Ok(EventSubscription {
            inner: Arc::clone(&self.inner),
            changed: self.inner.changed.subscribe(),
            next_sequence: cursor.sequence.0.saturating_add(1),
            last_delivered: cursor,
            request,
            turn_filter,
            terminal: None,
            registered,
            subscribed: true,
        })
    }
}

impl EventHubState {
    /// Retained events at or after `sequence`. Retention holds a contiguous sequence range
    /// (events are appended in sequence order and evicted from the front), so the start is an
    /// index, not a scan.
    fn retained_from(
        &self,
        sequence: u64,
    ) -> std::collections::vec_deque::Iter<'_, Arc<AgentEvent>> {
        let skip = self.retained.front().map_or(0, |first| {
            usize::try_from(sequence.saturating_sub(first.branch_sequence.0)).unwrap_or(usize::MAX)
        });
        self.retained.range(skip.min(self.retained.len())..)
    }
}

fn replay_debt_exceeds(
    state: &EventHubState,
    requested: u64,
    visibility: EventVisibility,
    limits: SubscriptionLimits,
) -> bool {
    let mut items = 0_usize;
    let mut bytes = 0_usize;
    for event in state.retained_from(requested) {
        items = items.saturating_add(1);
        bytes = bytes.saturating_add(event.projected_retained_bytes(visibility));
        if items > limits.max_items() || bytes > limits.max_bytes() {
            return true;
        }
    }
    false
}

fn cursor_for(
    inner: &EventHubInner,
    sequence: u64,
    visibility: EventVisibility,
    turn_filter: Option<TurnId>,
) -> EventCursor {
    EventCursor {
        session_id: inner.session_id,
        branch_id: inner.branch_id,
        run_id: inner.run_id,
        sequence: EventSequence(sequence),
        event_schema: EVENT_SCHEMA_VERSION,
        projection_revision: PROJECTION_REVISION,
        visibility,
        scope: turn_filter.map_or(EventScope::Agent, EventScope::Turn),
    }
}

fn cursor_compatible(inner: &EventHubInner, cursor: EventCursor, request: ViewRequest) -> bool {
    cursor.session_id == inner.session_id
        && cursor.branch_id == inner.branch_id
        && cursor.run_id == inner.run_id
        && cursor.event_schema == EVENT_SCHEMA_VERSION
        && cursor.projection_revision == PROJECTION_REVISION
        && cursor.visibility == request.visibility
        && cursor.scope == EventScope::Agent
}

fn snapshot_from(
    state: &EventHubState,
    cursor: EventCursor,
    visibility: EventVisibility,
    retained_turns: usize,
) -> AgentViewSnapshot {
    AgentViewSnapshot {
        cursor,
        state: state.view.lifecycle,
        presentation_incomplete: false,
        turns: state.view.turns.values().cloned().collect(),
        committed_conversation_items: state.view.committed_conversation_items,
        conversation: (visibility == EventVisibility::Content)
            .then(|| state.view.conversation.clone()),
        pending_effect_decisions: state
            .view
            .pending_effect_decisions
            .iter()
            .cloned()
            .map(|mut decision| {
                if visibility == EventVisibility::Metadata {
                    decision.prompt = None;
                }
                decision
            })
            .collect(),
        tool_activity: state
            .view
            .tool_activity
            .iter()
            .cloned()
            .map(|mut activity| {
                if visibility == EventVisibility::Metadata {
                    activity.message = None;
                }
                activity
            })
            .collect(),
        tool_activity_tombstones: state.view.tool_activity_tombstones.clone(),
        retired_tool_fence: state.view.retired_tool_fence.clone(),
        active_attempts: state.view.active_attempts.clone(),
        usage: state.view.usage,
        completed_turns: state.completed_turns.clone(),
        terminal_turn_tombstones: state.view.terminal_turn_tombstones.clone(),
        terminal_turn_fence: state.view.terminal_turn_fence.clone(),
        recovery_promotable_turn: state.view.recovery_promotable_turn,
        retained_turns,
        retained_conversation_bytes: state.view.retained_conversation_bytes,
    }
}

fn validate_causal_context(state: &EventHubState, event: &AgentEvent) -> Result<(), ()> {
    if matches!(event.kind(), AgentEventKind::ConversationCommitted { .. }) {
        let turn_id = event.turn_id.ok_or(())?;
        let turn = state.view.turns.get(&turn_id);
        let active = turn.is_some_and(|turn| turn.state() == TurnViewState::Running);
        let completed = turn.is_some_and(|turn| turn.state() != TurnViewState::Running)
            && state.completed_turn_parents.contains_key(&turn_id);
        if !active && !completed && state.view.recovery_promotable_turn != Some(turn_id) {
            return Err(());
        }
    }
    Ok(())
}

fn causal_parent(
    state: &EventHubState,
    context: EventContext,
    kind: &AgentEventKind,
) -> Option<EventId> {
    if matches!(kind, AgentEventKind::TurnStarted) {
        return None;
    }
    if matches!(kind, AgentEventKind::AgentClosed) {
        return state.last_event;
    }
    let active_parent = context
        .turn_id
        .zip(context.attempt_id)
        .zip(context.call_id)
        .and_then(|((turn, attempt), call)| state.call_parents.get(&(turn, attempt, call)).copied())
        .or_else(|| {
            context
                .turn_id
                .zip(context.attempt_id)
                .and_then(|binding| state.attempt_parents.get(&binding).copied())
        })
        .or_else(|| {
            context
                .turn_id
                .and_then(|turn| state.turn_parents.get(&turn).copied())
        });
    active_parent.or_else(|| {
        matches!(kind, AgentEventKind::ConversationCommitted { .. })
            .then(|| {
                context
                    .turn_id
                    .and_then(|turn| state.completed_turn_parents.get(&turn).copied())
            })
            .flatten()
    })
}

fn update_causal_parents(
    state: &mut EventHubState,
    context: EventContext,
    kind: &AgentEventKind,
    event_id: EventId,
) {
    if matches!(kind, AgentEventKind::ConversationCommitted { .. })
        && let Some(turn_id) = context.turn_id
    {
        if let Some(parent) = state.completed_turn_parents.get_mut(&turn_id) {
            *parent = event_id;
            return;
        }
        if !state
            .view
            .turns
            .get(&turn_id)
            .is_some_and(|turn| turn.state() == TurnViewState::Running)
        {
            return;
        }
    }
    if let Some(turn_id) = context.turn_id {
        state.turn_parents.insert(turn_id, event_id);
    }
    if let (Some(attempt_id), Some(turn_id)) = (context.attempt_id, context.turn_id) {
        state
            .attempt_parents
            .insert((turn_id, attempt_id), event_id);
    }
    if let (Some(call_id), Some(attempt_id), Some(turn_id)) =
        (context.call_id, context.attempt_id, context.turn_id)
    {
        let binding = (turn_id, attempt_id, call_id);
        if call_is_terminal(kind) {
            state.call_parents.remove(&binding);
        } else {
            state.call_parents.insert(binding, event_id);
        }
    }
}

fn call_is_terminal(kind: &AgentEventKind) -> bool {
    matches!(
        kind,
        AgentEventKind::ToolCompleted { .. }
            | AgentEventKind::ToolPreparationRejected { .. }
            | AgentEventKind::EffectDecisionResolved {
                resolution: EffectDecisionResolution::Denied
                    | EffectDecisionResolution::Expired
                    | EffectDecisionResolution::Cancelled
                    | EffectDecisionResolution::Shutdown
                    | EffectDecisionResolution::Invalidated,
                ..
            }
    )
}

fn complete_turn_indexes(state: &mut EventHubState, turn_id: TurnId, retained_turns: usize) {
    if let Some(terminal_parent) = state.turn_parents.remove(&turn_id) {
        state
            .completed_turn_parents
            .insert(turn_id, terminal_parent);
    }
    state
        .attempt_parents
        .retain(|(owner, _), _| *owner != turn_id);
    state
        .call_parents
        .retain(|(owner, _, _), _| *owner != turn_id);
    state.completed_turns.push_back(turn_id);
    while state.completed_turns.len() > retained_turns {
        let Some(evicted_turn) = state.completed_turns.pop_front() else {
            break;
        };
        state.completed_turn_parents.remove(&evicted_turn);
        state.view.turns.remove(&evicted_turn);
        state.view.terminal_turn_tombstones.push_back(evicted_turn);
        while state.view.terminal_turn_tombstones.len() > retained_turns {
            let _ = state.view.terminal_turn_tombstones.pop_front();
        }
        if !state.turn_subscribers.contains_key(&evicted_turn) {
            state.turn_evicted_through.remove(&evicted_turn);
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests;
