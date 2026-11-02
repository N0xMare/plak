//! Versioned event vocabulary and redaction-safe envelopes.

mod kind;
mod tools;
pub use kind::AgentEventKind;
#[cfg(test)]
use kind::presentation_safe_model_event;
mod event;

use super::projection::ConversationItemView;
use crate::control::CompactionSummary;
use crate::session::SessionWorkflowId;
use crate::{AgentErrorKind, EffectDecisionRef, TurnUsage};
use plakous_model::{
    AttemptId, BlockStart, BlockStarted, BranchId, CallId, ContextInputParts, DataClassification,
    Digest, DriverInstanceId, EventId, ModelEvent, RunId, SessionId, Timestamp, ToolCallerLineage,
    ToolName, TurnId,
};
use plakous_tools::{
    EffectDecisionPrompt, RegisteredToolId, ToolExecutorId, ToolExposureBinding, ToolInvocation,
    ToolProgress, ToolRuntimeErrorKind,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::io::{self, Write};
use std::sync::Arc;
use thiserror::Error;

pub(super) const EVENT_SCHEMA_VERSION: u32 = 3;
pub(super) const PROJECTION_REVISION: u32 = 6;
pub(super) const EVENT_KIND_VERSION: u32 = 12;
const EVENT_ENVELOPE_BYTES: usize = 512;
pub(super) const MAX_LIFETIME_IDENTITIES: usize = 1 << 13;
pub(crate) const MAX_EVENT_DEBT_BYTES: usize = 256 * 1024;

/// Minimum estimated byte debt that can admit one structural event envelope.
pub const MIN_EVENT_DEBT_BYTES: usize = EVENT_ENVELOPE_BYTES;

/// A monotonic position within one process-local observation epoch.
#[derive(
    Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Deserialize, Serialize,
)]
pub struct EventSequence(pub(super) u64);

impl EventSequence {
    /// The initial snapshot boundary before any event has been published.
    pub const INITIAL: Self = Self(0);

    /// Returns the sequence value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Returns the next sequence, or `None` at numeric exhaustion.
    #[must_use]
    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(next) => Some(Self(next)),
            None => None,
        }
    }
}

/// Content visibility fixed for one snapshot/cursor/subscription scope.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum EventVisibility {
    /// Structural metadata only; prompt, model, decision, and conversation content is absent.
    #[default]
    Metadata,
    /// Presentation-safe content visible to the holder of the owning [`crate::Agent`] capability.
    Content,
}

/// Immutable selection scope bound into a delivered event and recovery cursor.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum EventScope {
    /// Agent-wide ordered stream used by snapshots and resumable views.
    Agent,
    /// One exact turn's filtered live feed.
    Turn(TurnId),
}

/// Nonzero item and byte debt limits for one event subscription.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubscriptionLimits {
    max_items: usize,
    max_bytes: usize,
}

impl SubscriptionLimits {
    /// Constructs explicit subscription bounds capable of admitting at least
    /// one structural event envelope.
    ///
    /// # Errors
    ///
    /// Returns a typed error when either bound cannot admit one event.
    pub const fn new(max_items: usize, max_bytes: usize) -> Result<Self, SubscriptionLimitError> {
        if max_items == 0 {
            return Err(SubscriptionLimitError::ZeroItems);
        }
        if max_bytes < MIN_EVENT_DEBT_BYTES {
            return Err(SubscriptionLimitError::ByteCapacityTooSmall {
                minimum: MIN_EVENT_DEBT_BYTES,
            });
        }
        Ok(Self {
            max_items,
            max_bytes,
        })
    }

    /// Returns the maximum pending matching event count.
    #[must_use]
    pub const fn max_items(self) -> usize {
        self.max_items
    }

    /// Returns the maximum pending projected byte estimate.
    #[must_use]
    pub const fn max_bytes(self) -> usize {
        self.max_bytes
    }
}

/// Invalid bounded-subscription configuration.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum SubscriptionLimitError {
    /// An item bound must admit at least one event.
    #[error("event subscription item capacity must be nonzero")]
    ZeroItems,
    /// The byte bound cannot admit even one estimated structural envelope.
    #[error("event subscription byte capacity must be at least {minimum}")]
    ByteCapacityTooSmall {
        /// Minimum accepted estimated byte capacity.
        minimum: usize,
    },
}

