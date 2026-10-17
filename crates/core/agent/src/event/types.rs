//! Versioned event vocabulary and redaction-safe envelopes.

use super::projection::ConversationItemView;
use crate::control::CompactionSummary;
use crate::session::SessionWorkflowId;
use crate::{AgentErrorKind, EffectDecisionRef, TurnUsage};
use plakous_model::{
    AgentId, AttemptId, BlockStart, BlockStarted, BranchId, CallId, ContextInputParts,
    DataClassification, Digest, EventId, ModelEvent, RunId, SessionId, Timestamp,
    ToolCallerLineage, ToolName, TurnId,
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
pub(super) const EVENT_KIND_VERSION: u32 = 11;
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

impl ToolAuditIdentity {
    /// Captures immutable audit evidence from an identity-bound, validated invocation.
    #[must_use]
    pub fn from_invocation(invocation: &ToolInvocation) -> Self {
        let registered = invocation.identity();
        let exposure = invocation.exposure();
        Self {
            canonical_name: registered.name().clone(),
            exposed_name: exposure.map(|binding| binding.exposed_name().clone()),
            exposure_map_digest: exposure.map(ToolExposureBinding::map_digest),
            registered_tool_id: registered.tool_id().clone(),
            executor_id: registered.executor_id().clone(),
            definition_digest: registered.definition_digest(),
            schema_digest: registered.schema_digest(),
            effect_metadata_digest: registered.effect_metadata_digest(),
            argument_limits_digest: registered.argument_limits_digest(),
        }
    }

    /// Returns the canonical registered model-facing name.
    #[must_use]
    pub const fn canonical_name(&self) -> &ToolName {
        &self.canonical_name
    }

    /// Returns the provider-facing alias, when an exposure map prepared the invocation.
    #[must_use]
    pub const fn exposed_name(&self) -> Option<&ToolName> {
        self.exposed_name.as_ref()
    }

    /// Returns the exact frozen exposure-map digest, when one selected the invocation.
    #[must_use]
    pub const fn exposure_map_digest(&self) -> Option<Digest> {
        self.exposure_map_digest
    }

    /// Returns the stable application-scoped registered tool identity.
    #[must_use]
    pub const fn registered_tool_id(&self) -> &RegisteredToolId {
        &self.registered_tool_id
    }

    /// Returns the stable executable implementation identity.
    #[must_use]
    pub const fn executor_id(&self) -> &ToolExecutorId {
        &self.executor_id
    }

    /// Returns the complete canonical tool-definition digest.
    #[must_use]
    pub const fn definition_digest(&self) -> Digest {
        self.definition_digest
    }

    /// Returns the canonical parameter-schema digest.
    #[must_use]
    pub const fn schema_digest(&self) -> Digest {
        self.schema_digest
    }

    /// Returns the digest binding the tool effect class and recovery semantics.
    #[must_use]
    pub const fn effect_metadata_digest(&self) -> Digest {
        self.effect_metadata_digest
    }

    /// Returns the digest binding every validated argument-limit field.
    #[must_use]
    pub const fn argument_limits_digest(&self) -> Digest {
        self.argument_limits_digest
    }

    fn retained_bytes(&self) -> usize {
        self.canonical_name
            .as_str()
            .len()
            .saturating_add(
                self.exposed_name
                    .as_ref()
                    .map_or(0, |name| name.as_str().len()),
            )
            .saturating_add(self.registered_tool_id.as_str().len())
            .saturating_add(self.executor_id.as_str().len())
            .saturating_add(size_of::<Digest>().saturating_mul(
                if self.exposure_map_digest.is_some() {
                    5
                } else {
                    4
                },
            ))
    }
}

impl fmt::Debug for ToolAuditIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolAuditIdentity")
            .field("canonical_name", &self.canonical_name)
            .field("exposed_name", &self.exposed_name)
            .field("exposure_map_digest", &self.exposure_map_digest)
            .field("registered_tool_id", &self.registered_tool_id)
            .field("executor_id", &self.executor_id)
            .field("definition_digest", &self.definition_digest)
            .field("schema_digest", &self.schema_digest)
            .field("effect_metadata_digest", &self.effect_metadata_digest)
            .field("argument_limits_digest", &self.argument_limits_digest)
            .finish()
    }
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

impl Serialize for ToolAuditIdentity {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        ToolAuditIdentityWire {
            canonical_name: self.canonical_name.clone(),
            exposed_name: self.exposed_name.clone(),
            exposure_map_digest: self.exposure_map_digest,
            registered_tool_id: self.registered_tool_id.as_str().to_owned(),
            executor_id: self.executor_id.as_str().to_owned(),
            definition_digest: self.definition_digest,
            schema_digest: self.schema_digest,
            effect_metadata_digest: self.effect_metadata_digest,
            argument_limits_digest: self.argument_limits_digest,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ToolAuditIdentity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ToolAuditIdentityWire::deserialize(deserializer)?;
        Ok(Self {
            canonical_name: wire.canonical_name,
            exposed_name: wire.exposed_name,
            exposure_map_digest: wire.exposure_map_digest,
            registered_tool_id: RegisteredToolId::new(wire.registered_tool_id)
                .map_err(serde::de::Error::custom)?,
            executor_id: ToolExecutorId::new(wire.executor_id).map_err(serde::de::Error::custom)?,
            definition_digest: wire.definition_digest,
            schema_digest: wire.schema_digest,
            effect_metadata_digest: wire.effect_metadata_digest,
            argument_limits_digest: wire.argument_limits_digest,
        })
    }
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

impl ToolCallerLineageView {
    /// Returns the lineage of a call the model issued directly.
    #[must_use]
    pub fn direct() -> Self {
        Self::default()
    }

    /// Projects the canonical caller lineage minted on the child call.
    ///
    /// `parent_call_id` names the outer local-composition marker call that owns
    /// the evaluator cell; it is `None` for a direct call, and `None` for a
    /// provider-hosted program that does not name an owning call.
    #[must_use]
    pub fn from_lineage(lineage: &ToolCallerLineage, parent_call_id: Option<CallId>) -> Self {
        match lineage {
            ToolCallerLineage::ProviderProgrammatic { execution_id, .. } => Self {
                class: ToolCallerClass::ProviderProgrammatic,
                cell_id: Some(Arc::from(execution_id.as_str())),
                parent_call_id,
                nested_ordinal: None,
                depth: None,
            },
            ToolCallerLineage::LocalProgrammatic {
                cell_id,
                request_sequence,
                depth,
                ..
            } => Self {
                class: ToolCallerClass::LocalProgrammatic,
                cell_id: Some(Arc::from(cell_id.as_str())),
                parent_call_id,
                nested_ordinal: Some(*request_sequence),
                depth: Some(*depth),
            },
            ToolCallerLineage::Direct => Self::direct(),
        }
    }

