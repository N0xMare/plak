//! Presentation snapshots and the shared transition planner.

mod conversation;
pub use conversation::ConversationItemView;
pub(super) use conversation::ConversationProjection;

use super::hub::subscription::ViewError;
use super::types::{
    AgentEvent, AgentEventKind, EventCursor, EventDelivery, EventLostRange,
    MAX_LIFETIME_IDENTITIES, ToolAuditIdentity, ToolProgressSummary,
};
use crate::{AgentErrorKind, EffectDecisionRef, TurnUsage, UsageTotals};
use plakous_model::{AttemptId, CallId, ModelEvent, Timestamp, TurnId, Usage};
use plakous_tools::EffectDecisionPrompt;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::sync::Arc;
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LifetimeFenceDomain {
    TerminalTurn,
    RetiredToolCall,
}

#[derive(Clone, Copy)]
enum LifetimeFenceIdentity {
    Turn(TurnId),
    ToolCall(TurnId, AttemptId, CallId),
}

impl LifetimeFenceIdentity {
    const fn domain(self) -> LifetimeFenceDomain {
        match self {
            Self::Turn(_) => LifetimeFenceDomain::TerminalTurn,
            Self::ToolCall(_, _, _) => LifetimeFenceDomain::RetiredToolCall,
        }
    }

    fn stable_key(self) -> Vec<u8> {
        let mut key = Vec::with_capacity(match self {
            Self::Turn(_) => 16,
            Self::ToolCall(_, _, _) => 48,
        });
        match self {
            Self::Turn(turn_id) => key.extend_from_slice(turn_id.as_uuid().as_bytes()),
            Self::ToolCall(turn_id, attempt_id, call_id) => {
                key.extend_from_slice(turn_id.as_uuid().as_bytes());
                key.extend_from_slice(attempt_id.as_uuid().as_bytes());
                key.extend_from_slice(call_id.as_uuid().as_bytes());
            }
        }
        key
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct LifetimeFence {
    domain: LifetimeFenceDomain,
    identities: BTreeSet<Vec<u8>>,
    refuses_continuation: bool,
}

impl LifetimeFence {
    pub(super) fn new(domain: LifetimeFenceDomain) -> Self {
        Self {
            domain,
            identities: BTreeSet::new(),
            refuses_continuation: false,
        }
    }

    fn contains(&self, identity: LifetimeFenceIdentity) -> bool {
        self.refuses_continuation
            || self.domain != identity.domain()
            || self.identities.contains(&identity.stable_key())
    }
}

/// Public lifecycle state in the non-authoritative presentation projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum AgentViewState {
    /// The actor may accept or finish work.
    Running,
    /// The event source has closed.
    Closed,
}

/// Public lifecycle state of one observed turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TurnViewState {
    /// The turn is active.
    Running,
    /// The turn completed after semantic commitment.
    Completed,
    /// The turn failed or was cancelled.
    Failed,
}

/// Non-authoritative presentation projection for one turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnView {
    turn_id: TurnId,
    state: TurnViewState,
    model_attempts: usize,
    tools_authorized: usize,
    tools_started: usize,
    tools_completed: usize,
    usage: Option<TurnUsage>,
    live_usage: Usage,
    live_usage_base: Usage,
    failure: Option<AgentErrorKind>,
}

impl TurnView {
    fn running(turn_id: TurnId) -> Self {
        Self {
            turn_id,
            state: TurnViewState::Running,
            model_attempts: 0,
            tools_authorized: 0,
            tools_started: 0,
            tools_completed: 0,
            usage: None,
            live_usage: Usage::default(),
            live_usage_base: Usage::default(),
            failure: None,
        }
    }

    /// Returns the turn identity.
    #[must_use]
    pub const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    /// Returns the projected lifecycle state.
    #[must_use]
    pub const fn state(&self) -> TurnViewState {
        self.state
    }

    /// Returns the number of observed model operations.
    #[must_use]
    pub const fn model_attempts(&self) -> usize {
        self.model_attempts
    }

    /// Returns the number of observed authorized tool calls.
    #[must_use]
    pub const fn tools_authorized(&self) -> usize {
        self.tools_authorized
    }

    /// Returns the number of authorized tool calls that crossed dispatch.
    #[must_use]
    pub const fn tools_started(&self) -> usize {
        self.tools_started
    }

    /// Returns the number of observed terminal tool projections.
    #[must_use]
    pub const fn tools_completed(&self) -> usize {
        self.tools_completed
    }

    /// Returns observational token totals and the totals before the current
    /// model dispatch. Neither value is checked billing or terminal authority.
    #[must_use]
    pub const fn live_usage(&self) -> (Usage, Usage) {
        (self.live_usage, self.live_usage_base)
    }

    /// Returns complete checked usage after the turn reaches a terminal event.
    #[must_use]
    pub const fn usage(&self) -> Option<TurnUsage> {
        self.usage
    }

    /// Returns the redaction-safe terminal failure category.
    #[must_use]
    pub const fn failure(&self) -> Option<AgentErrorKind> {
        self.failure
    }
}

/// Recovery-complete non-authoritative view of one pending tool decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingEffectDecisionView {
    identity: ToolAuditIdentity,
    decision: EffectDecisionRef,
    pub(super) prompt: Option<EffectDecisionPrompt>,
    expires_at: Timestamp,
}

impl PendingEffectDecisionView {
    /// Returns the exact canonical/exposed identity awaiting decision.
    #[must_use]
    pub const fn identity(&self) -> &ToolAuditIdentity {
        &self.identity
    }

    /// Returns the exact binding-addressed decision reference.
    #[must_use]
    pub const fn decision(&self) -> &EffectDecisionRef {
        &self.decision
    }

    /// Returns descriptive content only under content visibility.
    #[must_use]
    pub const fn prompt(&self) -> Option<&EffectDecisionPrompt> {
        self.prompt.as_ref()
    }

    /// Returns the display timestamp for the monotonic effect-decision deadline.
    #[must_use]
    pub const fn expires_at(&self) -> Timestamp {
        self.expires_at
    }
}

/// Bounded current activity for one dispatched local tool call.
#[derive(Clone, Eq, PartialEq)]
pub struct ToolActivityView {
    turn_id: TurnId,
    attempt_id: AttemptId,
    call_id: CallId,
    identity: ToolAuditIdentity,
    progress: Option<ToolProgressSummary>,
    pub(super) message: Option<Arc<str>>,
}