impl Default for SubscriptionLimits {
    fn default() -> Self {
        Self {
            max_items: 256,
            max_bytes: 8 * 1024 * 1024,
        }
    }
}

/// Request for an atomic presentation snapshot and live tail.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ViewRequest {
    pub(super) visibility: EventVisibility,
    pub(super) limits: SubscriptionLimits,
}

impl ViewRequest {
    /// Creates a request with explicit visibility and resource limits.
    #[must_use]
    pub const fn new(visibility: EventVisibility, limits: SubscriptionLimits) -> Self {
        Self { visibility, limits }
    }

    /// Creates a metadata-only request with default bounded limits.
    #[must_use]
    pub fn metadata() -> Self {
        Self::default()
    }

    /// Creates a content-visible request with default bounded limits.
    #[must_use]
    pub fn content() -> Self {
        Self {
            visibility: EventVisibility::Content,
            ..Self::default()
        }
    }

    /// Returns the fixed visibility grant.
    #[must_use]
    pub const fn visibility(self) -> EventVisibility {
        self.visibility
    }

    /// Returns the subscriber debt limits.
    #[must_use]
    pub const fn limits(self) -> SubscriptionLimits {
        self.limits
    }
}

/// Opaque recovery boundary bound to one compatible presentation scope.
#[derive(Clone, Copy, Eq, Hash, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EventCursor {
    pub(super) session_id: SessionId,
    pub(super) branch_id: BranchId,
    pub(super) run_id: RunId,
    pub(super) sequence: EventSequence,
    pub(super) event_schema: u32,
    pub(super) projection_revision: u32,
    pub(super) visibility: EventVisibility,
    pub(super) scope: EventScope,
}

impl EventCursor {
    /// Returns the session scope.
    #[must_use]
    pub const fn session_id(self) -> SessionId {
        self.session_id
    }

    /// Returns the branch scope.
    #[must_use]
    pub const fn branch_id(self) -> BranchId {
        self.branch_id
    }

    /// Returns the process-local observation epoch.
    #[must_use]
    pub const fn run_id(self) -> RunId {
        self.run_id
    }

    /// Returns the last applied sequence.
    #[must_use]
    pub const fn sequence(self) -> EventSequence {
        self.sequence
    }

    /// Returns the event-envelope schema version.
    #[must_use]
    pub const fn event_schema(self) -> u32 {
        self.event_schema
    }

    /// Returns the read-model projection revision.
    #[must_use]
    pub const fn projection_revision(self) -> u32 {
        self.projection_revision
    }

    /// Returns the cursor's fixed visibility scope.
    #[must_use]
    pub const fn visibility(self) -> EventVisibility {
        self.visibility
    }

    /// Returns the cursor's immutable selection scope.
    #[must_use]
    pub const fn scope(self) -> EventScope {
        self.scope
    }
}

impl fmt::Debug for EventCursor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EventCursor")
            .field("session_id", &self.session_id)
            .field("branch_id", &self.branch_id)
            .field("run_id", &self.run_id)
            .field("sequence", &self.sequence)
            .field("event_schema", &self.event_schema)
            .field("projection_revision", &self.projection_revision)
            .field("visibility", &self.visibility)
            .field("scope", &self.scope)
            .finish()
    }
}

/// Component that originated an event.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub enum EventOrigin {
    /// Agent lifecycle or semantic commit logic.
    Agent,
    /// Canonical model operation.
    Model,
    /// Tool policy or execution path.
    Tool,
    /// Exact-turn control path.
    Control,
}

/// Stable category of a normalized model event without its content.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub enum NormalizedModelEventKind {
    /// Bounded semantic diagnostic.
    Diagnostic,
    /// The model call will be sent again; see [`plakous_model::ModelRetry`].
    Retry,
    /// Response start.
    ResponseStarted,
    /// Output block start.
    BlockStarted,
    /// Assistant text delta.
    TextDelta,
    /// Reasoning delta.
    ReasoningDelta,
    /// Reasoning replay binding.
    ReasoningReplayBound,
    /// Reasoning visibility binding.
    ReasoningVisibilityBound,
    /// Tool-input delta.
    ToolInputDelta,
    /// Citation projection.
    CitationAdded,
    /// Output block completion.
    BlockCompleted,
    /// Usage update.
    Usage,
    /// Terminal response aggregate.
    ResponseCompleted,
    /// Forward-compatible provider event.
    ProviderEvent,
    /// A newer canonical event kind not understood by this crate revision.
    Unknown,
}