    /// Returns the lineage of an outer local-composition marker call.
    ///
    /// The marker owns a cell rather than belonging to one, so it carries no
    /// parent, no ordinal, and no depth. `cell_id` is the driver's active cell
    /// when the interpreter has already minted one.
    #[must_use]
    pub fn local_composition_marker(cell_id: Option<&str>) -> Self {
        Self {
            class: ToolCallerClass::LocalProgrammatic,
            cell_id: cell_id.map(Arc::from),
            parent_call_id: None,
            nested_ordinal: None,
            depth: None,
        }
    }

    /// Returns the closed caller class.
    #[must_use]
    pub const fn class(&self) -> ToolCallerClass {
        self.class
    }

    /// Returns the evaluator cell, or the hosted execution identity.
    #[must_use]
    pub fn cell_id(&self) -> Option<&str> {
        self.cell_id.as_deref()
    }

    /// Returns the outer marker call that owns the cell, when nested.
    #[must_use]
    pub const fn parent_call_id(&self) -> Option<CallId> {
        self.parent_call_id
    }

    /// Returns the one-based acceptance sequence within the owning cell.
    #[must_use]
    pub const fn nested_ordinal(&self) -> Option<u32> {
        self.nested_ordinal
    }

    /// Returns the independent local-composition recursion depth.
    #[must_use]
    pub const fn depth(&self) -> Option<u16> {
        self.depth
    }

    /// Returns `true` when the call is the outer marker owning an evaluator cell.
    #[must_use]
    pub const fn is_composition_marker(&self) -> bool {
        matches!(self.class, ToolCallerClass::LocalProgrammatic) && self.parent_call_id.is_none()
    }

    fn retained_bytes(&self) -> usize {
        self.cell_id.as_deref().map_or(0, str::len)
    }
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

impl ToolCallObservation {
    pub(crate) fn new(subject: Option<String>, arguments_preview: String) -> Self {
        Self {
            subject: subject.map(Arc::from),
            arguments_preview: Arc::from(arguments_preview),
        }
    }

    /// Returns the compact redacted subject, when one could be derived.
    #[must_use]
    pub fn subject(&self) -> Option<&str> {
        self.subject.as_deref()
    }

    /// Returns bounded redacted arguments suitable for presentation.
    #[must_use]
    pub fn arguments_preview(&self) -> &str {
        &self.arguments_preview
    }

    fn retained_bytes(&self) -> usize {
        self.subject.as_ref().map_or(0, |value| value.len()) + self.arguments_preview.len()
    }
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

impl ToolResultObservation {
    pub(crate) fn new(succeeded: bool, preview: String, subject: Option<String>) -> Self {
        Self {
            succeeded,
            preview: Arc::from(preview),
            subject: subject.map(Arc::from),
        }
    }

    /// Whether the canonical model-visible result reports success.
    #[must_use]
    pub const fn succeeded(&self) -> bool {
        self.succeeded
    }

    /// Returns the bounded redacted result preview.
    #[must_use]
    pub fn preview(&self) -> &str {
        &self.preview
    }

    /// Returns the tool-provided presentation subject, when this result has one.
    #[must_use]
    pub fn subject(&self) -> Option<&str> {
        self.subject.as_deref()
    }

    fn retained_bytes(&self) -> usize {
        self.preview
            .len()
            .saturating_add(self.subject.as_ref().map_or(0, |value| value.len()))
    }
}

impl ToolCallAttemptIdentity {
    pub(crate) const fn new(
        model_facing_name: ToolName,
        exposure_map_digest: Option<Digest>,
    ) -> Self {
        Self {
            model_facing_name,
            exposure_map_digest,
        }
    }

    /// Returns the bounded name submitted by the model without asserting local identity.
    #[must_use]
    pub const fn model_facing_name(&self) -> &ToolName {
        &self.model_facing_name
    }

    /// Returns the exact active exposure-map digest, when calls used an exposed namespace.
    #[must_use]
    pub const fn exposure_map_digest(&self) -> Option<Digest> {
        self.exposure_map_digest
    }

    fn retained_bytes(&self) -> usize {
        self.model_facing_name
            .as_str()
            .len()
            .saturating_add(self.exposure_map_digest.map_or(0, |_| size_of::<Digest>()))
    }
}

/// Content-free ordered counters for one local tool progress update.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolProgressSummary {
    sequence: u64,
    timestamp: Timestamp,
    completed: Option<u64>,
    total: Option<u64>,
}

impl ToolProgressSummary {
    /// Creates exact ordered content-free progress counters.
    #[must_use]
    pub const fn new(
        sequence: u64,
        timestamp: Timestamp,
        completed: Option<u64>,
        total: Option<u64>,
    ) -> Self {
        Self {
            sequence,
            timestamp,
            completed,
            total,
        }
    }

    /// Captures the safe counters from one bounded progress update.
    #[must_use]
    pub fn from_progress(progress: &ToolProgress) -> Self {
        Self {
            sequence: progress.sequence(),
            timestamp: progress.timestamp(),
            completed: progress.completed(),
            total: progress.total(),
        }
    }

    pub(super) fn from_event_progress(progress: &EventToolProgress) -> Self {
        Self {
            sequence: progress.sequence(),
            timestamp: progress.timestamp(),
            completed: progress.completed(),
            total: progress.total(),
        }
    }

    /// Returns the monotonically increasing per-call sequence.
    #[must_use]
    pub const fn sequence(self) -> u64 {
        self.sequence
    }

    /// Returns the injected event timestamp.
    #[must_use]
    pub const fn timestamp(self) -> Timestamp {
        self.timestamp
    }

    /// Returns completed units when reported.
    #[must_use]
    pub const fn completed(self) -> Option<u64> {
        self.completed
    }

    /// Returns total units when reported.
    #[must_use]
    pub const fn total(self) -> Option<u64> {
        self.total
    }
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

impl EventToolProgress {
    /// Returns the monotonically increasing per-call sequence.
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Returns the correlated canonical call identity.
    #[must_use]
    pub const fn call_id(&self) -> CallId {
        self.call_id
    }

    /// Returns the injected event timestamp.
    #[must_use]
    pub const fn timestamp(&self) -> Timestamp {
        self.timestamp
    }

    /// Returns bounded model-independent progress text.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns completed units when reported.
    #[must_use]
    pub const fn completed(&self) -> Option<u64> {
        self.completed
    }