impl fmt::Debug for ToolActivityView {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolActivityView")
            .field("turn_id", &self.turn_id)
            .field("attempt_id", &self.attempt_id)
            .field("call_id", &self.call_id)
            .field("identity", &self.identity)
            .field("progress", &self.progress)
            .field("message", &self.message.as_ref().map(|_| "<redacted>"))
            .field(
                "message_bytes",
                &self.message.as_ref().map_or(0, |message| message.len()),
            )
            .finish()
    }
}

impl ToolActivityView {
    fn started(
        turn_id: TurnId,
        attempt_id: AttemptId,
        call_id: CallId,
        identity: ToolAuditIdentity,
    ) -> Self {
        Self {
            turn_id,
            attempt_id,
            call_id,
            identity,
            progress: None,
            message: None,
        }
    }

    /// Returns the exact logical turn that owns this activity.
    #[must_use]
    pub const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    /// Returns the exact model attempt that owns this activity.
    #[must_use]
    pub const fn attempt_id(&self) -> AttemptId {
        self.attempt_id
    }

    /// Returns the exact canonical call identity.
    #[must_use]
    pub const fn call_id(&self) -> CallId {
        self.call_id
    }

    /// Returns the exact registered and provider-facing audit identity.
    #[must_use]
    pub const fn identity(&self) -> &ToolAuditIdentity {
        &self.identity
    }

    /// Returns the latest ordered content-free progress counters, when reported.
    #[must_use]
    pub const fn progress(&self) -> Option<ToolProgressSummary> {
        self.progress
    }

    /// Returns the latest bounded progress message only under content visibility.
    #[must_use]
    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(super) struct ToolActivityTombstone {
    turn_id: TurnId,
    attempt_id: AttemptId,
    call_id: CallId,
    identity: ToolAuditIdentity,
}

impl ToolActivityTombstone {
    fn from_activity(activity: ToolActivityView) -> Self {
        Self {
            turn_id: activity.turn_id,
            attempt_id: activity.attempt_id,
            call_id: activity.call_id,
            identity: activity.identity,
        }
    }
}

/// Complete non-authoritative presentation projection through one cursor.
#[derive(Clone, PartialEq)]
pub struct AgentViewSnapshot {
    pub(super) cursor: EventCursor,
    pub(super) state: AgentViewState,
    pub(super) presentation_incomplete: bool,
    pub(super) turns: Vec<TurnView>,
    pub(super) committed_conversation_items: usize,
    pub(super) conversation: Option<ConversationProjection>,
    pub(super) pending_effect_decisions: Vec<PendingEffectDecisionView>,
    pub(super) tool_activity: Vec<ToolActivityView>,
    pub(super) tool_activity_tombstones: VecDeque<ToolActivityTombstone>,
    pub(super) retired_tool_fence: LifetimeFence,
    pub(super) active_attempts: BTreeSet<(TurnId, AttemptId)>,
    pub(super) usage: UsageTotals,
    pub(super) completed_turns: VecDeque<TurnId>,
    pub(super) terminal_turn_tombstones: VecDeque<TurnId>,
    pub(super) terminal_turn_fence: LifetimeFence,
    pub(super) recovery_promotable_turn: Option<TurnId>,
    pub(super) retained_turns: usize,
    pub(super) retained_conversation_bytes: usize,
}

impl AgentViewSnapshot {
    /// Returns the exact projection boundary.
    #[must_use]
    pub const fn cursor(&self) -> EventCursor {
        self.cursor
    }

    /// Returns the projected agent lifecycle.
    #[must_use]
    pub const fn state(&self) -> AgentViewState {
        self.state
    }

    /// Reports whether bounded live delivery omitted presentation-only events.
    #[must_use]
    pub const fn presentation_incomplete(&self) -> bool {
        self.presentation_incomplete
    }

    /// Returns turns in stable identity order.
    #[must_use]
    pub fn turns(&self) -> &[TurnView] {
        &self.turns
    }

    /// Returns the total committed conversation item count.
    #[must_use]
    pub const fn committed_conversation_items(&self) -> usize {
        self.committed_conversation_items
    }

    /// Returns the bounded retained tail of presentation-safe summaries only
    /// under content visibility. [`Self::committed_conversation_items`] reports
    /// the complete authoritative item count represented by the projection.
    #[must_use]
    pub fn conversation(&self) -> Option<&[ConversationItemView]> {
        self.conversation
            .as_ref()
            .map(ConversationProjection::items)
    }

    /// Returns recovery-complete pending effect decisions in admission order.
    #[must_use]
    pub fn pending_effect_decisions(&self) -> &[PendingEffectDecisionView] {
        &self.pending_effect_decisions
    }

    /// Returns bounded current local tool activity in dispatch order.
    #[must_use]
    pub fn tool_activity(&self) -> &[ToolActivityView] {
        &self.tool_activity
    }

    /// Returns checked aggregate usage across every observed terminal turn.
    #[must_use]
    pub const fn usage(&self) -> UsageTotals {
        self.usage
    }
}

impl fmt::Debug for AgentViewSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentViewSnapshot")
            .field("cursor", &self.cursor)
            .field("state", &self.state)
            .field("presentation_incomplete", &self.presentation_incomplete)
            .field("turns", &self.turns)
            .field(
                "committed_conversation_items",
                &self.committed_conversation_items,
            )
            .field("conversation", &"<redacted>")
            .field(
                "pending_decision_count",
                &self.pending_effect_decisions.len(),
            )
            .field("active_tool_count", &self.tool_activity.len())
            .field(
                "tool_activity_tombstones",
                &self.tool_activity_tombstones.len(),
            )
            .field("retired_tool_fence", &self.retired_tool_fence)
            .field("active_attempt_count", &self.active_attempts.len())
            .field("usage", &self.usage)
            .field("retained_completed_turns", &self.completed_turns.len())
            .field(
                "terminal_turn_tombstones",
                &self.terminal_turn_tombstones.len(),
            )
            .field("terminal_turn_fence", &self.terminal_turn_fence)
            .field("recovery_promotable_turn", &self.recovery_promotable_turn)
            .field("retained_turn_limit", &self.retained_turns)
            .field(
                "retained_conversation_byte_limit",
                &self.retained_conversation_bytes,
            )
            .finish()
    }
}