/// Backpressure class of one delivered event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum EventDeliveryClass {
    /// Ordered lifecycle or semantic structure which is never selectively dropped.
    Structural,
    /// A presentation-only model text, reasoning, or tool-input delta.
    ModelDelta,
    /// A presentation-only local tool progress update.
    ToolProgress,
}

impl From<&ModelEvent> for NormalizedModelEventKind {
    fn from(event: &ModelEvent) -> Self {
        match event {
            ModelEvent::Diagnostic(_) => Self::Diagnostic,
            ModelEvent::Retry(_) => Self::Retry,
            ModelEvent::ResponseStarted(_) => Self::ResponseStarted,
            ModelEvent::BlockStarted(_) => Self::BlockStarted,
            ModelEvent::TextDelta(_) => Self::TextDelta,
            ModelEvent::ReasoningDelta(_) => Self::ReasoningDelta,
            ModelEvent::ReasoningReplayBound(_) => Self::ReasoningReplayBound,
            ModelEvent::ReasoningVisibilityBound(_) => Self::ReasoningVisibilityBound,
            ModelEvent::ToolInputDelta(_) => Self::ToolInputDelta,
            ModelEvent::CitationAdded(_) => Self::CitationAdded,
            ModelEvent::BlockCompleted(_) => Self::BlockCompleted,
            ModelEvent::Usage(_) => Self::Usage,
            ModelEvent::ResponseCompleted(_) => Self::ResponseCompleted,
            ModelEvent::ProviderEvent(_) => Self::ProviderEvent,
            _ => Self::Unknown,
        }
    }
}

/// Terminal projection of one actor-owned pending effect-decision capability.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub enum EffectDecisionResolution {
    /// An exact operator decision minted a one-shot dispatch grant.
    Allowed,
    /// An exact operator denial prevented dispatch.
    Denied,
    /// The monotonic effect-decision deadline elapsed before a decision won.
    Expired,
    /// Exact-turn cancellation removed the pending capability.
    Cancelled,
    /// Agent shutdown removed the pending capability.
    Shutdown,
    /// Exact authority revalidation failed closed without minting a grant.
    Invalidated,
}

/// Bounded, redaction-safe identity evidence for one validated local tool invocation.
///
/// This value binds the canonical registered executor identity to the optional
/// provider-facing exposure selected before authorization. It deliberately
/// excludes invocation arguments, correlation values, decision content, and
/// executor outcomes.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct ToolAuditIdentity {
    canonical_name: ToolName,
    exposed_name: Option<ToolName>,
    exposure_map_digest: Option<Digest>,
    registered_tool_id: RegisteredToolId,
    executor_id: ToolExecutorId,
    definition_digest: Digest,
    schema_digest: Digest,
    effect_metadata_digest: Digest,
    argument_limits_digest: Digest,
}

#[derive(Deserialize, Serialize)]
struct ToolAuditIdentityWire {
    canonical_name: ToolName,
    exposed_name: Option<ToolName>,
    exposure_map_digest: Option<Digest>,
    registered_tool_id: String,
    executor_id: String,
    definition_digest: Digest,
    schema_digest: Digest,
    effect_metadata_digest: Digest,
    argument_limits_digest: Digest,
}

/// Closed caller class projected onto a tool lifecycle observation.
///
/// Mirrors the canonical [`plakous_model::ToolCallerLineage`] discriminants
/// without naming a provider, engine, or interpreter.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolCallerClass {
    /// The model called the configured tool directly.
    #[default]
    Direct,
    /// A provider-hosted program called the configured tool.
    ProviderProgrammatic,
    /// A Plakous-local evaluator called the tool through the private driver.
    LocalProgrammatic,
}

/// Closed, redaction-safe caller lineage retained on tool lifecycle events.
///
/// This is correlation, not content: it is retained at every visibility,
/// including metadata-only projections, exactly like [`ToolProgressSummary`].
/// It carries no invocation arguments, no result bytes, and no provider or
/// engine name. `Direct` is the default, so a non-programmatic call reports
/// [`ToolCallerClass::Direct`] with every other member absent.
///
/// The canonical [`plakous_model::ToolCallerLineage`] remains the authority;
/// `parent_call_id` is the one member the canonical value does not carry, and
/// it is supplied at publication time from the owning local-composition marker
/// call rather than added to canonical content.
#[derive(Clone, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCallerLineageView {
    class: ToolCallerClass,
    cell_id: Option<Arc<str>>,
    parent_call_id: Option<CallId>,
    nested_ordinal: Option<u32>,
    depth: Option<u16>,
}