    /// Returns total units when reported.
    #[must_use]
    pub const fn total(&self) -> Option<u64> {
        self.total
    }
}

impl From<&ToolProgress> for EventToolProgress {
    fn from(progress: &ToolProgress) -> Self {
        Self {
            sequence: progress.sequence(),
            call_id: progress.call_id(),
            timestamp: progress.timestamp(),
            message: progress.message().to_owned(),
            completed: progress.completed(),
            total: progress.total(),
        }
    }
}

impl PartialEq<ToolProgress> for EventToolProgress {
    fn eq(&self, other: &ToolProgress) -> bool {
        self.sequence == other.sequence()
            && self.call_id == other.call_id()
            && self.timestamp == other.timestamp()
            && self.message == other.message()
            && self.completed == other.completed()
            && self.total == other.total()
    }
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

/// Typed data carried by one causal event envelope.
#[derive(Clone, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub enum AgentEventKind {
    /// Hint that a process became session-owned; query `Agent::jobs` for state.
    JobPromoted {
        /// Vendor handle and process job number.
        number: u32,
    },
    /// Hint that a process finished; canonical job rows are delivery authority.
    JobFinished {
        /// Vendor handle and process job number.
        number: u32,
    },
    /// The owning agent event source reached its ordered terminal boundary.
    AgentClosed,
    /// A reopen applied a freshly derived output bound in place of the one the
    /// session was created under.
    ///
    /// A bound the product derives from a route's attestation moves when the
    /// provider's catalog moves. Reopen re-derives it rather than refusing the
    /// session, and says so here: the session's output contract changed, and a
    /// change to what the model may produce belongs in the transcript.
    OutputBoundRederived {
        /// The bound the session was created under.
        previous: Option<std::num::NonZeroU32>,
        /// The bound this reopen derived and will use.
        current: Option<std::num::NonZeroU32>,
    },
    /// The driver accepted a turn from its FIFO queue.
    TurnStarted,
    /// One canonical model operation began.
    ModelStarted,
    /// The admitted provider request is about to receive its first poll.
    /// Preparation and authority revalidation have completed.
    ModelDispatchStarted,
    /// Measured parts and configured tool names of the request just prepared.
    RequestPrepared {
        /// Measured byte partitions of the lowered request, when available.
        input_parts: Option<ContextInputParts>,
        /// Occupancy on the runtime context scale.
        occupied_tokens: Option<u64>,
        /// Complete request admission upper bound.
        prepared_input_upper_bound: Option<u64>,
        /// Exact configured names, removed from metadata-only projection.
        tool_names: Vec<ToolName>,
    },
    /// One normalized model event arrived.
    Model {
        /// Stable structural category retained under metadata projection.
        kind: NormalizedModelEventKind,
        /// Canonical content, absent from metadata-only projections.
        event: Option<Box<ModelEvent>>,
    },
    /// Policy paused one exact tool call for an operator decision.
    EffectDecisionRequired {
        /// Immutable identity of the validated invocation awaiting decision.
        identity: ToolAuditIdentity,
        /// Binding-digest-addressed decision reference.
        decision: EffectDecisionRef,
        /// Descriptive content, absent from metadata-only projections.
        #[serde(with = "effect_decision_prompt_option_wire")]
        prompt: Option<EffectDecisionPrompt>,
        /// Display timestamp corresponding to the monotonic expiry deadline.
        expires_at: Timestamp,
    },
    /// A pending decision reached a terminal resolution.
    EffectDecisionResolved {
        /// Exact identity captured by the corresponding required event.
        identity: ToolAuditIdentity,
        /// Exact reference originally published by the required event.
        decision: EffectDecisionRef,
        /// Terminal resolution.
        resolution: EffectDecisionResolution,
    },
    /// A complete local tool call passed the policy/decision boundary.
    ToolAuthorized {
        /// Immutable registered and provider-facing audit identity.
        identity: ToolAuditIdentity,
        /// Closed caller lineage retained at every visibility.
        lineage: ToolCallerLineageView,
    },
    /// An authorized local tool call crossed its exact dispatch boundary.
    ToolStarted {
        /// Exact identity captured at authorization.
        identity: ToolAuditIdentity,
        /// Closed caller lineage retained at every visibility.
        lineage: ToolCallerLineageView,
        /// Bounded redacted arguments, present only under content visibility.
        observation: Option<ToolCallObservation>,
    },
    /// A bounded local tool progress update.
    ToolProgress {
        /// Exact identity captured at authorization.
        identity: ToolAuditIdentity,
        /// Closed caller lineage retained at every visibility.
        lineage: ToolCallerLineageView,
        /// Content-free ordered progress counters retained at every visibility.
        summary: ToolProgressSummary,
        /// Progress content, absent from metadata-only projections.
        progress: Option<EventToolProgress>,
    },
    /// A model-visible tool rejection terminalized before a validated invocation existed.
    ToolPreparationRejected {
        /// Attempted model-facing identity without a canonical/executable identity claim.
        attempt: ToolCallAttemptIdentity,
        /// Closed caller lineage retained at every visibility.
        lineage: ToolCallerLineageView,

        /// Stable redaction-safe preparation failure category.
        #[serde(with = "tool_runtime_error_kind_wire")]
        reason: ToolRuntimeErrorKind,
        /// The rejection's own descriptive message, bounded and redacted.
        ///
        /// The category alone does not diagnose a rejection: every schema
        /// refusal is `InvalidArguments`, while the message says which
        /// constraint the call missed. Present only under content visibility.
        detail: Option<Arc<str>>,
        /// Bounded redacted subject and arguments of the refused call.
        ///
        /// A rejected call never starts, so nothing else in the stream carries
        /// what it sent. Present only under content visibility.
        observation: Option<ToolCallObservation>,
    },
    /// A validated local tool call produced a canonical model-visible result or rejection.
    ToolCompleted {
        /// Exact validated identity retained throughout the call lifecycle.
        identity: ToolAuditIdentity,
        /// Closed caller lineage retained at every visibility.
        lineage: ToolCallerLineageView,
        /// Bounded redacted outcome, present only under content visibility.
        observation: Option<ToolResultObservation>,
    },
    /// Steering was admitted into the active turn's bounded queue.
    SteeringQueued {
        /// One-based position at admission time.
        position: usize,
    },
    /// Steering was committed at a safe boundary.
    SteeringApplied,
    /// A model request is waiting for a slot in a shared request budget.
    ///
    /// Published once per wait, before the request's operation deadline
    /// starts; the request proceeds with `ModelDispatchStarted` once a slot
    /// frees. `slots` is the host's label for the budget.
    ModelRequestQueued {
        /// The host-supplied label of the budget the request waits on.
        slots: String,
    },
    /// The turn spent its model-operation budget and is wrapping up.
    ///
    /// Published when the runtime injects its budget note and before the one
    /// further round it runs with tool selection withdrawn. A surface that
    /// renders this tells the reader why the assistant stopped exploring; a
    /// turn that ends without it ended for some other reason.
    OperationBudgetExhausted,
    /// A response reached its output bound before its local tool call was
    /// complete, and the turn is retrying.
    ///
    /// Published when the runtime discards the truncated response, which ran
    /// nothing, and injects the note that tells the model to retry with less
    /// output per call. `bound` is the request's output bound, when it sent one.
    /// A surface that renders this explains why the streamed text it showed
    /// did not become part of the conversation.
    OutputLimitRetry {
        /// The per-response output bound the truncated request carried.
        bound: Option<std::num::NonZeroU32>,
    },
    /// Tool-result bodies were replaced so the turn could keep going.
    ///
    /// Published by the third compaction rung, which replaces the oldest
    /// completed tool-result bodies of the current turn with bounded
    /// placeholders when neither the provider-native facet nor the semantic
    /// fold can act. A surface that renders this tells the reader why earlier
    /// tool output is no longer quoted verbatim.
    ToolResultsReplaced {
        /// Tool results whose bodies were replaced.
        results: u32,
        /// Total bytes of the replaced bodies.
        bytes: u64,
        /// Unanchored whole-request estimate before the placeholder pass (the
        /// measured bound when the request has no parts).
        tokens_before: u64,
        /// Unanchored whole-request estimate after the placeholder pass, on
        /// the same basis as `tokens_before`.
        tokens_after: u64,
    },
    /// Observes the observation budget truncating one tool result.
    ///
    /// A clipped result changes what the model can answer from, and until this
    /// existed the only trace was the model's own prose: a surface saw a tool
    /// succeed and had no way to know its output had been cut. `bound` is the
    /// budget this result was measured against, which is what makes
    /// `bytes_dropped` mean anything to a reader.
    ObservationClipped {
        /// Tool whose observation was truncated.
        name: Arc<str>,
        /// Observation bytes that survived into canonical history.
        bytes_kept: u64,
        /// Observation bytes the budget removed.
        bytes_dropped: u64,
        /// Bytes still available to this result before it was clipped.
        bound: u64,
    },
    /// A committed next-round capacity fallback replaced one tool result body.
    ///
    /// These are bytes in the already bounded canonical result, so they must
    /// not be added to raw-result `ObservationClipped` byte counts.
    ObservationReplaced {
        /// Text bytes in the canonical result before replacement.
        bytes_before: u64,
        /// Text bytes in the committed replacement result.
        bytes_after: u64,
    },
    /// Conversation items became semantically committed.
    ConversationCommitted {
        /// Number of newly committed ordered items.
        item_count: usize,
        /// Canonical items, absent from metadata-only projections.
        items: Option<Arc<[ConversationItemView]>>,
    },
    /// Observes compaction starting on an active turn (tool-group boundary).
    CompactionStarted {
        /// Exact workflow identity shared with the change feed.
        workflow_id: SessionWorkflowId,
        /// Scheduled vs manual compact.
        reason: CompactionEventReason,
    },
    /// Observes compaction reaching a product-visible terminal on an active turn.
    CompactionFinished {
        /// Workflow identity from the matching started observation.
        workflow_id: SessionWorkflowId,
        /// Compact terminal classification.
        outcome: CompactionEventOutcome,
        /// Elapsed milliseconds from the matching started observation.
        elapsed_ms: u64,
        /// What a committed compaction did, when the successor records it.
        ///
        /// Always `None` for a non-committed terminal: there is nothing to
        /// report about a compaction that published no successor.
        summary: Option<CompactionSummary>,
        /// Redacted operator reason for a non-committed terminal.
        ///
        /// A `failed` or `unavailable` compaction that says only *that* it
        /// failed is undiagnosable from the log alone; the live growth runs of
        /// 2026-09-18 produced exactly that. `None` is for the outcomes that
        /// have nothing to explain.
        detail: Option<Arc<str>>,
    },
    /// Observes a completed turn-result boundary without replacing that result.
    TurnCompleted {
        /// Complete checked usage for the logical turn.
        usage: TurnUsage,
    },
    /// Observes a failed or cancelled turn-result boundary without terminal authority.
    TurnFailed {
        /// Redaction-safe category only; details arrive through the result.
        kind: AgentErrorKind,
        /// Complete checked usage observed before failure.
        usage: TurnUsage,
    },
}

impl AgentEventKind {
    /// Constructs a content-bearing normalized model event.
    #[must_use]
    pub fn model(event: ModelEvent) -> Self {
        Self::Model {
            kind: NormalizedModelEventKind::from(&event),
            event: Some(Box::new(event)),
        }
    }