/// Error applying an incompatible or discontinuous event to a presentation reducer.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum ReducerError {
    /// Event and snapshot scopes differ.
    #[error("event is incompatible with the read-model scope")]
    IncompatibleScope,
    /// The reducer observed a sequence gap or duplicate.
    #[error("event sequence is not the exact next read-model sequence")]
    NonContiguous,
    /// Aggregate usage could not be represented without partial mutation.
    #[error("event usage exceeds the read-model numeric range")]
    UsageOverflow,
    /// An event attempted to extend an agent or turn after its terminal boundary.
    #[error("event extends a terminal read-model lifecycle")]
    InvalidLifecycle,
    /// A monotonic lifetime fence reached its conservative session budget.
    #[error("event lifetime fence requires a new session")]
    FenceSaturated,
}

/// Pure deterministic reducer for replaceable presentation consumers.
#[derive(Clone, Debug)]
pub struct AgentViewReducer {
    snapshot: AgentViewSnapshot,
}

impl AgentViewReducer {
    /// Starts from an atomic snapshot.
    #[must_use]
    pub const fn from_snapshot(snapshot: AgentViewSnapshot) -> Self {
        Self { snapshot }
    }

    /// Applies the exact next projected event.
    ///
    /// # Errors
    ///
    /// Returns a typed mismatch for a different scope or a sequence gap.
    pub fn apply(&mut self, event: &AgentEvent) -> Result<(), ReducerError> {
        let cursor = self.snapshot.cursor;
        if event.session_id != cursor.session_id
            || event.branch_id != cursor.branch_id
            || event.run_id != cursor.run_id
            || event.event_schema != cursor.event_schema
            || event.projection_revision != cursor.projection_revision
            || event.visibility != cursor.visibility
            || event.scope != cursor.scope
        {
            return Err(ReducerError::IncompatibleScope);
        }
        let expected = cursor
            .sequence
            .0
            .checked_add(1)
            .ok_or(ReducerError::NonContiguous)?;
        if event.branch_sequence.0 != expected {
            return Err(ReducerError::NonContiguous);
        }
        let delta = preflight_snapshot(&self.snapshot, event)?;
        apply_snapshot_delta(&mut self.snapshot, delta);
        self.snapshot.cursor = event.cursor();
        Ok(())
    }

    /// Applies one exact event or a typed pressure-loss range.
    ///
    /// # Errors
    ///
    /// Returns a typed mismatch without mutating the projection.
    pub fn apply_delivery(&mut self, delivery: &EventDelivery) -> Result<(), ReducerError> {
        match delivery {
            EventDelivery::Event(event) => self.apply(event),
            EventDelivery::Lost(lost) => self.apply_lost(*lost),
        }
    }

    /// Applies one adjacent droppable-event loss range by advancing only the
    /// cursor and incompleteness indicator.
    ///
    /// # Errors
    ///
    /// Returns a typed mismatch without mutating the projection.
    pub fn apply_lost(&mut self, lost: EventLostRange) -> Result<(), ReducerError> {
        if self.snapshot.state == AgentViewState::Closed {
            return Err(ReducerError::InvalidLifecycle);
        }
        let cursor = self.snapshot.cursor;
        if lost.after != cursor {
            return Err(ReducerError::IncompatibleScope);
        }
        let expected = cursor
            .sequence
            .0
            .checked_add(1)
            .ok_or(ReducerError::NonContiguous)?;
        if lost.first().0 != expected || lost.through.0 < expected {
            return Err(ReducerError::NonContiguous);
        }
        self.snapshot.presentation_incomplete = true;
        self.snapshot.cursor = lost.cursor();
        Ok(())
    }

    /// Returns the current projection.
    #[must_use]
    pub const fn snapshot(&self) -> &AgentViewSnapshot {
        &self.snapshot
    }

    /// Consumes the reducer and returns its projection.
    #[must_use]
    pub fn into_snapshot(self) -> AgentViewSnapshot {
        self.snapshot
    }
}

#[derive(Clone)]
pub(super) struct ProjectionState {
    pub(super) lifecycle: AgentViewState,
    pub(super) turns: BTreeMap<TurnId, TurnView>,
    pub(super) terminal_turn_tombstones: VecDeque<TurnId>,
    pub(super) terminal_turn_fence: LifetimeFence,
    pub(super) recovery_promotable_turn: Option<TurnId>,
    pub(super) active_attempts: BTreeSet<(TurnId, AttemptId)>,
    pub(super) committed_conversation_items: usize,
    pub(super) conversation: ConversationProjection,
    pub(super) retained_conversation_items: usize,
    pub(super) retained_conversation_bytes: usize,
    pub(super) pending_effect_decisions: Vec<PendingEffectDecisionView>,
    pub(super) tool_activity: Vec<ToolActivityView>,
    pub(super) tool_activity_tombstones: VecDeque<ToolActivityTombstone>,
    pub(super) retired_tool_fence: LifetimeFence,
    pub(super) usage: UsageTotals,
}

#[derive(Clone)]
pub(super) enum TransitionPlan {
    Close,
    TurnStarted {
        turn_id: TurnId,
    },
    TurnChanged {
        turn_id: TurnId,
        turn: TurnView,
        attempt: Option<(TurnId, AttemptId)>,
    },
    DecisionRequired {
        attempt: (TurnId, AttemptId),
        decision: Option<PendingEffectDecisionView>,
    },
    EffectDecisionResolved {
        decision: EffectDecisionRef,
    },
    ToolStarted {
        attempt: (TurnId, AttemptId),
        activity: ToolActivityView,
        turn_id: TurnId,
        turn: TurnView,
    },
    ToolCompleted {
        index: usize,
        tombstone: Option<ToolActivityTombstone>,
        retired_key: Vec<u8>,
        retained_items: usize,
        turn_id: TurnId,
        turn: TurnView,
    },
    ToolProgress {
        index: usize,
        activity: ToolActivityView,
    },
    Conversation {
        item_count: usize,
        items: Option<Arc<[ConversationItemView]>>,
        turn_id: TurnId,
    },
    Terminal {
        turn_id: TurnId,
        turn: TurnView,
        usage: UsageTotals,
        terminal_key: Vec<u8>,
    },
    Noop,
}