/// Bounded identity evidence for a model-facing tool call rejected before validation.
///
/// The submitted name is not asserted to be registered, exposed, or executable.
/// When an exposure map was active, its digest identifies the exact model-facing
/// namespace against which the name was rejected without fabricating an exposure
/// binding or canonical executor identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolCallAttemptIdentity {
    model_facing_name: ToolName,
    exposure_map_digest: Option<Digest>,
}

/// Bounded, already-redacted presentation content for one validated tool call.
///
/// This observation is optional, carries no provider frame or credential
/// authority, and is removed from metadata-only event projections.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolCallObservation {
    subject: Option<Arc<str>>,
    arguments_preview: Arc<str>,
}

/// Bounded, already-redacted presentation content for one canonical tool result.
///
/// `subject` is the tool outcome's host-only presentation subject, copied by
/// the runtime. It is not read from the result text or from `preview`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolResultObservation {
    succeeded: bool,
    preview: Arc<str>,
    /// Host-only presentation subject supplied by the tool outcome.
    /// Absent when the tool set none, and omitted from the event encoding then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subject: Option<Arc<str>>,
}

/// Content-free ordered counters for one local tool progress update.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolProgressSummary {
    sequence: u64,
    timestamp: Timestamp,
    completed: Option<u64>,
    total: Option<u64>,
}

/// Exact bounded tool-progress content retained by a replayable agent event.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EventToolProgress {
    sequence: u64,
    call_id: CallId,
    timestamp: Timestamp,
    message: String,
    completed: Option<u64>,
    total: Option<u64>,
}

mod effect_decision_prompt_option_wire {
    use plakous_tools::EffectDecisionPrompt;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[allow(clippy::ref_option, reason = "Serde with-module serializer signature")]
    pub fn serialize<S>(
        prompt: &Option<EffectDecisionPrompt>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        prompt
            .as_ref()
            .map(|prompt| prompt.description().as_str())
            .serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<EffectDecisionPrompt>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<String>::deserialize(deserializer)
            .map(|prompt| prompt.map(EffectDecisionPrompt::new))
    }
}

mod tool_runtime_error_kind_wire {
    use plakous_tools::ToolRuntimeErrorKind;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Deserialize, Serialize)]
    enum Wire {
        Registration,
        InvalidDefinition,
        UnknownTool,
        InvalidArguments,
        Policy,
        EffectSafety,
        Cancelled,
        ExecutionTimeout,
        CleanupTimeout,
        WorkspacePolicy,
        Execution,
        ResourceLimit,
        ScriptExhausted,
    }

    #[allow(
        clippy::trivially_copy_pass_by_ref,
        reason = "Serde with-module serializer signature"
    )]
    pub fn serialize<S>(kind: &ToolRuntimeErrorKind, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let wire = match kind {
            ToolRuntimeErrorKind::Registration => Wire::Registration,
            ToolRuntimeErrorKind::InvalidDefinition => Wire::InvalidDefinition,
            ToolRuntimeErrorKind::UnknownTool => Wire::UnknownTool,
            ToolRuntimeErrorKind::InvalidArguments => Wire::InvalidArguments,
            ToolRuntimeErrorKind::Policy => Wire::Policy,
            ToolRuntimeErrorKind::EffectSafety => Wire::EffectSafety,
            ToolRuntimeErrorKind::Cancelled => Wire::Cancelled,
            ToolRuntimeErrorKind::ExecutionTimeout => Wire::ExecutionTimeout,
            ToolRuntimeErrorKind::CleanupTimeout => Wire::CleanupTimeout,
            ToolRuntimeErrorKind::WorkspacePolicy => Wire::WorkspacePolicy,
            ToolRuntimeErrorKind::Execution => Wire::Execution,
            ToolRuntimeErrorKind::ResourceLimit => Wire::ResourceLimit,
            ToolRuntimeErrorKind::ScriptExhausted => Wire::ScriptExhausted,
            _ => return Err(serde::ser::Error::custom("unknown tool runtime error kind")),
        };
        wire.serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<ToolRuntimeErrorKind, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match Wire::deserialize(deserializer)? {
            Wire::Registration => ToolRuntimeErrorKind::Registration,
            Wire::InvalidDefinition => ToolRuntimeErrorKind::InvalidDefinition,
            Wire::UnknownTool => ToolRuntimeErrorKind::UnknownTool,
            Wire::InvalidArguments => ToolRuntimeErrorKind::InvalidArguments,
            Wire::Policy => ToolRuntimeErrorKind::Policy,
            Wire::EffectSafety => ToolRuntimeErrorKind::EffectSafety,
            Wire::Cancelled => ToolRuntimeErrorKind::Cancelled,
            Wire::ExecutionTimeout => ToolRuntimeErrorKind::ExecutionTimeout,
            Wire::CleanupTimeout => ToolRuntimeErrorKind::CleanupTimeout,
            Wire::WorkspacePolicy => ToolRuntimeErrorKind::WorkspacePolicy,
            Wire::Execution => ToolRuntimeErrorKind::Execution,
            Wire::ResourceLimit => ToolRuntimeErrorKind::ResourceLimit,
            Wire::ScriptExhausted => ToolRuntimeErrorKind::ScriptExhausted,
        })
    }
}