    /// Returns a presentation-safe normalized model event when visible.
    #[must_use]
    pub fn model_event(&self) -> Option<&ModelEvent> {
        match self {
            Self::Model { event, .. } => event.as_deref(),
            _ => None,
        }
    }

    /// Returns immutable identity evidence for a validated tool lifecycle event.
    #[must_use]
    pub const fn tool_audit_identity(&self) -> Option<&ToolAuditIdentity> {
        match self {
            Self::EffectDecisionRequired { identity, .. }
            | Self::EffectDecisionResolved { identity, .. }
            | Self::ToolAuthorized { identity, .. }
            | Self::ToolStarted { identity, .. }
            | Self::ToolProgress { identity, .. }
            | Self::ToolCompleted { identity, .. } => Some(identity),
            _ => None,
        }
    }

    /// Returns the caller lineage retained on a tool lifecycle event.
    #[must_use]
    pub const fn tool_caller_lineage(&self) -> Option<&ToolCallerLineageView> {
        match self {
            Self::ToolAuthorized { lineage, .. }
            | Self::ToolStarted { lineage, .. }
            | Self::ToolProgress { lineage, .. }
            | Self::ToolPreparationRejected { lineage, .. }
            | Self::ToolCompleted { lineage, .. } => Some(lineage),
            _ => None,
        }
    }

    /// Returns attempted identity evidence for a pre-validation terminal rejection.
    #[must_use]
    pub const fn tool_call_attempt_identity(&self) -> Option<&ToolCallAttemptIdentity> {
        match self {
            Self::ToolPreparationRejected { attempt, .. } => Some(attempt),
            _ => None,
        }
    }