#[derive(Clone, Copy)]
struct TransitionState<'a> {
    turn: Option<&'a TurnView>,
    pending_effect_decisions: &'a [PendingEffectDecisionView],
    tool_activity: &'a [ToolActivityView],
    tool_activity_tombstones: &'a VecDeque<ToolActivityTombstone>,
    retired_tool_fence: &'a LifetimeFence,
    terminal_turn_fence: &'a LifetimeFence,
    usage: UsageTotals,
    retained_items: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ToolProgressDisposition {
    Applied,
    Stale,
    Idempotent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ToolActivityError {
    MissingStart,
    IdentityConflict,
    ConflictingProgress,
    Retired,
}

#[allow(
    clippy::too_many_arguments,
    reason = "the full composite call identity and lifetime retirement fence stay explicit"
)]
fn update_tool_activity(
    activity: &mut [ToolActivityView],
    retired_fence: &LifetimeFence,
    turn_id: TurnId,
    attempt_id: AttemptId,
    call_id: CallId,
    identity: &ToolAuditIdentity,
    progress: ToolProgressSummary,
    message: Option<Arc<str>>,
) -> Result<ToolProgressDisposition, ToolActivityError> {
    let Some(existing) = activity.iter_mut().find(|item| {
        item.turn_id == turn_id && item.attempt_id == attempt_id && item.call_id == call_id
    }) else {
        return Err(
            if retired_fence.contains(LifetimeFenceIdentity::ToolCall(
                turn_id, attempt_id, call_id,
            )) {
                ToolActivityError::Retired
            } else {
                ToolActivityError::MissingStart
            },
        );
    };
    if &existing.identity != identity {
        return Err(ToolActivityError::IdentityConflict);
    }
    if let Some(current) = existing.progress {
        if progress.sequence() < current.sequence() {
            return Ok(ToolProgressDisposition::Stale);
        }
        if progress.sequence() == current.sequence() {
            return if progress == current && message == existing.message {
                Ok(ToolProgressDisposition::Idempotent)
            } else {
                Err(ToolActivityError::ConflictingProgress)
            };
        }
    }
    existing.progress = Some(progress);
    existing.message = message;
    Ok(ToolProgressDisposition::Applied)
}

/// Live usage is reported cumulatively per model dispatch; the provider cost is not carried.
fn saturating_token_sum(base: Usage, usage: Usage) -> Usage {
    Usage::new(
        base.input_tokens().saturating_add(usage.input_tokens()),
        base.output_tokens().saturating_add(usage.output_tokens()),
        base.reasoning_tokens()
            .saturating_add(usage.reasoning_tokens()),
        base.cached_input_tokens()
            .saturating_add(usage.cached_input_tokens()),
        base.cache_write_tokens()
            .saturating_add(usage.cache_write_tokens()),
    )
}

fn event_context_shape_is_valid(event: &AgentEvent) -> bool {
    let shape_is_valid = match event.kind() {
        AgentEventKind::AgentClosed => {
            event.turn_id.is_none() && event.attempt_id.is_none() && event.call_id.is_none()
        }
        AgentEventKind::JobPromoted { .. } | AgentEventKind::JobFinished { .. } => {
            event.attempt_id.is_none() && event.call_id.is_none()
        }
        // `OutputBoundRederived` is turn-scoped because the product's only subscription is per
        // turn.
        AgentEventKind::TurnStarted
        | AgentEventKind::OutputBoundRederived { .. }
        | AgentEventKind::SteeringQueued { .. }
        | AgentEventKind::SteeringApplied
        | AgentEventKind::OperationBudgetExhausted
        | AgentEventKind::ModelRequestQueued { .. }
        | AgentEventKind::OutputLimitRetry { .. }
        | AgentEventKind::ToolResultsReplaced { .. }
        | AgentEventKind::ConversationCommitted { .. }
        | AgentEventKind::CompactionStarted { .. }
        | AgentEventKind::CompactionFinished { .. }
        | AgentEventKind::TurnCompleted { .. }
        | AgentEventKind::TurnFailed { .. } => {
            event.turn_id.is_some() && event.attempt_id.is_none() && event.call_id.is_none()
        }
        AgentEventKind::ModelStarted
        | AgentEventKind::ModelDispatchStarted
        | AgentEventKind::RequestPrepared { .. }
        | AgentEventKind::Model { .. } => {
            event.turn_id.is_some() && event.attempt_id.is_some() && event.call_id.is_none()
        }
        // Observation notes are named for the call whose observation was cut, so a surface can
        // attach the note to that tool row rather than to the turn.
        AgentEventKind::EffectDecisionRequired { .. }
        | AgentEventKind::EffectDecisionResolved { .. }
        | AgentEventKind::ToolAuthorized { .. }
        | AgentEventKind::ToolStarted { .. }
        | AgentEventKind::ToolProgress { .. }
        | AgentEventKind::ToolPreparationRejected { .. }
        | AgentEventKind::ToolCompleted { .. }
        | AgentEventKind::ObservationClipped { .. }
        | AgentEventKind::ObservationReplaced { .. } => {
            event.turn_id.is_some() && event.attempt_id.is_some() && event.call_id.is_some()
        }
    };
    shape_is_valid && tool_progress_content_is_valid(event)
}

fn tool_progress_content_is_valid(event: &AgentEvent) -> bool {
    match event.kind() {
        AgentEventKind::ToolProgress {
            summary,
            progress: Some(progress),
            ..
        } => {
            event.call_id == Some(progress.call_id())
                && *summary == ToolProgressSummary::from_event_progress(progress)
        }
        _ => true,
    }
}

fn validate_projection_context(
    state: &ProjectionState,
    event: &AgentEvent,
    retained_items: usize,
) -> Result<(), ()> {
    if !event_context_shape_is_valid(event) {
        return Err(());
    }
    if matches!(event.kind(), AgentEventKind::AgentClosed)
        || (event.turn_id.is_none()
            && matches!(
                event.kind(),
                AgentEventKind::JobPromoted { .. } | AgentEventKind::JobFinished { .. }
            ))
    {
        return Ok(());
    }
    let turn_id = event.turn_id.ok_or(())?;
    let turn = state.turns.get(&turn_id);
    let active = turn.is_some_and(|turn| turn.state == TurnViewState::Running);
    match event.kind() {
        AgentEventKind::TurnStarted => {
            if state
                .terminal_turn_fence
                .contains(LifetimeFenceIdentity::Turn(turn_id))
            {
                return Err(());
            }
            if turn.is_some()
                || state
                    .turns
                    .values()
                    .filter(|turn| turn.state == TurnViewState::Running)
                    .count()
                    >= retained_items
                || !terminal_fence_can_reserve(
                    &state.terminal_turn_fence,
                    state
                        .turns
                        .values()
                        .filter(|turn| turn.state == TurnViewState::Running)
                        .map(|turn| turn.turn_id)
                        .chain(std::iter::once(turn_id)),
                )
            {
                return Err(());
            }
        }
        AgentEventKind::ConversationCommitted { .. } => {
            let completed = turn.is_some_and(|turn| turn.state != TurnViewState::Running);
            if !active && !completed && state.recovery_promotable_turn != Some(turn_id) {
                return Err(());
            }
        }
        AgentEventKind::TurnCompleted { .. } | AgentEventKind::TurnFailed { .. } => {
            if !active
                && (turn.is_some()
                    || state
                        .terminal_turn_fence
                        .contains(LifetimeFenceIdentity::Turn(turn_id)))
            {
                return Err(());
            }
        }
        _ if !active => return Err(()),
        _ => {}
    }
    if event.attempt_id.is_some() {
        let binding = (turn_id, event.attempt_id.ok_or(())?);
        if !state.active_attempts.contains(&binding)
            && matches!(
                event.kind(),
                AgentEventKind::EffectDecisionResolved { .. }
                    | AgentEventKind::ToolProgress { .. }
                    | AgentEventKind::ToolCompleted { .. }
            )
        {
            return Err(());
        }
    }
    Ok(())
}