/// Product-visible reason for a turn-scoped compaction observation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionEventReason {
    /// Unpaid image residency before ordinary compaction scheduling.
    Residency,
    /// Manual `/compact` or host `CompactSession`.
    Manual,
    /// Scheduled compact (idle fence (a) or tool-group fence (b)).
    Scheduled,
}

/// Product-visible terminal of a turn-scoped compaction observation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionEventOutcome {
    /// Successor checkpoint published.
    Committed,
    /// Paid compact finished without a semantic replacement.
    Failed,
    /// Compact evidence remains unresolved.
    RecoveryRequired,
    /// Occupancy did not require compact after admission.
    NotNeeded,
    /// Protected-group planner selected no replaceable prefix.
    NoEligibleRange,
    /// Preparation/admission failed before dispatch.
    Unavailable,
}

#[cfg(test)]
mod presentation_safety_tests;

/// A private-field, versioned causal event envelope.
#[derive(Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentEvent {
    pub(super) branch_sequence: EventSequence,
    pub(super) event_id: EventId,
    pub(super) session_id: SessionId,
    pub(super) branch_id: BranchId,
    pub(super) agent_id: DriverInstanceId,
    pub(super) run_id: RunId,
    pub(super) turn_id: Option<TurnId>,
    pub(super) attempt_id: Option<AttemptId>,
    pub(super) call_id: Option<CallId>,
    pub(super) feed_seq: Option<crate::feed::FeedSeq>,
    pub(super) timestamp: Timestamp,
    pub(super) causal_parent: Option<EventId>,
    pub(super) origin: EventOrigin,
    pub(super) classification: DataClassification,
    pub(super) event_schema: u32,
    pub(super) projection_revision: u32,
    pub(super) visibility: EventVisibility,
    pub(super) scope: EventScope,
    pub(super) event_kind_version: u32,
    pub(super) kind: AgentEventKind,
    pub(super) retained_bytes: usize,
    pub(super) metadata_retained_bytes: usize,
    pub(super) content_retained_bytes: usize,
}

/// Explicit class of presentation events omitted under bounded subscriber pressure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum EventLossKind {
    /// Only normalized model deltas were omitted.
    ModelDelta,
    /// Only local tool progress was omitted.
    ToolProgress,
    /// Both droppable classes occurred in one adjacent range.
    Mixed,
}

/// Exact adjacent source-sequence range omitted from one bounded live delivery.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EventLostRange {
    pub(super) after: EventCursor,
    pub(super) through: EventSequence,
    pub(super) kind: EventLossKind,
}

impl EventLostRange {
    /// Returns the last cursor incorporated before the lost range.
    #[must_use]
    pub const fn after(self) -> EventCursor {
        self.after
    }

    /// Returns the first omitted sequence.
    #[must_use]
    pub const fn first(self) -> EventSequence {
        EventSequence(self.after.sequence.0.saturating_add(1))
    }