    pub(super) fn projected(&self, visibility: EventVisibility) -> Self {
        match self {
            Self::JobPromoted { number } => Self::JobPromoted { number: *number },
            Self::JobFinished { number } => Self::JobFinished { number: *number },
            // Two integers naming a budget: nothing to redact at any visibility.
            Self::OutputBoundRederived { previous, current } => Self::OutputBoundRederived {
                previous: *previous,
                current: *current,
            },
            Self::Model { kind, event } => Self::Model {
                kind: *kind,
                event: if visibility == EventVisibility::Content {
                    event
                        .as_deref()
                        .and_then(presentation_safe_model_event)
                        .map(Box::new)
                } else {
                    None
                },
            },
            Self::EffectDecisionRequired {
                identity,
                decision,
                prompt,
                expires_at,
            } => Self::EffectDecisionRequired {
                identity: identity.clone(),
                decision: decision.clone(),
                prompt: (visibility == EventVisibility::Content)
                    .then(|| prompt.clone())
                    .flatten(),
                expires_at: *expires_at,
            },
            Self::ConversationCommitted { item_count, items } => Self::ConversationCommitted {
                item_count: *item_count,
                items: (visibility == EventVisibility::Content)
                    .then(|| items.clone())
                    .flatten(),
            },
            Self::EffectDecisionResolved {
                identity,
                decision,
                resolution,
            } => Self::EffectDecisionResolved {
                identity: identity.clone(),
                decision: decision.clone(),
                resolution: *resolution,
            },
            Self::SteeringQueued { position } => Self::SteeringQueued {
                position: *position,
            },
            Self::ToolAuthorized { identity, lineage } => Self::ToolAuthorized {
                identity: identity.clone(),
                lineage: lineage.clone(),
            },
            Self::ToolStarted {
                identity,
                lineage,
                observation,
            } => Self::ToolStarted {
                identity: identity.clone(),
                lineage: lineage.clone(),
                observation: (visibility == EventVisibility::Content)
                    .then(|| observation.clone())
                    .flatten(),
            },
            Self::ToolProgress {
                identity,
                lineage,
                summary,
                progress,
            } => Self::ToolProgress {
                identity: identity.clone(),
                lineage: lineage.clone(),
                summary: *summary,
                progress: (visibility == EventVisibility::Content)
                    .then(|| progress.clone())
                    .flatten(),
            },
            Self::ToolPreparationRejected {
                attempt,
                lineage,
                reason,
                detail,
                observation,
            } => Self::ToolPreparationRejected {
                attempt: attempt.clone(),
                lineage: lineage.clone(),
                reason: *reason,
                detail: (visibility == EventVisibility::Content)
                    .then(|| detail.clone())
                    .flatten(),
                observation: (visibility == EventVisibility::Content)
                    .then(|| observation.clone())
                    .flatten(),
            },
            Self::ToolCompleted {
                identity,
                lineage,
                observation,
            } => Self::ToolCompleted {
                identity: identity.clone(),
                lineage: lineage.clone(),
                observation: (visibility == EventVisibility::Content)
                    .then(|| observation.clone())
                    .flatten(),
            },
            Self::TurnFailed { kind, usage } => Self::TurnFailed {
                kind: *kind,
                usage: *usage,
            },
            Self::AgentClosed => Self::AgentClosed,
            Self::TurnStarted => Self::TurnStarted,
            Self::ModelStarted => Self::ModelStarted,
            Self::ModelDispatchStarted => Self::ModelDispatchStarted,
            Self::RequestPrepared {
                input_parts,
                occupied_tokens,
                prepared_input_upper_bound,
                tool_names,
            } => Self::RequestPrepared {
                input_parts: *input_parts,
                occupied_tokens: *occupied_tokens,
                prepared_input_upper_bound: *prepared_input_upper_bound,
                tool_names: if visibility == EventVisibility::Content {
                    tool_names.clone()
                } else {
                    Vec::new()
                },
            },
            Self::SteeringApplied => Self::SteeringApplied,
            Self::OperationBudgetExhausted => Self::OperationBudgetExhausted,
            Self::ModelRequestQueued { slots } => Self::ModelRequestQueued {
                slots: slots.clone(),
            },
            Self::OutputLimitRetry { bound } => Self::OutputLimitRetry { bound: *bound },
            Self::ToolResultsReplaced {
                results,
                bytes,
                tokens_before,
                tokens_after,
            } => Self::ToolResultsReplaced {
                results: *results,
                bytes: *bytes,
                tokens_before: *tokens_before,
                tokens_after: *tokens_after,
            },
            Self::ObservationClipped {
                name,
                bytes_kept,
                bytes_dropped,
                bound,
            } => Self::ObservationClipped {
                name: Arc::clone(name),
                bytes_kept: *bytes_kept,
                bytes_dropped: *bytes_dropped,
                bound: *bound,
            },
            Self::ObservationReplaced {
                bytes_before,
                bytes_after,
            } => Self::ObservationReplaced {
                bytes_before: *bytes_before,
                bytes_after: *bytes_after,
            },
            Self::CompactionStarted {
                workflow_id,
                reason,
            } => Self::CompactionStarted {
                workflow_id: *workflow_id,
                reason: *reason,
            },
            Self::CompactionFinished {
                workflow_id,
                outcome,
                elapsed_ms,
                summary,
                detail,
            } => Self::CompactionFinished {
                workflow_id: *workflow_id,
                outcome: *outcome,
                elapsed_ms: *elapsed_ms,
                summary: *summary,
                detail: detail.clone(),
            },
            Self::TurnCompleted { usage } => Self::TurnCompleted { usage: *usage },
        }
    }