fn terminal_fence_can_reserve(
    fence: &LifetimeFence,
    turns: impl IntoIterator<Item = TurnId>,
) -> bool {
    if fence.refuses_continuation || fence.domain != LifetimeFenceDomain::TerminalTurn {
        return false;
    }
    let mut remaining = MAX_LIFETIME_IDENTITIES.saturating_sub(fence.identities.len());
    for turn_id in turns {
        if !fence.contains(LifetimeFenceIdentity::Turn(turn_id)) {
            if remaining == 0 {
                return false;
            }
            remaining -= 1;
        }
    }
    true
}

pub(super) fn preflight_projection(
    state: &ProjectionState,
    event: &AgentEvent,
    retained_items: usize,
) -> Result<TransitionPlan, ViewError> {
    validate_projection_for_bounded_delta(state, event, retained_items)?;
    plan_transition(
        event,
        TransitionState {
            turn: event.turn_id.and_then(|turn_id| state.turns.get(&turn_id)),
            pending_effect_decisions: &state.pending_effect_decisions,
            tool_activity: &state.tool_activity,
            tool_activity_tombstones: &state.tool_activity_tombstones,
            retired_tool_fence: &state.retired_tool_fence,
            terminal_turn_fence: &state.terminal_turn_fence,
            usage: state.usage,
            retained_items,
        },
    )
    .map_err(transition_view_error)
}

#[derive(Clone, Copy)]
enum TransitionError {
    Invalid,
    UsageOverflow,
    FenceSaturated,
}

const fn transition_view_error(error: TransitionError) -> ViewError {
    match error {
        TransitionError::FenceSaturated => ViewError::FenceSaturated,
        TransitionError::Invalid | TransitionError::UsageOverflow => ViewError::InvalidEvent,
    }
}

const fn transition_reducer_error(error: TransitionError) -> ReducerError {
    match error {
        TransitionError::UsageOverflow => ReducerError::UsageOverflow,
        TransitionError::FenceSaturated => ReducerError::FenceSaturated,
        TransitionError::Invalid => ReducerError::InvalidLifecycle,
    }
}

fn prepare_fence_key(
    fence: &LifetimeFence,
    identity: LifetimeFenceIdentity,
) -> Result<Vec<u8>, TransitionError> {
    if fence.refuses_continuation || fence.domain != identity.domain() {
        return Err(TransitionError::FenceSaturated);
    }
    let key = identity.stable_key();
    if !fence.identities.contains(&key) && fence.identities.len() >= MAX_LIFETIME_IDENTITIES {
        return Err(TransitionError::FenceSaturated);
    }
    Ok(key)
}