    /// Returns the last omitted sequence, inclusively.
    #[must_use]
    pub const fn through(self) -> EventSequence {
        self.through
    }

    /// Returns the omitted presentation class.
    #[must_use]
    pub const fn kind(self) -> EventLossKind {
        self.kind
    }

    /// Returns the cursor after applying this loss record.
    #[must_use]
    pub const fn cursor(self) -> EventCursor {
        EventCursor {
            sequence: self.through,
            ..self.after
        }
    }
}

/// One delivery from a bounded event subscription.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum EventDelivery {
    /// One exact projected source event.
    Event(Arc<AgentEvent>),
    /// An exact adjacent range of presentation-only events omitted under pressure.
    Lost(EventLostRange),
}

pub(super) fn event_metadata(kind: &AgentEventKind) -> (EventOrigin, DataClassification) {
    match kind {
        AgentEventKind::ModelStarted
        | AgentEventKind::ModelDispatchStarted
        | AgentEventKind::ResponseSettled { .. } => {
            (EventOrigin::Model, DataClassification::Public)
        }
        AgentEventKind::RequestPrepared { .. } | AgentEventKind::Model { .. } => {
            (EventOrigin::Model, DataClassification::Sensitive)
        }
        AgentEventKind::EffectDecisionRequired { .. }
        | AgentEventKind::ToolProgress { .. }
        | AgentEventKind::ToolStarted { .. }
        | AgentEventKind::ToolCompleted { .. } => {
            (EventOrigin::Tool, DataClassification::Sensitive)
        }
        AgentEventKind::EffectDecisionResolved { .. }
        | AgentEventKind::ToolAuthorized { .. }
        | AgentEventKind::ToolPreparationRejected { .. } => {
            (EventOrigin::Tool, DataClassification::Public)
        }
        AgentEventKind::SteeringQueued { .. } | AgentEventKind::SteeringApplied => {
            (EventOrigin::Control, DataClassification::Public)
        }
        AgentEventKind::ConversationCommitted { .. } => {
            (EventOrigin::Agent, DataClassification::Sensitive)
        }
        // Counts, budgets and tool names only: agent-owned and public.
        AgentEventKind::OperationBudgetExhausted
        | AgentEventKind::ModelRequestQueued { .. }
        | AgentEventKind::ModelCredentialWait { .. }
        | AgentEventKind::OutputLimitRetry { .. }
        | AgentEventKind::ToolResultsReplaced { .. }
        | AgentEventKind::ObservationClipped { .. }
        | AgentEventKind::ObservationReplaced { .. }
        | AgentEventKind::CompactionStarted { .. }
        | AgentEventKind::JobPromoted { .. }
        | AgentEventKind::JobFinished { .. }
        | AgentEventKind::OutputBoundRederived { .. }
        | AgentEventKind::CompactionFinished { .. } => {
            (EventOrigin::Agent, DataClassification::Public)
        }
        AgentEventKind::AgentClosed
        | AgentEventKind::TurnStarted
        | AgentEventKind::TurnCompleted { .. }
        | AgentEventKind::TurnFailed { .. } => (EventOrigin::Agent, DataClassification::Public),
    }
}

fn serialized_size<T: Serialize + ?Sized>(value: &T) -> usize {
    let mut counter = CountingWriter::default();
    if serde_json::to_writer(&mut counter, value).is_err() {
        usize::MAX
    } else {
        counter.bytes
    }
}

#[derive(Default)]
struct CountingWriter {
    bytes: usize,
}

impl Write for CountingWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.bytes = self.bytes.saturating_add(buffer.len());
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod retry_logging_tests {
    use super::*;

    #[test]
    fn ephemeral_retry_request_strings_spend_live_window_bytes() {
        let retry = plakous_model::ModelRetry::new(
            2,
            4,
            std::time::Duration::ZERO,
            plakous_model::ModelRetryCause::RecordRejectedInTransit,
            false,
            "peer_rejected_record",
        );
        let plain = AgentEventKind::model(ModelEvent::Retry(retry.clone()));
        let logged = AgentEventKind::model(ModelEvent::Retry(retry.with_request(
            "fixture.invalid",
            "/messages",
            123,
        )));
        assert_eq!(
            logged.retained_bytes() - plain.retained_bytes(),
            "fixture.invalid".len() + "/messages".len()
        );
    }
}