    pub(super) fn retained_bytes(&self) -> usize {
        let content = match self {
            Self::Model {
                event: Some(event), ..
            } => {
                // Retry logging facts are skipped on the wire but still own
                // strings while the event is retained in a live window.
                let logging_bytes = match event.as_ref() {
                    ModelEvent::Retry(retry) => retry
                        .request()
                        .map_or(0, |(host, route, _)| host.len().saturating_add(route.len())),
                    _ => 0,
                };
                serialized_size(event.as_ref()).saturating_add(logging_bytes)
            }
            Self::EffectDecisionRequired {
                identity,
                prompt: Some(prompt),
                ..
            } => identity
                .retained_bytes()
                .saturating_add(prompt.description().as_str().len()),
            Self::ConversationCommitted {
                items: Some(items), ..
            } => items.iter().fold(0_usize, |total, item| {
                total.saturating_add(
                    128_usize.saturating_add(item.display_text().map_or(0, str::len)),
                )
            }),
            Self::ToolStarted {
                identity,
                observation: Some(observation),
                ..
            } => identity
                .retained_bytes()
                .saturating_add(observation.retained_bytes()),
            Self::ToolCompleted {
                identity,
                observation: Some(observation),
                ..
            } => identity
                .retained_bytes()
                .saturating_add(observation.retained_bytes()),
            Self::ToolProgress {
                identity,
                progress: Some(progress),
                ..
            } => identity
                .retained_bytes()
                .saturating_add(progress.message().len()),
            Self::ToolAuthorized { identity, .. }
            | Self::EffectDecisionRequired {
                identity,
                prompt: None,
                ..
            }
            | Self::EffectDecisionResolved { identity, .. }
            | Self::ToolStarted {
                identity,
                observation: None,
                ..
            }
            | Self::ToolProgress {
                identity,
                progress: None,
                ..
            }
            | Self::ToolCompleted {
                identity,
                observation: None,
                ..
            } => identity.retained_bytes(),
            Self::ToolPreparationRejected {
                attempt,
                detail,
                observation,
                ..
            } => attempt
                .retained_bytes()
                .saturating_add(detail.as_ref().map_or(0, |detail| detail.len()))
                .saturating_add(
                    observation
                        .as_ref()
                        .map_or(0, ToolCallObservation::retained_bytes),
                ),
            Self::RequestPrepared { tool_names, .. } => {
                128 + tool_names
                    .iter()
                    .map(|name| name.as_str().len())
                    .sum::<usize>()
            }
            // Job notes carry two optional integers, retained inline.
            Self::AgentClosed
            | Self::JobPromoted { .. }
            | Self::JobFinished { .. }
            | Self::OutputBoundRederived { .. }
            | Self::TurnStarted
            | Self::ModelStarted
            | Self::ModelDispatchStarted
            | Self::Model { event: None, .. }
            | Self::SteeringQueued { .. }
            | Self::SteeringApplied
            | Self::OperationBudgetExhausted
            | Self::OutputLimitRetry { .. }
            | Self::ToolResultsReplaced { .. }
            | Self::ConversationCommitted { items: None, .. }
            | Self::ObservationClipped { .. }
            | Self::ObservationReplaced { .. }
            | Self::CompactionStarted { .. }
            | Self::TurnCompleted { .. }
            | Self::TurnFailed { .. } => 0,
            Self::ModelRequestQueued { slots } => slots.len(),
            Self::CompactionFinished { detail, .. } => {
                detail.as_ref().map_or(0, |detail| detail.len())
            }
        };
        let lineage = self
            .tool_caller_lineage()
            .map_or(0, ToolCallerLineageView::retained_bytes);
        EVENT_ENVELOPE_BYTES
            .saturating_add(content)
            .saturating_add(lineage)
    }
}

/// The presentation-safe projection of one model event, or `None` when a
/// Content view may see nothing of it.
///
/// Block boundaries are structural: a Content consumer needs `BlockStarted` to
/// attribute reasoning deltas to the visibility the provider declared for that
/// location and `BlockCompleted` to seal what it showed. They
/// pass with replay material stripped. A complete block, the completed
/// response, and replay bindings carry provider replay/opaque material and
/// never leave the runtime.
fn presentation_safe_model_event(event: &ModelEvent) -> Option<ModelEvent> {
    match event {
        ModelEvent::Diagnostic(_)
        | ModelEvent::Retry(_)
        | ModelEvent::ResponseStarted(_)
        | ModelEvent::TextDelta(_)
        | ModelEvent::ReasoningDelta(_)
        | ModelEvent::ReasoningVisibilityBound(_)
        | ModelEvent::ToolInputDelta(_)
        | ModelEvent::CitationAdded(_)
        | ModelEvent::Usage(_)
        | ModelEvent::BlockCompleted(_) => Some(event.clone()),
        ModelEvent::BlockStarted(started) => {
            let block = match started.block() {
                BlockStart::Text { phase } => BlockStart::Text { phase: *phase },
                BlockStart::Reasoning { visibility, .. } => BlockStart::Reasoning {
                    visibility: *visibility,
                    replay: None,
                },
                BlockStart::ReasoningPending { .. } => {
                    BlockStart::ReasoningPending { replay: None }
                }
                BlockStart::ToolCall { .. } => started.block().clone(),
                BlockStart::Refusal => BlockStart::Refusal,
                // A complete block and any block kind this crate does not
                // know carry content it cannot vouch for.
                BlockStart::Complete(_) | _ => return None,
            };
            Some(ModelEvent::BlockStarted(BlockStarted::new(
                started.item_id(),
                started.location(),
                block,
            )))
        }
        _ => None,
    }
}

#[cfg(test)]
mod presentation_safety_tests {
    use plakous_model::{
        BlockStart, BlockStarted, ContentBlock, EntryId, ModelEvent, OutputLocation,
        ReasoningVisibility, TextBlock,
    };
    use uuid::Uuid;

    use super::{AgentEventKind, EventVisibility};

    #[test]
    fn stream_activity_is_not_a_presentation_event() {
        let event = ModelEvent::StreamActivity(plakous_model::ModelStreamActivity::default());
        assert!(super::presentation_safe_model_event(&event).is_none());
    }

    #[test]
    fn prompt_cache_diagnosis_is_not_a_presentation_event() {
        let event = ModelEvent::PromptCacheDiagnosis(plakous_model::PromptCacheDiagnosis {
            reason: plakous_model::PromptCacheMissReason::ToolsChanged,
            missed_input_tokens: Some(4),
        });
        assert!(super::presentation_safe_model_event(&event).is_none());
    }

    #[test]
    fn prepared_request_names_are_content_only() -> Result<(), Box<dyn std::error::Error>> {
        let event = AgentEventKind::RequestPrepared {
            input_parts: None,
            occupied_tokens: Some(23),
            prepared_input_upper_bound: Some(42),
            tool_names: vec![plakous_model::ToolName::new("read_file")?],
        };
        let AgentEventKind::RequestPrepared { tool_names, .. } =
            event.projected(EventVisibility::Content)
        else {
            panic!("content projection must keep the request shape");
        };
        assert_eq!(tool_names.len(), 1);
        let AgentEventKind::RequestPrepared {
            tool_names,
            occupied_tokens,
            ..
        } = event.projected(EventVisibility::Metadata)
        else {
            panic!("metadata projection must keep the request shape");
        };
        assert_eq!(tool_names, [] as [plakous_model::ToolName; 0]);
        assert_eq!(occupied_tokens, Some(23));
        Ok(())
    }

    fn started(block: BlockStart) -> Result<ModelEvent, Box<dyn std::error::Error>> {
        Ok(ModelEvent::BlockStarted(BlockStarted::new(
            EntryId::try_from(Uuid::from_u128(7))?,
            OutputLocation::new(0, 0),
            block,
        )))
    }

    #[test]
    fn a_content_view_retains_transient_history_projection_identity()
    -> Result<(), Box<dyn std::error::Error>> {
        let identity = plakous_model::Digest::sha256(
            plakous_model::DigestDomain::HistoryProjection,
            1,
            &[b"transition"],
        )?;
        let event = AgentEventKind::model(ModelEvent::Diagnostic(
            plakous_model::ModelDiagnostic::historical_projection(identity, "history projected"),
        ));
        let content = event.projected(EventVisibility::Content);
        assert!(matches!(
            content.model_event(),
            Some(ModelEvent::Diagnostic(diagnostic))
                if diagnostic.history_projection_identity() == Some(identity)
        ));
        assert!(
            event
                .projected(EventVisibility::Metadata)
                .model_event()
                .is_none()
        );
        Ok(())
    }

    /// The host attributes reasoning deltas to the visibility declared at
    /// `BlockStarted`, so a Content view must carry the start
    /// and its visibility, without any replay envelope.
    #[test]
    fn a_content_view_keeps_reasoning_block_starts_and_their_visibility()
    -> Result<(), Box<dyn std::error::Error>> {
        let event = AgentEventKind::model(started(BlockStart::Reasoning {
            visibility: ReasoningVisibility::Summary,
            replay: None,
        })?)
        .projected(EventVisibility::Content);
        let Some(ModelEvent::BlockStarted(kept)) = event.model_event() else {
            panic!("block start is presentation-safe: {event:?}");
        };
        assert_eq!(
            kept.block(),
            &BlockStart::Reasoning {
                visibility: ReasoningVisibility::Summary,
                replay: None,
            }
        );
        let pending =
            AgentEventKind::model(started(BlockStart::ReasoningPending { replay: None })?)
                .projected(EventVisibility::Content);
        assert!(matches!(
            pending.model_event(),
            Some(ModelEvent::BlockStarted(kept))
                if kept.block() == &BlockStart::ReasoningPending { replay: None }
        ));
        Ok(())
    }