#[allow(
    clippy::too_many_lines,
    reason = "one exhaustive event-kind law is the compile-time transition authority"
)]
fn plan_transition(
    event: &AgentEvent,
    state: TransitionState<'_>,
) -> Result<TransitionPlan, TransitionError> {
    let turn_id = event.turn_id;
    match event.kind() {
        AgentEventKind::AgentClosed => Ok(TransitionPlan::Close),
        AgentEventKind::TurnStarted => Ok(TransitionPlan::TurnStarted {
            turn_id: turn_id.ok_or(TransitionError::Invalid)?,
        }),
        AgentEventKind::ModelStarted => {
            let turn_id = turn_id.ok_or(TransitionError::Invalid)?;
            let attempt_id = event.attempt_id.ok_or(TransitionError::Invalid)?;
            let mut turn = state.turn.cloned().ok_or(TransitionError::Invalid)?;
            turn.model_attempts = turn.model_attempts.saturating_add(1);
            Ok(TransitionPlan::TurnChanged {
                turn_id,
                turn,
                attempt: Some((turn_id, attempt_id)),
            })
        }
        AgentEventKind::ModelDispatchStarted
        | AgentEventKind::Model { .. }
        | AgentEventKind::RequestPrepared { .. } => {
            let turn_id = event.turn_id.ok_or(TransitionError::Invalid)?;
            let attempt_id = event.attempt_id.ok_or(TransitionError::Invalid)?;
            let mut turn = state.turn.cloned().ok_or(TransitionError::Invalid)?;
            if matches!(event.kind(), AgentEventKind::ModelDispatchStarted) {
                turn.live_usage_base = turn.live_usage;
            }
            if let AgentEventKind::Model {
                event: Some(model), ..
            } = event.kind()
                && let ModelEvent::Usage(update) = model.as_ref()
            {
                let base = turn.live_usage_base;
                turn.live_usage = saturating_token_sum(base, update.usage());
            }
            Ok(TransitionPlan::TurnChanged {
                turn_id,
                turn,
                attempt: Some((turn_id, attempt_id)),
            })
        }
        // Notes about the turn or about tool results that were already recorded; they move no
        // projected state.
        AgentEventKind::JobPromoted { .. }
        | AgentEventKind::JobFinished { .. }
        | AgentEventKind::OutputBoundRederived { .. }
        | AgentEventKind::SteeringQueued { .. }
        | AgentEventKind::SteeringApplied
        | AgentEventKind::OperationBudgetExhausted
        | AgentEventKind::ModelRequestQueued { .. }
        | AgentEventKind::OutputLimitRetry { .. }
        | AgentEventKind::ToolResultsReplaced { .. }
        | AgentEventKind::ObservationClipped { .. }
        | AgentEventKind::ObservationReplaced { .. }
        | AgentEventKind::CompactionStarted { .. }
        | AgentEventKind::CompactionFinished { .. } => Ok(TransitionPlan::Noop),
        AgentEventKind::EffectDecisionRequired {
            identity,
            decision,
            prompt,
            expires_at,
        } => {
            let attempt = (
                turn_id.ok_or(TransitionError::Invalid)?,
                event.attempt_id.ok_or(TransitionError::Invalid)?,
            );
            let candidate = (!state
                .pending_effect_decisions
                .iter()
                .any(|pending| pending.decision == *decision))
            .then(|| PendingEffectDecisionView {
                identity: identity.clone(),
                decision: decision.clone(),
                prompt: prompt.clone(),
                expires_at: *expires_at,
            });
            Ok(TransitionPlan::DecisionRequired {
                attempt,
                decision: candidate,
            })
        }
        AgentEventKind::EffectDecisionResolved { decision, .. } => {
            Ok(TransitionPlan::EffectDecisionResolved {
                decision: decision.clone(),
            })
        }
        AgentEventKind::ToolAuthorized { .. } | AgentEventKind::ToolPreparationRejected { .. } => {
            let turn_id = turn_id.ok_or(TransitionError::Invalid)?;
            let attempt_id = event.attempt_id.ok_or(TransitionError::Invalid)?;
            let mut turn = state.turn.cloned().ok_or(TransitionError::Invalid)?;
            if matches!(event.kind(), AgentEventKind::ToolAuthorized { .. }) {
                turn.tools_authorized = turn.tools_authorized.saturating_add(1);
            } else {
                turn.tools_completed = turn.tools_completed.saturating_add(1);
            }
            Ok(TransitionPlan::TurnChanged {
                turn_id,
                turn,
                attempt: Some((turn_id, attempt_id)),
            })
        }
        AgentEventKind::ToolStarted { identity, .. } => {
            let turn_id = turn_id.ok_or(TransitionError::Invalid)?;
            let attempt_id = event.attempt_id.ok_or(TransitionError::Invalid)?;
            let call_id = event.call_id.ok_or(TransitionError::Invalid)?;
            if let Some(existing) = state.tool_activity.iter().find(|item| {
                item.turn_id == turn_id && item.attempt_id == attempt_id && item.call_id == call_id
            }) {
                return if existing.identity == *identity {
                    Ok(TransitionPlan::Noop)
                } else {
                    Err(TransitionError::Invalid)
                };
            }
            if state.tool_activity_tombstones.iter().any(|item| {
                item.turn_id == turn_id && item.attempt_id == attempt_id && item.call_id == call_id
            }) || state
                .retired_tool_fence
                .contains(LifetimeFenceIdentity::ToolCall(
                    turn_id, attempt_id, call_id,
                ))
                || state.retained_items == 0
            {
                return Err(TransitionError::Invalid);
            }
            let mut turn = state.turn.cloned().ok_or(TransitionError::Invalid)?;
            turn.tools_started = turn.tools_started.saturating_add(1);
            Ok(TransitionPlan::ToolStarted {
                attempt: (turn_id, attempt_id),
                activity: ToolActivityView::started(turn_id, attempt_id, call_id, identity.clone()),
                turn_id,
                turn,
            })
        }
        AgentEventKind::ToolCompleted { identity, .. } => {
            let turn_id = turn_id.ok_or(TransitionError::Invalid)?;
            let attempt_id = event.attempt_id.ok_or(TransitionError::Invalid)?;
            let call_id = event.call_id.ok_or(TransitionError::Invalid)?;
            let index = state
                .tool_activity
                .iter()
                .position(|item| {
                    item.turn_id == turn_id
                        && item.attempt_id == attempt_id
                        && item.call_id == call_id
                })
                .ok_or(TransitionError::Invalid)?;
            let activity = &state.tool_activity[index];
            if activity.identity != *identity {
                return Err(TransitionError::Invalid);
            }
            let retired_key = prepare_fence_key(
                state.retired_tool_fence,
                LifetimeFenceIdentity::ToolCall(turn_id, attempt_id, call_id),
            )?;
            let mut turn = state.turn.cloned().ok_or(TransitionError::Invalid)?;
            turn.tools_completed = turn.tools_completed.saturating_add(1);
            Ok(TransitionPlan::ToolCompleted {
                index,
                tombstone: (state.retained_items != 0)
                    .then(|| ToolActivityTombstone::from_activity(activity.clone())),
                retired_key,
                retained_items: state.retained_items,
                turn_id,
                turn,
            })
        }
        AgentEventKind::ToolProgress {
            identity,
            summary,
            progress,
            ..
        } => {
            let turn_id = turn_id.ok_or(TransitionError::Invalid)?;
            let attempt_id = event.attempt_id.ok_or(TransitionError::Invalid)?;
            let call_id = event.call_id.ok_or(TransitionError::Invalid)?;
            let index = state
                .tool_activity
                .iter()
                .position(|item| {
                    item.turn_id == turn_id
                        && item.attempt_id == attempt_id
                        && item.call_id == call_id
                })
                .ok_or(TransitionError::Invalid)?;
            let mut activity = state.tool_activity[index].clone();
            let disposition = update_tool_activity(
                std::slice::from_mut(&mut activity),
                state.retired_tool_fence,
                turn_id,
                attempt_id,
                call_id,
                identity,
                *summary,
                progress
                    .as_ref()
                    .map(|progress| Arc::from(progress.message())),
            )
            .map_err(|_| TransitionError::Invalid)?;
            if disposition == ToolProgressDisposition::Applied {
                Ok(TransitionPlan::ToolProgress { index, activity })
            } else {
                Ok(TransitionPlan::Noop)
            }
        }
        AgentEventKind::ConversationCommitted { item_count, items } => {
            Ok(TransitionPlan::Conversation {
                item_count: *item_count,
                items: items.clone(),
                turn_id: turn_id.ok_or(TransitionError::Invalid)?,
            })
        }
        AgentEventKind::TurnCompleted { usage } | AgentEventKind::TurnFailed { usage, .. } => {
            let turn_id = turn_id.ok_or(TransitionError::Invalid)?;
            let mut turn = state
                .turn
                .cloned()
                .unwrap_or_else(|| TurnView::running(turn_id));
            match event.kind() {
                AgentEventKind::TurnCompleted { .. } => {
                    turn.state = TurnViewState::Completed;
                    turn.failure = None;
                }
                AgentEventKind::TurnFailed { kind, .. } => {
                    turn.state = TurnViewState::Failed;
                    turn.failure = Some(*kind);
                }
                _ => unreachable!(),
            }
            turn.usage = Some(*usage);
            turn.live_usage = usage.model();
            turn.live_usage_base = Usage::default();
            let mut aggregate = state.usage;
            aggregate
                .record_turn(*usage)
                .map_err(|_| TransitionError::UsageOverflow)?;
            let terminal_key = prepare_fence_key(
                state.terminal_turn_fence,
                LifetimeFenceIdentity::Turn(turn_id),
            )?;
            Ok(TransitionPlan::Terminal {
                turn_id,
                turn,
                usage: aggregate,
                terminal_key,
            })
        }
    }
}

fn validate_projection_for_bounded_delta(
    state: &ProjectionState,
    event: &AgentEvent,
    retained_items: usize,
) -> Result<(), ViewError> {
    if state.lifecycle == AgentViewState::Closed {
        return Err(ViewError::InvalidEvent);
    }
    if state.terminal_turn_fence.refuses_continuation
        || state.retired_tool_fence.refuses_continuation
    {
        return Err(ViewError::FenceStateUnavailable);
    }
    validate_projection_context(state, event, retained_items).map_err(|()| ViewError::InvalidEvent)
}

pub(super) fn apply_projection_delta(state: &mut ProjectionState, delta: TransitionPlan) {
    match delta {
        TransitionPlan::Close => {
            state.lifecycle = AgentViewState::Closed;
            state.recovery_promotable_turn = None;
            state.pending_effect_decisions.clear();
            state.tool_activity.clear();
            state.tool_activity_tombstones.clear();
            state.active_attempts.clear();
        }
        TransitionPlan::TurnStarted { turn_id } => {
            state.turns.insert(turn_id, TurnView::running(turn_id));
        }
        TransitionPlan::TurnChanged {
            turn_id,
            turn,
            attempt,
        } => {
            state.turns.insert(turn_id, turn);
            if let Some(attempt) = attempt {
                state.active_attempts.insert(attempt);
            }
        }
        TransitionPlan::DecisionRequired { attempt, decision } => {
            state.active_attempts.insert(attempt);
            if let Some(decision) = decision {
                state.pending_effect_decisions.push(decision);
            }
        }
        TransitionPlan::EffectDecisionResolved { decision } => {
            state
                .pending_effect_decisions
                .retain(|pending| pending.decision != decision);
        }
        TransitionPlan::ToolStarted {
            attempt,
            activity,
            turn_id,
            turn,
        } => {
            state.active_attempts.insert(attempt);
            state.turns.insert(turn_id, turn);
            state.tool_activity.push(activity);
        }
        TransitionPlan::ToolCompleted {
            index,
            tombstone,
            retired_key,
            retained_items,
            turn_id,
            turn,
        } => {
            state.tool_activity.remove(index);
            state.retired_tool_fence.identities.insert(retired_key);
            if let Some(tombstone) = tombstone {
                while state.tool_activity_tombstones.len() >= retained_items {
                    state.tool_activity_tombstones.pop_front();
                }
                state.tool_activity_tombstones.push_back(tombstone);
            }
            state.turns.insert(turn_id, turn);
        }
        TransitionPlan::ToolProgress { index, activity } => state.tool_activity[index] = activity,
        TransitionPlan::Conversation {
            item_count,
            items,
            turn_id,
        } => {
            state.committed_conversation_items = state
                .committed_conversation_items
                .saturating_add(item_count);
            if let Some(items) = items {
                state.conversation = state.conversation.appended_bounded(
                    items,
                    state.retained_conversation_items,
                    state.retained_conversation_bytes,
                );
            }
            if state.recovery_promotable_turn == Some(turn_id) {
                state.recovery_promotable_turn = None;
            }
        }
        TransitionPlan::Terminal {
            turn_id,
            turn,
            usage,
            terminal_key,
        } => {
            state.turns.insert(turn_id, turn);
            state.usage = usage;
            state
                .pending_effect_decisions
                .retain(|item| item.decision.turn_id() != turn_id);
            state.tool_activity.retain(|item| item.turn_id != turn_id);
            state
                .tool_activity_tombstones
                .retain(|item| item.turn_id != turn_id);
            state.active_attempts.retain(|(owner, _)| *owner != turn_id);
            state.terminal_turn_fence.identities.insert(terminal_key);
        }
        TransitionPlan::Noop => {}
    }
}

fn validate_snapshot_context(snapshot: &AgentViewSnapshot, event: &AgentEvent) -> Result<(), ()> {
    if !event_context_shape_is_valid(event) {
        return Err(());
    }
    if matches!(event.kind(), AgentEventKind::AgentClosed) {
        return Ok(());
    }
    let turn_id = event.turn_id.ok_or(())?;
    let turn = snapshot.turns.iter().find(|turn| turn.turn_id == turn_id);
    let active = turn.is_some_and(|turn| turn.state == TurnViewState::Running);
    match event.kind() {
        AgentEventKind::TurnStarted => {
            if snapshot
                .terminal_turn_fence
                .contains(LifetimeFenceIdentity::Turn(turn_id))
            {
                return Err(());
            }
            if turn.is_some()
                || snapshot
                    .turns
                    .iter()
                    .filter(|turn| turn.state == TurnViewState::Running)
                    .count()
                    >= snapshot.retained_turns
                || !terminal_fence_can_reserve(
                    &snapshot.terminal_turn_fence,
                    snapshot
                        .turns
                        .iter()
                        .filter(|turn| turn.state == TurnViewState::Running)
                        .map(|turn| turn.turn_id)
                        .chain(std::iter::once(turn_id)),
                )
            {
                return Err(());
            }
        }
        AgentEventKind::ConversationCommitted { .. } => {
            let completed = turn.is_some_and(|turn| turn.state != TurnViewState::Running)
                && snapshot.completed_turns.contains(&turn_id);
            if !active && !completed && snapshot.recovery_promotable_turn != Some(turn_id) {
                return Err(());
            }
        }
        AgentEventKind::TurnCompleted { .. } | AgentEventKind::TurnFailed { .. } => {
            if !active
                && (turn.is_some()
                    || snapshot
                        .terminal_turn_fence
                        .contains(LifetimeFenceIdentity::Turn(turn_id)))
            {
                return Err(());
            }
        }
        _ if !active => return Err(()),
        _ => {}
    }
    if event.attempt_id.is_some() {
        let binding = (turn_id, event.attempt_id.ok_or(())?);
        if !snapshot.active_attempts.contains(&binding)
            && matches!(
                event.kind(),
                AgentEventKind::EffectDecisionResolved { .. }
                    | AgentEventKind::ToolProgress { .. }
                    | AgentEventKind::ToolCompleted { .. }
            )
        {
            return Err(());
        }
    }
    Ok(())
}