    #[test]
    fn a_content_view_never_carries_complete_blocks_and_metadata_carries_no_model_event()
    -> Result<(), Box<dyn std::error::Error>> {
        let complete = started(BlockStart::Complete(ContentBlock::Text(TextBlock::new(
            "done", None,
        )?)))?;
        assert!(
            AgentEventKind::model(complete.clone())
                .projected(EventVisibility::Content)
                .model_event()
                .is_none()
        );
        assert!(
            AgentEventKind::model(complete)
                .projected(EventVisibility::Metadata)
                .model_event()
                .is_none()
        );
        Ok(())
    }
}

impl fmt::Debug for AgentEventKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut event = formatter.debug_struct("AgentEventKind");
        match self {
            Self::AgentClosed => event.field("kind", &"agent_closed"),
            Self::JobPromoted { number } => {
                event.field("kind", &"job_promoted").field("number", number)
            }
            Self::JobFinished { number } => {
                event.field("kind", &"job_finished").field("number", number)
            }
            Self::OutputBoundRederived { previous, current } => event
                .field("kind", &"output_bound_rederived")
                .field("previous", previous)
                .field("current", current),
            Self::TurnStarted => event.field("kind", &"turn_started"),
            Self::ModelStarted => event.field("kind", &"model_started"),
            Self::ModelDispatchStarted => event.field("kind", &"model_dispatch_started"),
            Self::RequestPrepared { tool_names, .. } => event
                .field("kind", &"request_prepared")
                .field("tool_count", &tool_names.len()),
            Self::Model { kind, .. } => event
                .field("kind", &"model_event")
                .field("model_event_kind", kind),
            Self::EffectDecisionRequired {
                identity,
                decision,
                expires_at,
                ..
            } => event
                .field("kind", &"tool_decision_required")
                .field("identity", identity)
                .field("decision", decision)
                .field("expires_at", expires_at),
            Self::EffectDecisionResolved {
                identity,
                decision,
                resolution,
            } => event
                .field("kind", &"tool_decision_resolved")
                .field("identity", identity)
                .field("decision", decision)
                .field("resolution", resolution),
            Self::ToolAuthorized { identity, lineage } => event
                .field("kind", &"tool_authorized")
                .field("identity", identity)
                .field("lineage", lineage),
            Self::ToolStarted {
                identity, lineage, ..
            } => event
                .field("kind", &"tool_started")
                .field("identity", identity)
                .field("lineage", lineage),
            Self::ToolProgress {
                identity,
                lineage,
                progress,
                ..
            } => event
                .field("kind", &"tool_progress")
                .field("identity", identity)
                .field("lineage", lineage)
                .field("progress", progress),
            Self::ToolPreparationRejected {
                attempt,
                lineage,
                reason,
                ..
            } => event
                .field("kind", &"tool_preparation_rejected")
                .field("attempt", attempt)
                .field("lineage", lineage)
                .field("reason", reason)
                .field("detail", &"<redacted>")
                .field("observation", &"<redacted>"),
            Self::ToolCompleted {
                identity, lineage, ..
            } => event
                .field("kind", &"tool_completed")
                .field("identity", identity)
                .field("lineage", lineage),
            Self::SteeringQueued { position } => event
                .field("kind", &"steering_queued")
                .field("position", position),
            Self::SteeringApplied => event.field("kind", &"steering_applied"),
            Self::OperationBudgetExhausted => event.field("kind", &"operation_budget_exhausted"),
            Self::ModelRequestQueued { slots } => event
                .field("kind", &"model_request_queued")
                .field("slots", slots),
            Self::OutputLimitRetry { bound } => event
                .field("kind", &"output_limit_retry")
                .field("bound", bound),
            Self::ToolResultsReplaced {
                results,
                bytes,
                tokens_before,
                tokens_after,
            } => event
                .field("kind", &"context_placeholders")
                .field("results", results)
                .field("bytes", bytes)
                .field("tokens_before", tokens_before)
                .field("tokens_after", tokens_after),
            Self::ObservationClipped {
                name,
                bytes_kept,
                bytes_dropped,
                bound,
            } => event
                .field("kind", &"observation_clipped")
                .field("name", name)
                .field("bytes_kept", bytes_kept)
                .field("bytes_dropped", bytes_dropped)
                .field("bound", bound),
            Self::ObservationReplaced {
                bytes_before,
                bytes_after,
            } => event
                .field("kind", &"observation_replaced")
                .field("bytes_before", bytes_before)
                .field("bytes_after", bytes_after),
            Self::ConversationCommitted { item_count, .. } => event
                .field("kind", &"conversation_committed")
                .field("item_count", item_count)
                .field("items", &"<redacted>"),
            Self::CompactionStarted {
                workflow_id,
                reason,
            } => event
                .field("kind", &"compaction_started")
                .field("workflow_id", workflow_id)
                .field("reason", reason),
            Self::CompactionFinished {
                workflow_id,
                outcome,
                elapsed_ms,
                summary,
                detail,
            } => event
                .field("kind", &"compaction_finished")
                .field("workflow_id", workflow_id)
                .field("outcome", outcome)
                .field("elapsed_ms", elapsed_ms)
                .field("summary", summary)
                .field("detail", &detail.is_some()),
            Self::TurnCompleted { usage } => {
                event.field("kind", &"turn_completed").field("usage", usage)
            }
            Self::TurnFailed { kind, usage } => event
                .field("kind", &"turn_failed")
                .field("error_kind", kind)
                .field("usage", usage),
        }
        .finish()
    }
}

/// A private-field, versioned causal event envelope.
#[derive(Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentEvent {
    pub(super) branch_sequence: EventSequence,
    pub(super) event_id: EventId,
    pub(super) session_id: SessionId,
    pub(super) branch_id: BranchId,
    pub(super) agent_id: AgentId,
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

impl AgentEvent {
    /// Returns the branch sequence.
    #[must_use]
    pub const fn branch_sequence(&self) -> EventSequence {
        self.branch_sequence
    }

    /// Returns the unique event identity.
    #[must_use]
    pub const fn event_id(&self) -> EventId {
        self.event_id
    }

    /// Returns the session identity.
    #[must_use]
    pub const fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// Returns the branch identity.
    #[must_use]
    pub const fn branch_id(&self) -> BranchId {
        self.branch_id
    }

    /// Returns the agent identity.
    #[must_use]
    pub const fn agent_id(&self) -> AgentId {
        self.agent_id
    }

    /// Returns the run identity.
    #[must_use]
    pub const fn run_id(&self) -> RunId {
        self.run_id
    }

    /// Returns the logical turn identity when applicable.
    #[must_use]
    pub const fn turn_id(&self) -> Option<TurnId> {
        self.turn_id
    }