fn preflight_snapshot(
    snapshot: &AgentViewSnapshot,
    event: &AgentEvent,
) -> Result<TransitionPlan, ReducerError> {
    validate_snapshot_for_bounded_delta(snapshot, event)?;
    plan_transition(
        event,
        TransitionState {
            turn: event
                .turn_id
                .and_then(|turn_id| snapshot.turns.iter().find(|turn| turn.turn_id == turn_id)),
            pending_effect_decisions: &snapshot.pending_effect_decisions,
            tool_activity: &snapshot.tool_activity,
            tool_activity_tombstones: &snapshot.tool_activity_tombstones,
            retired_tool_fence: &snapshot.retired_tool_fence,
            terminal_turn_fence: &snapshot.terminal_turn_fence,
            usage: snapshot.usage,
            retained_items: snapshot.retained_turns,
        },
    )
    .map_err(transition_reducer_error)
}
fn validate_snapshot_for_bounded_delta(
    snapshot: &AgentViewSnapshot,
    event: &AgentEvent,
) -> Result<(), ReducerError> {
    if snapshot.state == AgentViewState::Closed {
        return Err(ReducerError::InvalidLifecycle);
    }
    if snapshot.terminal_turn_fence.refuses_continuation
        || snapshot.retired_tool_fence.refuses_continuation
    {
        return Err(ReducerError::FenceSaturated);
    }
    validate_snapshot_context(snapshot, event).map_err(|()| ReducerError::InvalidLifecycle)
}

fn apply_snapshot_delta(snapshot: &mut AgentViewSnapshot, delta: TransitionPlan) {
    match delta {
        TransitionPlan::Close => {
            snapshot.state = AgentViewState::Closed;
            snapshot.recovery_promotable_turn = None;
            snapshot.pending_effect_decisions.clear();
            snapshot.tool_activity.clear();
            snapshot.tool_activity_tombstones.clear();
            snapshot.active_attempts.clear();
        }
        TransitionPlan::TurnStarted { turn_id } => {
            insert_snapshot_turn(&mut snapshot.turns, TurnView::running(turn_id));
        }
        TransitionPlan::TurnChanged { turn, attempt, .. } => {
            insert_snapshot_turn(&mut snapshot.turns, turn);
            if let Some(attempt) = attempt {
                snapshot.active_attempts.insert(attempt);
            }
        }
        TransitionPlan::DecisionRequired { attempt, decision } => {
            snapshot.active_attempts.insert(attempt);
            if let Some(decision) = decision {
                snapshot.pending_effect_decisions.push(decision);
            }
        }
        TransitionPlan::EffectDecisionResolved { decision } => {
            snapshot
                .pending_effect_decisions
                .retain(|pending| pending.decision != decision);
        }
        TransitionPlan::ToolStarted {
            attempt,
            activity,
            turn,
            ..
        } => {
            snapshot.active_attempts.insert(attempt);
            snapshot.tool_activity.push(activity);
            insert_snapshot_turn(&mut snapshot.turns, turn);
        }
        TransitionPlan::ToolCompleted {
            index,
            tombstone,
            retired_key,
            retained_items,
            turn,
            ..
        } => {
            snapshot.tool_activity.remove(index);
            snapshot.retired_tool_fence.identities.insert(retired_key);
            if let Some(tombstone) = tombstone {
                while snapshot.tool_activity_tombstones.len() >= retained_items {
                    snapshot.tool_activity_tombstones.pop_front();
                }
                snapshot.tool_activity_tombstones.push_back(tombstone);
            }
            insert_snapshot_turn(&mut snapshot.turns, turn);
        }
        TransitionPlan::ToolProgress { index, activity } => {
            snapshot.tool_activity[index] = activity;
        }
        TransitionPlan::Conversation {
            item_count,
            items,
            turn_id,
        } => {
            snapshot.committed_conversation_items = snapshot
                .committed_conversation_items
                .saturating_add(item_count);
            if let (Some(existing), Some(items)) = (&snapshot.conversation, items) {
                snapshot.conversation = Some(existing.appended_bounded(
                    items,
                    snapshot.retained_turns,
                    snapshot.retained_conversation_bytes,
                ));
            }
            if snapshot.recovery_promotable_turn == Some(turn_id) {
                snapshot.recovery_promotable_turn = None;
            }
        }
        TransitionPlan::Terminal {
            turn_id,
            turn,
            usage,
            terminal_key,
        } => {
            insert_snapshot_turn(&mut snapshot.turns, turn);
            snapshot.usage = usage;
            snapshot
                .pending_effect_decisions
                .retain(|item| item.decision.turn_id() != turn_id);
            snapshot
                .tool_activity
                .retain(|item| item.turn_id != turn_id);
            snapshot
                .tool_activity_tombstones
                .retain(|item| item.turn_id != turn_id);
            snapshot
                .active_attempts
                .retain(|(owner, _)| *owner != turn_id);
            snapshot.terminal_turn_fence.identities.insert(terminal_key);
            snapshot.completed_turns.push_back(turn_id);
            if snapshot.completed_turns.len() > snapshot.retained_turns
                && let Some(evicted) = snapshot.completed_turns.pop_front()
            {
                if let Ok(index) = snapshot
                    .turns
                    .binary_search_by_key(&evicted, TurnView::turn_id)
                {
                    snapshot.turns.remove(index);
                }
                snapshot.terminal_turn_tombstones.push_back(evicted);
                while snapshot.terminal_turn_tombstones.len() > snapshot.retained_turns {
                    snapshot.terminal_turn_tombstones.pop_front();
                }
            }
        }
        TransitionPlan::Noop => {}
    }
}
fn insert_snapshot_turn(turns: &mut Vec<TurnView>, turn: TurnView) {
    match turns.binary_search_by_key(&turn.turn_id, TurnView::turn_id) {
        Ok(index) => turns[index] = turn,
        Err(index) => turns.insert(index, turn),
    }
}

#[cfg(test)]
mod tests;