    /// Returns the model attempt identity when applicable.
    #[must_use]
    pub const fn attempt_id(&self) -> Option<AttemptId> {
        self.attempt_id
    }

    /// Returns the tool-call identity when applicable.
    #[must_use]
    pub const fn call_id(&self) -> Option<CallId> {
        self.call_id
    }

    /// Returns the sequence of the change-feed record of the commit this
    /// event reports (a turn terminal, a committed compaction), when a
    /// durable store committed it. Observation only: the feed is authority.
    #[must_use]
    pub const fn feed_seq(&self) -> Option<crate::feed::FeedSeq> {
        self.feed_seq
    }

    /// Returns the observation timestamp.
    #[must_use]
    pub const fn timestamp(&self) -> Timestamp {
        self.timestamp
    }

    /// Returns the closest causal predecessor when one exists.
    #[must_use]
    pub const fn causal_parent(&self) -> Option<EventId> {
        self.causal_parent
    }

    /// Returns the originating component.
    #[must_use]
    pub const fn origin(&self) -> EventOrigin {
        self.origin
    }

    /// Returns the data-sensitivity classification.
    #[must_use]
    pub const fn classification(&self) -> DataClassification {
        self.classification
    }

    /// Returns the envelope schema version.
    #[must_use]
    pub const fn event_schema(&self) -> u32 {
        self.event_schema
    }

    /// Returns the projection policy revision.
    #[must_use]
    pub const fn projection_revision(&self) -> u32 {
        self.projection_revision
    }

    /// Returns the immutable visibility scope of this delivered projection.
    #[must_use]
    pub const fn visibility(&self) -> EventVisibility {
        self.visibility
    }

    /// Returns the immutable selection scope of this delivered projection.
    #[must_use]
    pub const fn scope(&self) -> EventScope {
        self.scope
    }

    /// Returns the event-kind schema version.
    #[must_use]
    pub const fn event_kind_version(&self) -> u32 {
        self.event_kind_version
    }

    /// Returns the typed payload.
    #[must_use]
    pub const fn kind(&self) -> &AgentEventKind {
        &self.kind
    }

    /// Returns the explicit backpressure class for this event.
    #[must_use]
    pub const fn delivery_class(&self) -> EventDeliveryClass {
        match &self.kind {
            AgentEventKind::Model {
                kind:
                    NormalizedModelEventKind::TextDelta
                    | NormalizedModelEventKind::ReasoningDelta
                    | NormalizedModelEventKind::ToolInputDelta,
                ..
            } => EventDeliveryClass::ModelDelta,
            AgentEventKind::ToolProgress { .. } => EventDeliveryClass::ToolProgress,
            AgentEventKind::AgentClosed
            | AgentEventKind::JobPromoted { .. }
            | AgentEventKind::JobFinished { .. }
            | AgentEventKind::OutputBoundRederived { .. }
            | AgentEventKind::TurnStarted
            | AgentEventKind::ModelStarted
            | AgentEventKind::ModelDispatchStarted
            | AgentEventKind::RequestPrepared { .. }
            | AgentEventKind::Model { .. }
            | AgentEventKind::EffectDecisionRequired { .. }
            | AgentEventKind::EffectDecisionResolved { .. }
            | AgentEventKind::ToolAuthorized { .. }
            | AgentEventKind::ToolStarted { .. }
            | AgentEventKind::ToolPreparationRejected { .. }
            | AgentEventKind::ToolCompleted { .. }
            | AgentEventKind::SteeringQueued { .. }
            | AgentEventKind::SteeringApplied
            | AgentEventKind::OperationBudgetExhausted
            | AgentEventKind::ModelRequestQueued { .. }
            | AgentEventKind::OutputLimitRetry { .. }
            | AgentEventKind::ToolResultsReplaced { .. }
            | AgentEventKind::ObservationClipped { .. }
            | AgentEventKind::ObservationReplaced { .. }
            | AgentEventKind::ConversationCommitted { .. }
            | AgentEventKind::CompactionStarted { .. }
            | AgentEventKind::CompactionFinished { .. }
            | AgentEventKind::TurnCompleted { .. }
            | AgentEventKind::TurnFailed { .. } => EventDeliveryClass::Structural,
        }
    }

    /// Returns a cursor through this event in its immutable projection scope.
    #[must_use]
    pub const fn cursor(&self) -> EventCursor {
        EventCursor {
            session_id: self.session_id,
            branch_id: self.branch_id,
            run_id: self.run_id,
            sequence: self.branch_sequence,
            event_schema: self.event_schema,
            projection_revision: self.projection_revision,
            visibility: self.visibility,
            scope: self.scope,
        }
    }

    pub(super) fn projected(&self, visibility: EventVisibility, scope: EventScope) -> Arc<Self> {
        let kind = self.kind.projected(visibility);
        let retained_bytes = kind.retained_bytes();
        // Spelled out so the source kind is not cloned only to be replaced.
        Arc::new(Self {
            branch_sequence: self.branch_sequence,
            event_id: self.event_id,
            session_id: self.session_id,
            branch_id: self.branch_id,
            agent_id: self.agent_id,
            run_id: self.run_id,
            turn_id: self.turn_id,
            attempt_id: self.attempt_id,
            call_id: self.call_id,
            feed_seq: self.feed_seq,
            timestamp: self.timestamp,
            causal_parent: self.causal_parent,
            origin: self.origin,
            classification: self.classification,
            event_schema: self.event_schema,
            projection_revision: self.projection_revision,
            visibility,
            scope,
            event_kind_version: self.event_kind_version,
            kind,
            retained_bytes,
            metadata_retained_bytes: self.metadata_retained_bytes,
            content_retained_bytes: self.content_retained_bytes,
        })
    }

    pub(super) const fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    pub(super) const fn projected_retained_bytes(&self, visibility: EventVisibility) -> usize {
        match visibility {
            EventVisibility::Metadata => self.metadata_retained_bytes,
            EventVisibility::Content => self.content_retained_bytes,
        }
    }
}

impl fmt::Debug for AgentEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentEvent")
            .field("branch_sequence", &self.branch_sequence)
            .field("event_id", &self.event_id)
            .field("session_id", &self.session_id)
            .field("branch_id", &self.branch_id)
            .field("agent_id", &self.agent_id)
            .field("run_id", &self.run_id)
            .field("turn_id", &self.turn_id)
            .field("attempt_id", &self.attempt_id)
            .field("call_id", &self.call_id)
            .field("timestamp", &self.timestamp)
            .field("causal_parent", &self.causal_parent)
            .field("origin", &self.origin)
            .field("classification", &self.classification)
            .field("event_schema", &self.event_schema)
            .field("projection_revision", &self.projection_revision)
            .field("visibility", &self.visibility)
            .field("scope", &self.scope)
            .field("event_kind_version", &self.event_kind_version)
            .field("data", &self.kind)
            .finish_non_exhaustive()
    }
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
        AgentEventKind::ModelStarted | AgentEventKind::ModelDispatchStarted => {
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
