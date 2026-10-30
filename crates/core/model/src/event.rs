//! Structural model events, complete responses, and strict deterministic assembly.

use std::collections::BTreeMap;
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;

use crate::{
    Actor, CanonicalItemPrefixIdentity, CitationBlock, ContentBlock, ConversationItem, Digest,
    DigestDomain, EntryId, FamilyArtifactError, FamilyArtifactProducerFacts, McpServerId,
    ModelError, ModelErrorKind, ModelId, ModelProviderRequestId, OutputFormat, Provenance,
    ReasoningBlock, ReasoningVisibility, RefusalBlock, ReplayEnvelope, SafeDiagnostic, ServedBy,
    TextBlock, TextPhase, ToolCallBlock, ToolCallState, ToolCorrelation, ToolExecutionOwner,
    ToolName, ValidatedRequestPlan, family_artifact_integrity, validate_family_bytes,
    validate_family_digest, validate_family_id, validate_family_revisions,
};

/// A stable location for one output block.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct OutputLocation {
    item_index: u32,
    block_index: u32,
}

impl OutputLocation {
    /// Creates an output location.
    #[must_use]
    pub const fn new(item_index: u32, block_index: u32) -> Self {
        Self {
            item_index,
            block_index,
        }
    }

    /// Returns the assistant-item index.
    #[must_use]
    pub const fn item_index(self) -> u32 {
        self.item_index
    }

    /// Returns the content-block index within the item.
    #[must_use]
    pub const fn block_index(self) -> u32 {
        self.block_index
    }
}

/// Validated provider-neutral attribution for a successful model response.
#[derive(Clone, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelResponseMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    actual_model: Option<ModelId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider_request_id: Option<ModelProviderRequestId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    served_by: Option<ServedBy>,
}

impl ModelResponseMetadata {
    /// Creates validated successful-response metadata.
    #[must_use]
    pub const fn new(
        actual_model: Option<ModelId>,
        provider_request_id: Option<ModelProviderRequestId>,
    ) -> Self {
        Self {
            actual_model,
            provider_request_id,
            served_by: None,
        }
    }

    /// Creates empty successful-response metadata.
    #[must_use]
    pub const fn empty() -> Self {
        Self::new(None, None)
    }

    /// Attaches the exact provider-reported model.
    #[must_use]
    pub fn with_actual_model(mut self, actual_model: ModelId) -> Self {
        self.actual_model = Some(actual_model);
        self
    }

    /// Attaches the validated provider request identifier.
    #[must_use]
    pub fn with_provider_request_id(mut self, provider_request_id: ModelProviderRequestId) -> Self {
        self.provider_request_id = Some(provider_request_id);
        self
    }

    /// Attaches the validated provider display name when reported.
    #[must_use]
    pub fn with_served_by(mut self, served_by: ServedBy) -> Self {
        self.served_by = Some(served_by);
        self
    }

    /// Returns the exact provider-reported model when available.
    #[must_use]
    pub const fn actual_model(&self) -> Option<&ModelId> {
        self.actual_model.as_ref()
    }

    /// Returns the validated provider request identifier when available.
    #[must_use]
    pub fn provider_request_id(&self) -> Option<&str> {
        self.provider_request_id
            .as_ref()
            .map(ModelProviderRequestId::as_str)
    }

    /// Returns the validated provider display name when available.
    #[must_use]
    pub const fn served_by(&self) -> Option<ServedBy> {
        self.served_by
    }

    /// Reports whether no successful-response attribution is present.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.actual_model.is_none()
            && self.provider_request_id.is_none()
            && self.served_by.is_none()
    }
}

impl fmt::Debug for ModelResponseMetadata {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelResponseMetadata")
            .field("actual_model", &self.actual_model)
            .field(
                "provider_request_id",
                &self.provider_request_id.as_ref().map(|_| "<redacted>"),
            )
            .field("served_by", &self.served_by)
            .finish()
    }
}

/// Structural response-start metadata.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResponseStarted {
    response_id: Option<String>,
    plan_digest: Digest,
    #[serde(default, skip_serializing_if = "ModelResponseMetadata::is_empty")]
    metadata: ModelResponseMetadata,
}

impl ResponseStarted {
    /// Creates response-start metadata.
    #[must_use]
    pub fn new(response_id: Option<String>, plan_digest: Digest) -> Self {
        Self {
            response_id,
            plan_digest,
            metadata: ModelResponseMetadata::empty(),
        }
    }

    /// Attaches validated successful-response metadata.
    #[must_use]
    pub fn with_metadata(mut self, metadata: ModelResponseMetadata) -> Self {
        self.metadata = metadata;
        self
    }

    /// Returns the remote response identity when reported.
    #[must_use]
    pub fn response_id(&self) -> Option<&str> {
        self.response_id.as_deref()
    }

    /// Returns the exact prepared-plan digest.
    #[must_use]
    pub const fn plan_digest(&self) -> Digest {
        self.plan_digest
    }

    /// Returns validated successful-response metadata.
    #[must_use]
    pub const fn metadata(&self) -> &ModelResponseMetadata {
        &self.metadata
    }

    /// Returns the exact provider-reported model when available.
    #[must_use]
    pub const fn actual_model(&self) -> Option<&ModelId> {
        self.metadata.actual_model()
    }

    /// Returns the validated provider request identifier when available.
    #[must_use]
    pub fn provider_request_id(&self) -> Option<&str> {
        self.metadata.provider_request_id()
    }
}

/// Metadata sufficient to assemble one independently indexed block.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
pub enum BlockStart {
    /// Incrementally assembled UTF-8 text.
    Text {
        /// Optional semantic text phase.
        phase: Option<TextPhase>,
    },
    /// Incrementally assembled reasoning text or replay marker.
    Reasoning {
        /// The reasoning visibility state.
        visibility: ReasoningVisibility,
        /// Optional scoped replay material.
        replay: Option<ReplayEnvelope>,
    },
    /// Incrementally assembled reasoning whose visibility is not yet known.
    ReasoningPending {
        /// Optional scoped replay material.
        replay: Option<ReplayEnvelope>,
    },
    /// Incrementally assembled JSON tool input.
    ToolCall {
        /// The stable canonical call ID.
        call_id: crate::CallId,
        /// The model-facing tool name.
        tool: ToolName,
        /// The execution owner.
        owner: ToolExecutionOwner,
        /// Initial lifecycle state.
        state: ToolCallState,
        /// Remote correlation and caller lineage.
        correlation: ToolCorrelation,
    },
    /// Incrementally assembled UTF-8 refusal text.
    Refusal,
    /// An already complete non-delta content block.
    Complete(ContentBlock),
}

/// A block-start event.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BlockStarted {
    item_id: EntryId,
    location: OutputLocation,
    block: BlockStart,
}

impl BlockStarted {
    /// Creates a block-start event.
    #[must_use]
    pub const fn new(item_id: EntryId, location: OutputLocation, block: BlockStart) -> Self {
        Self {
            item_id,
            location,
            block,
        }
    }

    /// Returns stable assistant-item identity.
    #[must_use]
    pub const fn item_id(&self) -> EntryId {
        self.item_id
    }

    /// Returns the independent output location.
    #[must_use]
    pub const fn location(&self) -> OutputLocation {
        self.location
    }

    /// Returns block assembly metadata.
    #[must_use]
    pub const fn block(&self) -> &BlockStart {
        &self.block
    }
}

macro_rules! delta_event {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
        pub struct $name {
            location: OutputLocation,
            bytes: Vec<u8>,
        }

        impl $name {
            /// Creates an owned byte delta.
            #[must_use]
            pub fn new(location: OutputLocation, bytes: impl Into<Vec<u8>>) -> Self {
                Self {
                    location,
                    bytes: bytes.into(),
                }
            }

            /// Returns the target output location.
            #[must_use]
            pub const fn location(&self) -> OutputLocation {
                self.location
            }

            /// Returns raw bytes, which may end inside a UTF-8 sequence.
            #[must_use]
            pub fn bytes(&self) -> &[u8] {
                &self.bytes
            }
        }
    };
}

delta_event!(TextDelta, "A raw text byte delta.");
delta_event!(ReasoningDelta, "A raw reasoning byte delta.");
delta_event!(ToolInputDelta, "A raw tool-input byte delta.");

/// Binds replay material discovered after a reasoning block started.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ReasoningReplayBound {
    location: OutputLocation,
    replay: ReplayEnvelope,
}

impl ReasoningReplayBound {
    /// Creates a late reasoning replay binding.
    #[must_use]
    pub const fn new(location: OutputLocation, replay: ReplayEnvelope) -> Self {
        Self { location, replay }
    }

    /// Returns the target reasoning block location.
    #[must_use]
    pub const fn location(&self) -> OutputLocation {
        self.location
    }

    /// Returns the replay material bound to the reasoning block.
    #[must_use]
    pub const fn replay(&self) -> &ReplayEnvelope {
        &self.replay
    }
}

/// Binds the final visibility of a reasoning block that started pending.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReasoningVisibilityBound {
    location: OutputLocation,
    visibility: ReasoningVisibility,
}

impl ReasoningVisibilityBound {
    /// Creates a final reasoning visibility binding.
    #[must_use]
    pub const fn new(location: OutputLocation, visibility: ReasoningVisibility) -> Self {
        Self {
            location,
            visibility,
        }
    }

    /// Returns the target reasoning block location.
    #[must_use]
    pub const fn location(self) -> OutputLocation {
        self.location
    }

    /// Returns the final reasoning visibility.
    #[must_use]
    pub const fn visibility(self) -> ReasoningVisibility {
        self.visibility
    }
}

/// A complete inline citation event.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CitationAdded {
    item_id: EntryId,
    location: OutputLocation,
    citation: CitationBlock,
}

impl CitationAdded {
    /// Creates an inline citation event.
    #[must_use]
    pub const fn new(item_id: EntryId, location: OutputLocation, citation: CitationBlock) -> Self {
        Self {
            item_id,
            location,
            citation,
        }
    }

    /// Returns stable assistant-item identity.
    #[must_use]
    pub const fn item_id(&self) -> EntryId {
        self.item_id
    }

    /// Returns the citation's ordered block location.
    #[must_use]
    pub const fn location(&self) -> OutputLocation {
        self.location
    }

    /// Returns citation content.
    #[must_use]
    pub const fn citation(&self) -> &CitationBlock {
        &self.citation
    }
}

/// Marks one independently indexed block complete.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BlockCompleted {
    location: OutputLocation,
}

impl BlockCompleted {
    /// Creates a block-completed event.
    #[must_use]
    pub const fn new(location: OutputLocation) -> Self {
        Self { location }
    }

    /// Returns the completed location.
    #[must_use]
    pub const fn location(self) -> OutputLocation {
        self.location
    }
}

/// Cumulative token usage reported by a model operation.
///
/// `input_tokens` is the provider-reported prompt total for the operation.
/// Messages-family dialects fold uncached input, cache writes, and cache reads
/// into this field; chat-completions uses `prompt_tokens` as reported.
/// `cached_input_tokens` and `cache_write_tokens` are observational breakdowns
/// when the wire supplies them. Neither cache counter is subtracted from context
/// admission or semantic input measurement.
#[allow(
    clippy::struct_field_names,
    reason = "the explicit token unit is part of the stable usage vocabulary"
)]
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Usage {
    input_tokens: u64,
    output_tokens: u64,
    reasoning_tokens: u64,
    cached_input_tokens: u64,
    cache_write_tokens: u64,
    cost_micros: Option<u64>,
}

impl Usage {
    /// Creates a cumulative usage snapshot.
    #[must_use]
    pub const fn new(
        input_tokens: u64,
        output_tokens: u64,
        reasoning_tokens: u64,
        cached_input_tokens: u64,
        cache_write_tokens: u64,
    ) -> Self {
        Self {
            input_tokens,
            output_tokens,
            reasoning_tokens,
            cached_input_tokens,
            cache_write_tokens,
            cost_micros: None,
        }
    }

    /// Attaches provider-reported cost in USD micros when available.
    #[must_use]
    pub const fn with_cost_micros(self, cost_micros: Option<u64>) -> Self {
        Self {
            cost_micros,
            ..self
        }
    }

    /// Returns input tokens.
    #[must_use]
    pub const fn input_tokens(self) -> u64 {
        self.input_tokens
    }

    /// Returns output tokens.
    #[must_use]
    pub const fn output_tokens(self) -> u64 {
        self.output_tokens
    }

    /// Returns reasoning tokens.
    #[must_use]
    pub const fn reasoning_tokens(self) -> u64 {
        self.reasoning_tokens
    }

    /// Returns cached input tokens.
    #[must_use]
    pub const fn cached_input_tokens(self) -> u64 {
        self.cached_input_tokens
    }

    /// Returns cache-write input tokens.
    #[must_use]
    pub const fn cache_write_tokens(self) -> u64 {
        self.cache_write_tokens
    }

    /// Returns provider-reported cost in USD micros when available.
    #[must_use]
    pub const fn cost_micros(self) -> Option<u64> {
        self.cost_micros
    }

    fn is_monotonic_after(self, earlier: Self) -> bool {
        self.input_tokens >= earlier.input_tokens
            && self.output_tokens >= earlier.output_tokens
            && self.reasoning_tokens >= earlier.reasoning_tokens
            && self.cached_input_tokens >= earlier.cached_input_tokens
            && self.cache_write_tokens >= earlier.cache_write_tokens
            && cost_micros_monotonic_after(self.cost_micros, earlier.cost_micros)
    }
}

const fn cost_micros_monotonic_after(current: Option<u64>, earlier: Option<u64>) -> bool {
    match (earlier, current) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(earlier), Some(current)) => current >= earlier,
    }
}

/// A cumulative usage event.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UsageUpdate {
    usage: Usage,
}

impl UsageUpdate {
    /// Creates a usage update.
    #[must_use]
    pub const fn new(usage: Usage) -> Self {
        Self { usage }
    }

    /// Returns cumulative usage.
    #[must_use]
    pub const fn usage(self) -> Usage {
        self.usage
    }
}

/// Why a successful model operation stopped.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum FinishReason {
    /// Natural completion.
    Stop,
    /// The model requested one or more tool calls.
    ToolCalls,
    /// The assistant completed this response but requested another model round.
    AssistantContinuation,
    /// Provider-hosted execution paused and requires exact provider continuation.
    ProviderContinuation,
    /// An output limit was reached.
    Length,
    /// The model refused the request.
    Refusal,
    /// A policy stopped generation.
    Policy,
}

/// Structural finish details.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FinishDetails {
    reason: FinishReason,
    detail: Option<String>,
}

impl FinishDetails {
    /// Creates finish details.
    #[must_use]
    pub fn new(reason: FinishReason, detail: Option<String>) -> Self {
        Self { reason, detail }
    }

    /// Returns the normalized finish reason.
    #[must_use]
    pub const fn reason(&self) -> FinishReason {
        self.reason
    }

    /// Returns an optional non-secret detail.
    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }
}

/// Bounded terminal native-state candidate sealed by one family kernel.
///
/// The neutral runtime can persist this value with a completed provider
/// operation, verify every exact producer fact, and then atomically promote
/// its opaque bytes into the manifest. The candidate itself grants no active
/// continuation, replay, cache, history, or terminal authority.
#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct SealedFamilyArtifactCandidate {
    family: String,
    schema_revision: u32,
    codec_revision: u32,
    binding: Digest,
    producer: Arc<FamilyArtifactProducerFacts>,
    bytes: Arc<[u8]>,
    integrity: Digest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SealedFamilyArtifactCandidateWire {
    family: String,
    schema_revision: u32,
    codec_revision: u32,
    binding: Digest,
    producer: FamilyArtifactProducerFacts,
    bytes: Arc<[u8]>,
    integrity: Digest,
}

impl<'de> Deserialize<'de> for SealedFamilyArtifactCandidate {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = SealedFamilyArtifactCandidateWire::deserialize(deserializer)?;
        Self::from_recorded(
            wire.family,
            wire.schema_revision,
            wire.codec_revision,
            wire.binding,
            wire.producer,
            wire.bytes,
            wire.integrity,
        )
        .map_err(serde::de::Error::custom)
    }
}

impl SealedFamilyArtifactCandidate {
    /// Seals exact family bytes and their neutral producer facts.
    ///
    /// # Errors
    ///
    /// Rejects invalid family/revision/binding metadata or empty/oversized bytes.
    #[allow(
        clippy::large_types_passed_by_value,
        reason = "the constructor consumes and shares one immutable exact fact set"
    )]
    pub fn new(
        family: impl Into<String>,
        schema_revision: u32,
        codec_revision: u32,
        binding: Digest,
        producer: FamilyArtifactProducerFacts,
        bytes: impl Into<Arc<[u8]>>,
    ) -> Result<Self, FamilyArtifactError> {
        let family = validate_family_id(family.into())?;
        validate_family_revisions(schema_revision, codec_revision)?;
        validate_family_digest(binding, DigestDomain::ModelBinding, "binding")?;
        let bytes = bytes.into();
        validate_family_bytes(&bytes)?;
        let producer_integrity = producer.integrity()?;
        let integrity = family_artifact_integrity(
            b"plakous-model/sealed-family-artifact-candidate-v3",
            &family,
            schema_revision,
            codec_revision,
            binding,
            Some(producer_integrity),
            &bytes,
        )?;
        Ok(Self {
            family,
            schema_revision,
            codec_revision,
            binding,
            producer: Arc::new(producer),
            bytes,
            integrity,
        })
    }

    #[allow(
        clippy::large_types_passed_by_value,
        reason = "the arguments are the exact serialized candidate fields"
    )]
    fn from_recorded(
        family: String,
        schema_revision: u32,
        codec_revision: u32,
        binding: Digest,
        producer: FamilyArtifactProducerFacts,
        bytes: Arc<[u8]>,
        integrity: Digest,
    ) -> Result<Self, FamilyArtifactError> {
        validate_family_digest(integrity, DigestDomain::Blob, "integrity")?;
        let candidate = Self::new(
            family,
            schema_revision,
            codec_revision,
            binding,
            producer,
            bytes,
        )?;
        if candidate.integrity != integrity {
            return Err(FamilyArtifactError::IntegrityMismatch);
        }
        Ok(candidate)
    }

    fn validate_response_binding(
        &self,
        plan_digest: Digest,
        output_items: usize,
    ) -> Result<(), FamilyArtifactError> {
        if self.producer.plan() != plan_digest {
            return Err(FamilyArtifactError::ForeignResponsePlan);
        }
        if usize::try_from(self.producer.output_items()).ok() != Some(output_items) {
            return Err(FamilyArtifactError::ResponseOutputMismatch);
        }
        Ok(())
    }

    /// Verifies the candidate against complete logical and canonical commit inputs.
    ///
    /// # Errors
    ///
    /// Returns a mismatch for another request, plan, run, turn, attempt,
    /// source/covered projection, or canonical response-output count.
    pub fn validate_for_commit(
        &self,
        plan: &ValidatedRequestPlan,
        covered_projection: CanonicalItemPrefixIdentity,
        response_output_items: usize,
    ) -> Result<(), FamilyArtifactError> {
        self.validate_response_binding(plan.digest(), response_output_items)?;
        self.producer.validate_for_plan(plan, covered_projection)
    }

    /// Returns the stable protocol-family identity.
    #[must_use]
    pub fn family(&self) -> &str {
        &self.family
    }

    /// Returns the behavior-bearing family artifact schema revision.
    #[must_use]
    pub const fn schema_revision(&self) -> u32 {
        self.schema_revision
    }

    /// Returns the exact family artifact codec revision.
    #[must_use]
    pub const fn codec_revision(&self) -> u32 {
        self.codec_revision
    }

    /// Returns the complete generic family/target/producer binding.
    #[must_use]
    pub const fn binding(&self) -> Digest {
        self.binding
    }

    /// Returns all exact neutral producer facts.
    #[must_use]
    pub fn producer(&self) -> &FamilyArtifactProducerFacts {
        self.producer.as_ref()
    }

    /// Returns exact opaque family-owned bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Returns the complete candidate integrity identity.
    #[must_use]
    pub const fn integrity(&self) -> Digest {
        self.integrity
    }
}

impl fmt::Debug for SealedFamilyArtifactCandidate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SealedFamilyArtifactCandidate")
            .field("family", &self.family)
            .field("schema_revision", &self.schema_revision)
            .field("codec_revision", &self.codec_revision)
            .field("binding", &self.binding)
            .field("producer", &self.producer)
            .field(
                "bytes",
                &format_args!("<redacted; {} bytes>", self.bytes.len()),
            )
            .field("integrity", &self.integrity)
            .finish()
    }
}

/// A complete ordered successful model response.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ModelResponse {
    output: Vec<ConversationItem>,
    finish: FinishDetails,
    usage: Option<Usage>,
    response_id: Option<String>,
    plan_digest: Digest,
    #[serde(default, skip_serializing_if = "ModelResponseMetadata::is_empty")]
    metadata: ModelResponseMetadata,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    family_artifact_candidate: Option<SealedFamilyArtifactCandidate>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelResponseWire {
    output: Vec<ConversationItem>,
    finish: FinishDetails,
    usage: Option<Usage>,
    response_id: Option<String>,
    plan_digest: Digest,
    #[serde(default)]
    metadata: ModelResponseMetadata,
    #[serde(default)]
    family_artifact_candidate: Option<SealedFamilyArtifactCandidate>,
}

impl<'de> Deserialize<'de> for ModelResponse {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ModelResponseWire::deserialize(deserializer)?;
        if let Some(candidate) = &wire.family_artifact_candidate {
            candidate
                .validate_response_binding(wire.plan_digest, wire.output.len())
                .map_err(serde::de::Error::custom)?;
        }
        Ok(Self {
            output: wire.output,
            finish: wire.finish,
            usage: wire.usage,
            response_id: wire.response_id,
            plan_digest: wire.plan_digest,
            metadata: wire.metadata,
            family_artifact_candidate: wire.family_artifact_candidate,
        })
    }
}

impl ModelResponse {
    /// Creates a complete response aggregate.
    #[must_use]
    pub fn new(
        output: Vec<ConversationItem>,
        finish: FinishDetails,
        usage: Option<Usage>,
        response_id: Option<String>,
        plan_digest: Digest,
    ) -> Self {
        Self {
            output,
            finish,
            usage,
            response_id,
            plan_digest,
            metadata: ModelResponseMetadata::empty(),
            family_artifact_candidate: None,
        }
    }

    /// Attaches one sealed family-artifact candidate to this terminal response.
    ///
    /// # Errors
    ///
    /// Rejects a candidate for another request plan or canonical output count.
    pub fn with_family_artifact_candidate(
        mut self,
        candidate: SealedFamilyArtifactCandidate,
    ) -> Result<Self, FamilyArtifactError> {
        candidate.validate_response_binding(self.plan_digest, self.output.len())?;
        self.family_artifact_candidate = Some(candidate);
        Ok(self)
    }

    /// Attaches validated successful-response metadata.
    #[must_use]
    pub fn with_metadata(mut self, metadata: ModelResponseMetadata) -> Self {
        self.metadata = metadata;
        self
    }

    /// Records the canonical name and runtime owner `resolve` gives each
    /// runtime-owned tool call in the output: `None` is Plak-local,
    /// `Some(server)` that server's MCP owner. Exposed names are never
    /// rewritten, and provider-hosted calls are left as they are.
    #[must_use]
    pub fn with_canonical_tool_calls(
        mut self,
        mut resolve: impl FnMut(&ToolCallBlock) -> (ToolName, Option<McpServerId>),
    ) -> Self {
        for item in &mut self.output {
            item.resolve_canonical_tool_calls(&mut resolve);
        }
        self
    }

    /// Returns complete ordered output items.
    #[must_use]
    pub fn output(&self) -> &[ConversationItem] {
        &self.output
    }

    /// Returns structural finish details.
    #[must_use]
    pub const fn finish(&self) -> &FinishDetails {
        &self.finish
    }

    /// Returns final reported usage.
    #[must_use]
    pub const fn usage(&self) -> Option<Usage> {
        self.usage
    }

    /// Returns the remote response identity when reported.
    #[must_use]
    pub fn response_id(&self) -> Option<&str> {
        self.response_id.as_deref()
    }

    /// Returns the exact prepared-plan digest.
    #[must_use]
    pub const fn plan_digest(&self) -> Digest {
        self.plan_digest
    }

    /// Returns validated successful-response metadata.
    #[must_use]
    pub const fn metadata(&self) -> &ModelResponseMetadata {
        &self.metadata
    }

    /// Returns the exact provider-reported model when available.
    #[must_use]
    pub const fn actual_model(&self) -> Option<&ModelId> {
        self.metadata.actual_model()
    }

    /// Returns the validated provider request identifier when available.
    #[must_use]
    pub fn provider_request_id(&self) -> Option<&str> {
        self.metadata.provider_request_id()
    }

    /// Returns the sealed terminal family-artifact candidate, when present.
    #[must_use]
    pub const fn family_artifact_candidate(&self) -> Option<&SealedFamilyArtifactCandidate> {
        self.family_artifact_candidate.as_ref()
    }

    /// Validates the canonical semantic response boundary used before commit
    /// or any post-response effects.
    ///
    /// # Errors
    ///
    /// Returns an invalid-event error for empty output other than refusal/length, or foreign-shaped output.
    pub fn validate_for_commit(&self) -> Result<(), ModelError> {
        if self.output.is_empty()
            && !matches!(
                self.finish.reason(),
                FinishReason::Refusal | FinishReason::Length
            )
        {
            return Err(invalid_event(
                "successful model response contains no assistant output",
            ));
        }
        if self.plan_digest.domain() != DigestDomain::RequestPlan {
            return Err(invalid_event(
                "model response plan digest is outside the request-plan domain",
            ));
        }
        if self.output.iter().any(|item| {
            item.actor() != Actor::Assistant || item.provenance() != Provenance::ModelOutput
        }) {
            return Err(invalid_event(
                "model response output is not canonical assistant model output",
            ));
        }
        if let Some(candidate) = &self.family_artifact_candidate {
            candidate
                .validate_response_binding(self.plan_digest, self.output.len())
                .map_err(|_| invalid_event("model response has a foreign family artifact"))?;
        }
        Ok(())
    }

    /// Validates the completed assistant output against its admitted format.
    ///
    /// Text output retains the ordinary structural response rules. Structured
    /// output must be one assistant item containing only one text block whose
    /// complete contents strictly parse and validate against the exact schema.
    ///
    /// # Errors
    ///
    /// Returns a content-free invalid-event error when structured output is
    /// malformed, mismatched, or mixed with any other output item or block.
    pub fn validate_output_format(&self, format: &OutputFormat) -> Result<(), ModelError> {
        let OutputFormat::Structured(schema) = format else {
            return Ok(());
        };
        if self.finish.reason() != FinishReason::Stop {
            return Err(invalid_event("structured assistant output was invalid"));
        }
        let [item] = self.output.as_slice() else {
            return Err(invalid_event("structured assistant output was invalid"));
        };
        if item.actor() != Actor::Assistant || item.provenance() != Provenance::ModelOutput {
            return Err(invalid_event("structured assistant output was invalid"));
        }
        let [ContentBlock::Text(text)] = item.blocks() else {
            return Err(invalid_event("structured assistant output was invalid"));
        };
        schema
            .parse_instance(text.text())
            .map(|_| ())
            .map_err(|_| invalid_event("structured assistant output was invalid"))
    }

    /// Projects all text blocks without replacing structural output.
    #[must_use]
    pub fn output_text(&self) -> String {
        let mut output = String::new();
        for item in &self.output {
            for block in item.blocks() {
                if let ContentBlock::Text(text) = block {
                    output.push_str(text.text());
                }
            }
        }
        output
    }
}

/// A bounded provider-neutral diagnostic emitted by model preparation or execution.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelDiagnostic {
    message: SafeDiagnostic,
    #[serde(skip)]
    history_projection: Option<Digest>,
}

impl ModelDiagnostic {
    /// Creates a bounded, credential-safe diagnostic.
    #[must_use]
    pub fn new(message: impl AsRef<str>) -> Self {
        Self {
            message: SafeDiagnostic::new(message),
            history_projection: None,
        }
    }

    /// Creates a credential-safe notice for one historical projection transition.
    #[must_use]
    pub fn historical_projection(identity: Digest, message: impl AsRef<str>) -> Self {
        Self {
            message: SafeDiagnostic::new(message),
            history_projection: Some(identity),
        }
    }

    /// Returns the transition identity, present only on historical projection notices.
    #[must_use]
    pub const fn history_projection_identity(&self) -> Option<Digest> {
        self.history_projection
    }

    /// Returns the retained bounded diagnostic.
    #[must_use]
    pub const fn message(&self) -> &SafeDiagnostic {
        &self.message
    }
}

/// The transport or provider fault that broke a physical request a kernel is
/// about to send again.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTransportFault {
    /// The connection failed or was reset.
    Io,
    /// HTTP framing or protocol processing failed.
    Protocol,
    /// TLS failed.
    Tls,
    /// A deadline or the stream's idle limit elapsed.
    Timeout,
    /// The response stream ended before its terminal event.
    EndOfStream,
    /// The provider reported itself overloaded inside the stream.
    Overloaded,
    /// The provider reported a rate limit inside the stream.
    RateLimited,
}

impl fmt::Display for ModelTransportFault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Io => "connection lost",
            Self::Protocol => "protocol error",
            Self::Tls => "TLS error",
            Self::Timeout => "timed out",
            Self::EndOfStream => "stream ended early",
            Self::Overloaded => "provider overloaded",
            Self::RateLimited => "provider rate limited",
        })
    }
}

/// Why a kernel is sending a turn request again.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModelRetryCause {
    /// The provider answered with this HTTP status before any response stream.
    HttpStatus {
        /// The status code.
        status: u16,
    },
    /// The connection could not be established, or the request never left
    /// whole; the provider cannot have acted on it.
    Connect,
    /// The peer killed the connection after rejecting a TLS record of this
    /// incomplete request; no provider action can have occurred.
    RecordRejectedInTransit,
    /// The request may have reached the provider, and the exchange failed
    /// before any response byte arrived.
    TransportAfterSend {
        /// What broke.
        fault: ModelTransportFault,
    },
    /// The response stream broke after its head.
    StreamInterrupted {
        /// What broke.
        fault: ModelTransportFault,
    },
}

impl ModelRetryCause {
    /// The stable snake-case name of this cause's kind.
    #[must_use]
    pub const fn kind(self) -> &'static str {
        match self {
            Self::HttpStatus { .. } => "http_status",
            Self::Connect => "connect",
            Self::RecordRejectedInTransit => "record_rejected_in_transit",
            Self::TransportAfterSend { .. } => "transport_after_send",
            Self::StreamInterrupted { .. } => "stream_interrupted",
        }
    }
}

impl fmt::Display for ModelRetryCause {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HttpStatus { status: 429 } => {
                formatter.write_str("provider rate limited (HTTP 429)")
            }
            Self::HttpStatus {
                status: status @ (503 | 529),
            } => write!(formatter, "provider busy (HTTP {status})"),
            Self::HttpStatus { status } => write!(formatter, "provider error (HTTP {status})"),
            Self::Connect => formatter.write_str("provider unreachable (connect failed)"),
            Self::RecordRejectedInTransit => {
                formatter.write_str("request corrupted in transit (TLS record rejected)")
            }
            Self::TransportAfterSend { fault } => {
                write!(formatter, "request failed before a response ({fault})")
            }
            Self::StreamInterrupted { fault } => {
                write!(formatter, "response stream interrupted ({fault})")
            }
        }
    }
}

/// Content-free physical transport phase of a replayed failure.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRetryPhase {
    /// Hostname resolution.
    Resolution,
    /// TCP connection establishment.
    Connect,
    /// TLS or negotiated HTTP establishment.
    Tls,
    /// Request head or body transmission.
    RequestWrite,
    /// Unobservable boundary between transmission and response head.
    RequestWriteOrResponseHead,
    /// Response head wait or validation.
    ResponseHead,
    /// Response body or stream decoding.
    ResponseBody,
    /// Stopping and joining connection work.
    Cleanup,
}

impl ModelRetryPhase {
    /// Stable analytics label; never provider text.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Resolution => "resolution",
            Self::Connect => "connect",
            Self::Tls => "tls",
            Self::RequestWrite => "request_write",
            Self::RequestWriteOrResponseHead => "request_write_or_response_head",
            Self::ResponseHead => "response_head",
            Self::ResponseBody => "response_body",
            Self::Cleanup => "cleanup",
        }
    }
}

/// Content-free request transmission evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelNetworkTransmission {
    /// No request bytes were sent.
    NotSent,
    /// The peer could not receive a complete request.
    Incomplete,
    /// The complete request may have reached the peer.
    MayHaveSent,
}

/// Content-free network cause, suitable for public failure presentation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModelNetworkFailure {
    /// The physical phase that failed.
    pub phase: ModelRetryPhase,
    /// Whether a request could have reached the peer.
    pub sent: ModelNetworkTransmission,
    /// Whether a configured deadline elapsed.
    pub timed_out: bool,
}

impl ModelNetworkFailure {
    /// Whether no request bytes were submitted to the connection.
    #[must_use]
    pub const fn nothing_sent(self) -> bool {
        matches!(self.sent, ModelNetworkTransmission::NotSent)
    }
}

impl fmt::Display for ModelNetworkFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self.phase {
            ModelRetryPhase::Resolution if self.timed_out => {
                "DNS lookup timed out before anything was sent"
            }
            ModelRetryPhase::Resolution => "DNS lookup failed before anything was sent",
            ModelRetryPhase::Connect => "could not connect to the provider (nothing was sent)",
            ModelRetryPhase::Tls => {
                "TLS handshake failed before anything was sent (a captive portal or intercepting proxy can cause this)"
            }
            ModelRetryPhase::RequestWrite if self.sent == ModelNetworkTransmission::Incomplete => {
                "connection dropped while sending (the provider did not receive the request whole)"
            }
            ModelRetryPhase::ResponseBody => "response stream interrupted",
            _ if self.sent == ModelNetworkTransmission::NotSent => {
                "connection failed before anything was sent"
            }
            _ => "connection dropped after the request was sent, before a response",
        };
        formatter.write_str(text)
    }
}

/// Why an otherwise classifiable retry was declined by its owning kernel.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ModelRetryDeclineReason {
    /// A genuine caller deadline prevents the replay.
    Deadline,
    /// A physical, wait, or long-generation replay budget is exhausted.
    Budget,
    /// Replaying may duplicate provider-hosted effects.
    ServerEffect,
    /// The provider explicitly disabled retries.
    RetryDisabled,
    /// This operation is not a replayable turn workflow.
    Workflow,
}

impl ModelRetryDeclineReason {
    /// Closed, content-free telemetry spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Deadline => "deadline",
            Self::Budget => "budget",
            Self::ServerEffect => "server_effect",
            Self::RetryDisabled => "retry_disabled",
            Self::Workflow => "workflow",
        }
    }
}

/// Closed Messages `diagnostics.cache_miss_reason.type` class.
///
/// Unknown provider spellings are [`Self::Other`]. The raw spelling is never retained.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum PromptCacheMissReason {
    /// `model_changed`.
    ModelChanged,
    /// `system_changed`.
    SystemChanged,
    /// `tools_changed`.
    ToolsChanged,
    /// `messages_changed`.
    MessagesChanged,
    /// `previous_message_not_found`.
    PreviousMessageNotFound,
    /// `unavailable`.
    Unavailable,
    /// Any other string, including an empty one.
    Other,
}

impl PromptCacheMissReason {
    /// Stable content-free spelling. Never a raw provider string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ModelChanged => "model_changed",
            Self::SystemChanged => "system_changed",
            Self::ToolsChanged => "tools_changed",
            Self::MessagesChanged => "messages_changed",
            Self::PreviousMessageNotFound => "previous_message_not_found",
            Self::Unavailable => "unavailable",
            Self::Other => "other",
        }
    }
}

/// Content-free Messages cache-miss diagnosis.
///
/// Advisory telemetry only. It is not usage, history, or replay authority.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PromptCacheDiagnosis {
    /// Closed miss class.
    pub reason: PromptCacheMissReason,
    /// `cache_missed_input_tokens` when the provider sent a non-negative integer.
    /// Absent when the key was omitted. Never a raw provider string.
    #[serde(default)]
    pub missed_input_tokens: Option<u64>,
}

/// Content-free activity facts, independent of canonical output projection.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelStreamActivity {
    /// Visible message/refusal or tool output has begun, including unprojected tool input.
    pub output_started: bool,
    /// Time from physical dispatch to the latest semantic event.
    pub last_semantic_event_elapsed_ms: Option<u64>,
    /// Age of that event at observation, in milliseconds.
    pub semantic_gap_ms: u64,
    /// Keepalive or unclassified events, cumulative per logical attempt.
    pub keepalive_count: u64,
    /// Largest gap observed while the semantic stall bound was active, in milliseconds.
    /// Cumulative per logical attempt.
    pub max_semantic_gap_ms: u64,
}

/// Content-free accounting of one failed physical response.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelRetryLoss {
    /// Streamed assistant text bytes, not token estimates.
    pub assistant_text_bytes: u64,
    /// Streamed reasoning bytes, including blocks completed before the break.
    pub reasoning_bytes: u64,
    /// Closed streamed output items/blocks that never reached a complete response.
    pub completed_items: u64,
    /// Wall time of the broken physical request, before backoff.
    pub elapsed_ms: u64,
    /// The exact request can start hosted work beyond inference.
    pub server_effect: bool,
    /// Exact failed transport phase, or response phase for a protocol refusal.
    pub transport_phase: Option<ModelRetryPhase>,
    /// Present only when no retry will follow this observation.
    pub declined_reason: Option<ModelRetryDeclineReason>,
}

impl ModelRetryLoss {
    /// Counts a structural event without retaining its content.
    pub fn observe(&mut self, event: &ModelEvent) {
        match event {
            ModelEvent::TextDelta(delta) => {
                self.assistant_text_bytes = self
                    .assistant_text_bytes
                    .saturating_add(delta.bytes().len() as u64);
            }
            ModelEvent::ReasoningDelta(delta) => {
                self.reasoning_bytes = self
                    .reasoning_bytes
                    .saturating_add(delta.bytes().len() as u64);
            }
            ModelEvent::BlockCompleted(_) => {
                self.completed_items = self.completed_items.saturating_add(1);
            }
            _ => {}
        }
    }
}

/// A kernel is waiting to send a turn request again under its retry budget.
///
/// When [`Self::output_discarded`] is true the failed physical request had
/// already streamed events for this model call; every consumer must drop
/// what it accumulated from them, because the replay's events follow from a
/// fresh response start and only its terminal response can reach history.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelRetry {
    next_physical: u32,
    max_physical: u32,
    delay_ms: u64,
    cause: ModelRetryCause,
    output_discarded: bool,
    detail: SafeDiagnostic,
    loss: ModelRetryLoss,
    #[serde(skip)]
    request: Option<(SafeDiagnostic, SafeDiagnostic, u64)>,
}

impl ModelRetry {
    /// Creates a retry notice. `detail` is the bounded, credential-safe
    /// diagnostic of the failure being retried.
    #[must_use]
    pub fn new(
        next_physical: u32,
        max_physical: u32,
        delay: std::time::Duration,
        cause: ModelRetryCause,
        output_discarded: bool,
        detail: impl AsRef<str>,
    ) -> Self {
        Self {
            next_physical,
            max_physical,
            delay_ms: u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
            cause,
            output_discarded,
            detail: SafeDiagnostic::new(detail),
            loss: ModelRetryLoss::default(),
            request: None,
        }
    }

    /// Attaches content-free physical-attempt accounting.
    #[must_use]
    pub const fn with_loss(mut self, loss: ModelRetryLoss) -> Self {
        self.loss = loss;
        self
    }

    /// Attaches ephemeral, content-free request logging facts. The route is a
    /// path only: query parameters, headers and body content are not retained.
    #[must_use]
    pub fn with_request(mut self, host: &str, route: &str, body_bytes: usize) -> Self {
        let path = route.split('?').next().unwrap_or_default();
        self.request = Some((
            SafeDiagnostic::new(host),
            SafeDiagnostic::new(path),
            u64::try_from(body_bytes).unwrap_or(u64::MAX),
        ));
        self
    }

    /// Content-free host, route path and exact serialized body byte count.
    /// These transient logging facts are not serialized with the event.
    #[must_use]
    pub fn request(&self) -> Option<(&str, &str, u64)> {
        self.request
            .as_ref()
            .map(|(host, route, bytes)| (host.as_str(), route.as_str(), *bytes))
    }

    /// What the failed physical response discarded.
    #[must_use]
    pub const fn loss(&self) -> ModelRetryLoss {
        self.loss
    }

    /// The proposed next physical request (2-based); never sent when declined.
    #[must_use]
    pub const fn next_physical(&self) -> u32 {
        self.next_physical
    }

    /// The physical-request budget of the logical attempt.
    #[must_use]
    pub const fn max_physical(&self) -> u32 {
        self.max_physical
    }

    /// The wait before the next physical request.
    #[must_use]
    pub const fn delay(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.delay_ms)
    }

    /// What broke the previous physical request.
    #[must_use]
    pub const fn cause(&self) -> ModelRetryCause {
        self.cause
    }

    /// Operator-facing cause, preserving the closed physical network phase.
    #[must_use]
    pub fn human_cause(&self) -> String {
        let Some(phase) = self.loss.transport_phase else {
            return self.cause.to_string();
        };
        let (sent, timed_out) = match self.cause {
            ModelRetryCause::Connect => (
                if phase == ModelRetryPhase::RequestWrite {
                    ModelNetworkTransmission::Incomplete
                } else {
                    ModelNetworkTransmission::NotSent
                },
                false,
            ),
            ModelRetryCause::TransportAfterSend { fault }
            | ModelRetryCause::StreamInterrupted { fault } => (
                ModelNetworkTransmission::MayHaveSent,
                fault == ModelTransportFault::Timeout,
            ),
            _ => return self.cause.to_string(),
        };
        let network = ModelNetworkFailure {
            phase,
            sent,
            timed_out,
        }
        .to_string();
        if self.cause == ModelRetryCause::Connect
            && matches!(
                phase,
                ModelRetryPhase::Resolution | ModelRetryPhase::Connect | ModelRetryPhase::Tls
            )
            && self.loss.declined_reason.is_none()
        {
            format!(
                "provider unreachable ({network}); waiting for network, try {} in {}s",
                self.next_physical(),
                self.delay().as_secs()
            )
        } else {
            network
        }
    }

    /// Whether events the failed physical request streamed are discarded.
    #[must_use]
    pub const fn output_discarded(&self) -> bool {
        self.output_discarded
    }

    /// The bounded diagnostic of the failure being retried.
    #[must_use]
    pub const fn detail(&self) -> &SafeDiagnostic {
        &self.detail
    }
}

/// An unknown remote event retained without exposing its payload to `Debug`.
#[derive(Clone, Deserialize, Serialize)]
pub struct ProviderEvent {
    kind: String,
    payload: Box<RawValue>,
}

impl PartialEq for ProviderEvent {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind && self.payload.get() == other.payload.get()
    }
}

impl ProviderEvent {
    /// Creates an opaque remote event.
    #[must_use]
    pub fn new(kind: impl Into<String>, payload: Box<RawValue>) -> Self {
        Self {
            kind: kind.into(),
            payload,
        }
    }

    /// Returns the remote event kind.
    #[must_use]
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// Returns the exact raw payload.
    #[must_use]
    pub fn payload(&self) -> &RawValue {
        &self.payload
    }
}

impl fmt::Debug for ProviderEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderEvent")
            .field("kind", &self.kind)
            .field("payload", &"<redacted>")
            .finish()
    }
}

/// A structural provider-neutral model stream event.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ModelEvent {
    /// A bounded semantic diagnostic observable through the normal turn channel.
    Diagnostic(ModelDiagnostic),
    /// The kernel is waiting to send the request again; when it discards
    /// output, every event since the last response start is void.
    Retry(ModelRetry),
    /// Content-free physical stream activity, never canonical history.
    StreamActivity(ModelStreamActivity),
    /// Content-free Messages cache-miss diagnosis, never canonical history.
    PromptCacheDiagnosis(PromptCacheDiagnosis),
    /// A response was accepted and started.
    ResponseStarted(ResponseStarted),
    /// An independently indexed output block started.
    BlockStarted(BlockStarted),
    /// A raw text byte fragment arrived.
    TextDelta(TextDelta),
    /// A raw reasoning byte fragment arrived.
    ReasoningDelta(ReasoningDelta),
    /// Replay material was bound to an open reasoning block.
    ReasoningReplayBound(ReasoningReplayBound),
    /// Final visibility was bound to an open pending reasoning block.
    ReasoningVisibilityBound(ReasoningVisibilityBound),
    /// A raw tool-input byte fragment arrived.
    ToolInputDelta(ToolInputDelta),
    /// A complete citation arrived inline.
    CitationAdded(CitationAdded),
    /// One output block completed.
    BlockCompleted(BlockCompleted),
    /// A cumulative usage update arrived.
    Usage(UsageUpdate),
    /// The single terminal aggregate arrived.
    ResponseCompleted(ModelResponse),
    /// An unknown remote event was retained for forward compatibility.
    ProviderEvent(ProviderEvent),
}

#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "OpenAssembly carries bounded assembly maps; boxing would spread indirection through every push path"
)]
enum AssemblyState {
    AwaitingStart,
    Open(OpenAssembly),
    Succeeded,
    Failed,
}

#[derive(Debug)]
struct OpenAssembly {
    started: ResponseStarted,
    item_ids: BTreeMap<u32, EntryId>,
    blocks: BTreeMap<OutputLocation, PartialBlock>,
    usage: Option<Usage>,
}

/// Resource limits enforced by the structural response assembler.
#[allow(
    clippy::struct_field_names,
    reason = "the max prefix makes every independently configurable bound explicit"
)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AssemblerLimits {
    max_events: NonZeroUsize,
    max_items: NonZeroUsize,
    max_blocks: NonZeroUsize,
    max_block_bytes: NonZeroUsize,
    max_provider_event_bytes: NonZeroUsize,
}

impl AssemblerLimits {
    /// Creates an explicit set of nonzero resource limits.
    #[must_use]
    pub const fn new(
        max_events: NonZeroUsize,
        max_items: NonZeroUsize,
        max_blocks: NonZeroUsize,
        max_block_bytes: NonZeroUsize,
        max_provider_event_bytes: NonZeroUsize,
    ) -> Self {
        Self {
            max_events,
            max_items,
            max_blocks,
            max_block_bytes,
            max_provider_event_bytes,
        }
    }

    /// Returns the maximum accepted event count.
    #[must_use]
    pub const fn max_events(self) -> usize {
        self.max_events.get()
    }

    /// Returns the maximum output-item count.
    #[must_use]
    pub const fn max_items(self) -> usize {
        self.max_items.get()
    }

    /// Returns the maximum total output-block count.
    #[must_use]
    pub const fn max_blocks(self) -> usize {
        self.max_blocks.get()
    }

    /// Returns the maximum bytes accumulated for one streamed block.
    #[must_use]
    pub const fn max_block_bytes(self) -> usize {
        self.max_block_bytes.get()
    }

    /// Returns the maximum retained bytes for one unknown remote event.
    #[must_use]
    pub const fn max_provider_event_bytes(self) -> usize {
        self.max_provider_event_bytes.get()
    }
}

impl Default for AssemblerLimits {
    fn default() -> Self {
        Self::new(
            NonZeroUsize::new(100_000).unwrap_or(NonZeroUsize::MIN),
            NonZeroUsize::new(1_024).unwrap_or(NonZeroUsize::MIN),
            NonZeroUsize::new(4_096).unwrap_or(NonZeroUsize::MIN),
            NonZeroUsize::new(16 * 1024 * 1024).unwrap_or(NonZeroUsize::MIN),
            NonZeroUsize::new(1024 * 1024).unwrap_or(NonZeroUsize::MIN),
        )
    }
}

#[derive(Debug)]
enum PartialBlock {
    Text {
        phase: Option<TextPhase>,
        bytes: Vec<u8>,
    },
    Reasoning {
        visibility: Option<ReasoningVisibility>,
        replay: Option<ReplayEnvelope>,
        bytes: Vec<u8>,
    },
    ToolCall {
        call_id: crate::CallId,
        tool: ToolName,
        owner: ToolExecutionOwner,
        state: ToolCallState,
        correlation: ToolCorrelation,
        bytes: Vec<u8>,
    },
    Refusal(Vec<u8>),
    Complete(ContentBlock),
}

impl PartialBlock {
    fn retained_bytes(&self) -> usize {
        match self {
            Self::Text { bytes, .. } | Self::ToolCall { bytes, .. } | Self::Refusal(bytes) => {
                bytes.len()
            }
            Self::Reasoning { replay, bytes, .. } => bytes.len().saturating_add(
                replay
                    .as_ref()
                    .map_or(0, |value| value.payload().get().len()),
            ),
            Self::Complete(block) => block.retained_bytes(),
        }
    }
}

/// A strict, deterministic observation-consistency checker.
///
/// This helper is useful for lossless adapter/projector tests. It is not
/// semantic result authority: runtime consumers must accept and validate the
/// self-contained [`ModelEvent::ResponseCompleted`] independently because
/// bounded nonterminal observations may be dropped. An empty refusal terminal
/// may revoke complete provisional output and unfinished local tool inputs;
/// identity and usage still agree, and possible tool effects cannot be discarded.
#[derive(Debug)]
pub struct ModelResponseAssembler {
    state: AssemblyState,
    expected_plan_digest: Digest,
    limits: AssemblerLimits,
    events_seen: usize,
}

impl ModelResponseAssembler {
    /// Creates an assembler bound to the inspected prepared-plan digest.
    ///
    /// # Errors
    ///
    /// Returns an invalid-request error when the supplied digest is not in the
    /// request-plan domain.
    pub fn new(expected_plan_digest: Digest) -> Result<Self, ModelError> {
        Self::with_limits(expected_plan_digest, AssemblerLimits::default())
    }

    /// Creates a plan-bound assembler with explicit resource limits.
    ///
    /// # Errors
    ///
    /// Returns an invalid-request error when the supplied digest is not in the
    /// request-plan domain.
    pub fn with_limits(
        expected_plan_digest: Digest,
        limits: AssemblerLimits,
    ) -> Result<Self, ModelError> {
        if expected_plan_digest.domain() != DigestDomain::RequestPlan {
            return Err(ModelError::new(
                ModelErrorKind::InvalidRequest,
                "response assembler requires a request-plan digest",
            ));
        }
        Ok(Self {
            state: AssemblyState::AwaitingStart,
            expected_plan_digest,
            limits,
            events_seen: 0,
        })
    }

    /// Applies one event, returning the terminal aggregate exactly once.
    ///
    /// # Errors
    ///
    /// Returns a terminal protocol error when the event violates the assembly
    /// grammar or contradicts already assembled state.
    pub fn push(&mut self, event: ModelEvent) -> Result<Option<ModelResponse>, ModelError> {
        if matches!(
            event,
            ModelEvent::StreamActivity(_) | ModelEvent::PromptCacheDiagnosis(_)
        ) {
            return Ok(None);
        }
        self.events_seen = self.events_seen.saturating_add(1);
        if self.events_seen > self.limits.max_events() {
            return self.fail("model stream exceeded the event limit");
        }
        if let ModelEvent::ProviderEvent(remote) = &event
            && remote.payload().get().len() > self.limits.max_provider_event_bytes()
        {
            return self.fail("remote event exceeded the retained payload limit");
        }
        match event {
            ModelEvent::Diagnostic(_) | ModelEvent::ProviderEvent(_)
                if matches!(self.state, AssemblyState::AwaitingStart) =>
            {
                Ok(None)
            }
            ModelEvent::Retry(retry) => self.restart(retry.output_discarded()),
            ModelEvent::ResponseStarted(started) => self.start_response(started),
            ModelEvent::ResponseCompleted(response) => self.complete_response(response),
            other => self.apply_open_event(other),
        }
    }

    /// Validates EOF, rejecting it unless a terminal aggregate was assembled.
    ///
    /// # Errors
    ///
    /// Returns an incomplete-response error unless a valid terminal aggregate
    /// was assembled before EOF.
    pub fn finish_eof(&mut self) -> Result<(), ModelError> {
        match self.state {
            AssemblyState::Succeeded => Ok(()),
            AssemblyState::Failed => Err(invalid_event("assembler already failed")),
            AssemblyState::AwaitingStart | AssemblyState::Open(_) => {
                self.state = AssemblyState::Failed;
                Err(ModelError::incomplete())
            }
        }
    }

    /// A retry before any output leaves the assembly awaiting its start; one
    /// that discards output voids the open response so the replay starts over.
    fn restart(&mut self, output_discarded: bool) -> Result<Option<ModelResponse>, ModelError> {
        match self.state {
            AssemblyState::AwaitingStart => Ok(None),
            AssemblyState::Open(_) if output_discarded => {
                self.state = AssemblyState::AwaitingStart;
                Ok(None)
            }
            AssemblyState::Open(_) => self.fail("retry kept output of an open response"),
            AssemblyState::Succeeded | AssemblyState::Failed => {
                self.fail("retry after terminal state")
            }
        }
    }

    fn start_response(
        &mut self,
        started: ResponseStarted,
    ) -> Result<Option<ModelResponse>, ModelError> {
        if !matches!(self.state, AssemblyState::AwaitingStart) {
            return self.fail("response started more than once or after terminal state");
        }
        if started.plan_digest != self.expected_plan_digest {
            return self.fail("response start does not match the prepared request plan");
        }
        self.state = AssemblyState::Open(OpenAssembly {
            started,
            item_ids: BTreeMap::new(),
            blocks: BTreeMap::new(),
            usage: None,
        });
        Ok(None)
    }

    fn apply_open_event(&mut self, event: ModelEvent) -> Result<Option<ModelResponse>, ModelError> {
        let AssemblyState::Open(open) = &mut self.state else {
            return self.fail("event received outside an open response");
        };
        let result = match event {
            ModelEvent::BlockStarted(started) => add_started(open, started, self.limits),
            ModelEvent::TextDelta(delta) => append_delta(
                open,
                delta.location(),
                delta.bytes(),
                DeltaKind::Text,
                self.limits,
            ),
            ModelEvent::ReasoningDelta(delta) => append_delta(
                open,
                delta.location(),
                delta.bytes(),
                DeltaKind::Reasoning,
                self.limits,
            ),
            ModelEvent::ReasoningReplayBound(binding) => {
                bind_reasoning_replay(open, binding, self.limits)
            }
            ModelEvent::ReasoningVisibilityBound(binding) => {
                bind_reasoning_visibility(open, binding)
            }
            ModelEvent::ToolInputDelta(delta) => append_delta(
                open,
                delta.location(),
                delta.bytes(),
                DeltaKind::ToolInput,
                self.limits,
            ),
            ModelEvent::CitationAdded(citation) => add_citation(open, citation, self.limits),
            ModelEvent::BlockCompleted(completed) => complete_block(open, completed.location()),
            ModelEvent::Usage(update) => update_usage(open, update.usage()),
            ModelEvent::Diagnostic(_)
            | ModelEvent::ProviderEvent(_)
            | ModelEvent::StreamActivity(_)
            | ModelEvent::PromptCacheDiagnosis(_) => Ok(()),
            ModelEvent::Retry(_)
            | ModelEvent::ResponseStarted(_)
            | ModelEvent::ResponseCompleted(_) => {
                Err(invalid_event("invalid nested response event"))
            }
        };
        match result {
            Ok(()) => Ok(None),
            Err(error) => {
                self.state = AssemblyState::Failed;
                Err(error)
            }
        }
    }

    fn complete_response(
        &mut self,
        response: ModelResponse,
    ) -> Result<Option<ModelResponse>, ModelError> {
        let AssemblyState::Open(open) = &self.state else {
            return self.fail("terminal aggregate received outside an open response");
        };
        if let Err(error) = validate_completed_response(open, &response) {
            self.state = AssemblyState::Failed;
            return Err(error);
        }
        self.state = AssemblyState::Succeeded;
        Ok(Some(response))
    }

    fn fail<T>(&mut self, message: &'static str) -> Result<T, ModelError> {
        self.state = AssemblyState::Failed;
        Err(invalid_event(message))
    }
}

fn validate_completed_response(
    open: &OpenAssembly,
    response: &ModelResponse,
) -> Result<(), ModelError> {
    response.validate_for_commit()?;
    // Refusal can revoke local proposals before their provisional arguments
    // become valid canonical tool input. Other open blocks still fail closed.
    if response.finish.reason() == FinishReason::Refusal && response.output.is_empty() {
        validate_revoked_output(open)?;
    } else if assemble_items(open)? != response.output {
        return Err(invalid_event(
            "terminal aggregate output does not match streamed blocks",
        ));
    }
    if open.started.response_id != response.response_id {
        return Err(invalid_event(
            "terminal response identity does not match response start",
        ));
    }
    if open.started.plan_digest != response.plan_digest {
        return Err(invalid_event(
            "terminal plan digest does not match response start",
        ));
    }
    validate_response_metadata(&open.started.metadata, &response.metadata)?;
    if open.usage.is_some() && open.usage != response.usage {
        return Err(invalid_event(
            "terminal usage does not match the final usage update",
        ));
    }
    Ok(())
}

fn validate_revoked_output(open: &OpenAssembly) -> Result<(), ModelError> {
    let mut previous_item = None;
    let mut next_block = 0;
    for (location, block) in &open.blocks {
        if previous_item != Some(location.item_index) {
            let expected_item = previous_item.map_or(0, |index| index + 1);
            if location.item_index != expected_item {
                return Err(invalid_event("assistant item indices are not contiguous"));
            }
            previous_item = Some(location.item_index);
            next_block = 0;
        }
        if location.block_index != next_block {
            return Err(invalid_event("output block indices are not contiguous"));
        }
        next_block += 1;
        match block {
            PartialBlock::ToolCall {
                owner: ToolExecutionOwner::Local | ToolExecutionOwner::Mcp { .. },
                state: ToolCallState::Requested,
                ..
            } => {}
            PartialBlock::Complete(block) => {
                if matches!(block, ContentBlock::ToolCall(call)
                    if call.state() != ToolCallState::Requested
                        || matches!(call.owner(), ToolExecutionOwner::ProviderHosted { .. }))
                    || matches!(
                        block,
                        ContentBlock::ToolResult(_) | ContentBlock::ToolResultUpdate(_)
                    )
                {
                    return Err(invalid_event(
                        "refusal cannot discard possible tool effects",
                    ));
                }
            }
            _ => {
                return Err(invalid_event(
                    "refusal arrived with an open non-client block",
                ));
            }
        }
    }
    Ok(())
}

fn validate_response_metadata(
    started: &ModelResponseMetadata,
    completed: &ModelResponseMetadata,
) -> Result<(), ModelError> {
    match (started.actual_model(), completed.actual_model()) {
        (Some(started), Some(completed)) if started == completed => {}
        (Some(_), None) => {
            return Err(invalid_event(
                "terminal response omitted the actual model reported at response start",
            ));
        }
        (Some(_), Some(_)) => {
            return Err(invalid_event(
                "terminal actual model contradicts response start",
            ));
        }
        (None, _) => {}
    }
    match (
        started.provider_request_id(),
        completed.provider_request_id(),
    ) {
        (Some(started), Some(completed)) if started == completed => {}
        (Some(_), None) => {
            return Err(invalid_event(
                "terminal response omitted the provider request ID reported at response start",
            ));
        }
        (Some(_), Some(_)) => {
            return Err(invalid_event(
                "terminal provider request ID contradicts response start",
            ));
        }
        (None, _) => {}
    }
    match (started.served_by(), completed.served_by()) {
        (Some(started), Some(completed)) if started == completed => {}
        (Some(_), None) => {
            return Err(invalid_event(
                "terminal response omitted the served endpoint reported at response start",
            ));
        }
        (Some(_), Some(_)) => {
            return Err(invalid_event(
                "terminal served endpoint contradicts response start",
            ));
        }
        (None, _) => {}
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum DeltaKind {
    Text,
    Reasoning,
    ToolInput,
}

fn add_started(
    open: &mut OpenAssembly,
    started: BlockStarted,
    limits: AssemblerLimits,
) -> Result<(), ModelError> {
    register_item(open, started.location.item_index, started.item_id, limits)?;
    if open.blocks.contains_key(&started.location) {
        return Err(invalid_event("output block started more than once"));
    }
    if open.blocks.len() >= limits.max_blocks() {
        return Err(invalid_event("model output exceeded the block limit"));
    }
    let partial = match started.block {
        BlockStart::Text { phase } => PartialBlock::Text {
            phase,
            bytes: Vec::new(),
        },
        BlockStart::Reasoning { visibility, replay } => PartialBlock::Reasoning {
            visibility: Some(visibility),
            replay,
            bytes: Vec::new(),
        },
        BlockStart::ReasoningPending { replay } => PartialBlock::Reasoning {
            visibility: None,
            replay,
            bytes: Vec::new(),
        },
        BlockStart::ToolCall {
            call_id,
            tool,
            owner,
            state,
            correlation,
        } => PartialBlock::ToolCall {
            call_id,
            tool,
            owner,
            state,
            correlation,
            bytes: Vec::new(),
        },
        BlockStart::Refusal => PartialBlock::Refusal(Vec::new()),
        BlockStart::Complete(block) => PartialBlock::Complete(block),
    };
    if partial.retained_bytes() > limits.max_block_bytes() {
        return Err(invalid_event("model output block exceeded the byte limit"));
    }
    open.blocks.insert(started.location, partial);
    Ok(())
}

fn add_citation(
    open: &mut OpenAssembly,
    citation: CitationAdded,
    limits: AssemblerLimits,
) -> Result<(), ModelError> {
    register_item(open, citation.location.item_index, citation.item_id, limits)?;
    if open.blocks.contains_key(&citation.location) {
        return Err(invalid_event("citation location is already occupied"));
    }
    if open.blocks.len() >= limits.max_blocks() {
        return Err(invalid_event("model output exceeded the block limit"));
    }
    let block = ContentBlock::Citation(citation.citation);
    if block.retained_bytes() > limits.max_block_bytes() {
        return Err(invalid_event("citation exceeded the block byte limit"));
    }
    open.blocks
        .insert(citation.location, PartialBlock::Complete(block));
    Ok(())
}

fn register_item(
    open: &mut OpenAssembly,
    index: u32,
    id: EntryId,
    limits: AssemblerLimits,
) -> Result<(), ModelError> {
    if open.item_ids.get(&index).is_some_and(|known| *known != id) {
        return Err(invalid_event("assistant item index changed identity"));
    }
    if !open.item_ids.contains_key(&index) && open.item_ids.len() >= limits.max_items() {
        return Err(invalid_event("model output exceeded the item limit"));
    }
    open.item_ids.entry(index).or_insert(id);
    Ok(())
}

fn append_delta(
    open: &mut OpenAssembly,
    location: OutputLocation,
    bytes: &[u8],
    kind: DeltaKind,
    limits: AssemblerLimits,
) -> Result<(), ModelError> {
    if bytes.is_empty() {
        return Err(invalid_event("empty deltas are not canonical"));
    }
    let Some(block) = open.blocks.get_mut(&location) else {
        return Err(invalid_event("delta arrived before block start"));
    };
    if matches!(kind, DeltaKind::Reasoning) {
        match &*block {
            PartialBlock::Reasoning {
                visibility: None, ..
            } => {
                return Err(invalid_event(
                    "reasoning delta arrived before visibility was bound",
                ));
            }
            PartialBlock::Reasoning {
                visibility: Some(ReasoningVisibility::Visible | ReasoningVisibility::Summary),
                ..
            }
            | PartialBlock::Text { .. }
            | PartialBlock::ToolCall { .. }
            | PartialBlock::Refusal(_)
            | PartialBlock::Complete(_) => {}
            PartialBlock::Reasoning {
                visibility: Some(_),
                ..
            } => {
                return Err(invalid_event("reasoning delta targeted hidden reasoning"));
            }
        }
    }
    let Some(new_retained_bytes) = block.retained_bytes().checked_add(bytes.len()) else {
        return Err(invalid_event("streamed block byte length overflowed"));
    };
    if new_retained_bytes > limits.max_block_bytes() {
        return Err(invalid_event("streamed block exceeded the byte limit"));
    }
    let ((
        DeltaKind::Text,
        PartialBlock::Text { bytes: target, .. } | PartialBlock::Refusal(target),
    )
    | (DeltaKind::Reasoning, PartialBlock::Reasoning { bytes: target, .. })
    | (DeltaKind::ToolInput, PartialBlock::ToolCall { bytes: target, .. })) = (kind, block)
    else {
        return Err(invalid_event("delta kind does not match output block"));
    };
    target.extend_from_slice(bytes);
    Ok(())
}

fn bind_reasoning_replay(
    open: &mut OpenAssembly,
    binding: ReasoningReplayBound,
    limits: AssemblerLimits,
) -> Result<(), ModelError> {
    let ReasoningReplayBound { location, replay } = binding;
    let Some(block) = open.blocks.get_mut(&location) else {
        return Err(invalid_event(
            "reasoning replay binding targeted an unknown block",
        ));
    };
    match block {
        PartialBlock::Reasoning {
            replay: target,
            bytes,
            ..
        } => {
            if target.is_some() {
                return Err(invalid_event("reasoning replay was bound more than once"));
            }
            let Some(retained_bytes) = bytes.len().checked_add(replay.payload().get().len()) else {
                return Err(invalid_event("reasoning block byte length overflowed"));
            };
            if retained_bytes > limits.max_block_bytes() {
                return Err(invalid_event(
                    "reasoning replay binding exceeded the block byte limit",
                ));
            }
            *target = Some(replay);
            Ok(())
        }
        PartialBlock::Complete(_) => Err(invalid_event(
            "reasoning replay binding arrived after block completion",
        )),
        PartialBlock::Text { .. } | PartialBlock::ToolCall { .. } | PartialBlock::Refusal(_) => {
            Err(invalid_event(
                "reasoning replay binding targeted a non-reasoning block",
            ))
        }
    }
}

fn bind_reasoning_visibility(
    open: &mut OpenAssembly,
    binding: ReasoningVisibilityBound,
) -> Result<(), ModelError> {
    let Some(block) = open.blocks.get_mut(&binding.location()) else {
        return Err(invalid_event(
            "reasoning visibility binding targeted an unknown block",
        ));
    };
    match block {
        PartialBlock::Reasoning {
            visibility, bytes, ..
        } => {
            if visibility.is_some() {
                return Err(invalid_event("reasoning visibility was already known"));
            }
            if !bytes.is_empty() {
                return Err(invalid_event(
                    "reasoning visibility was bound after reasoning bytes arrived",
                ));
            }
            *visibility = Some(binding.visibility());
            Ok(())
        }
        PartialBlock::Complete(_) => Err(invalid_event(
            "reasoning visibility binding arrived after block completion",
        )),
        PartialBlock::Text { .. } | PartialBlock::ToolCall { .. } | PartialBlock::Refusal(_) => {
            Err(invalid_event(
                "reasoning visibility binding targeted a non-reasoning block",
            ))
        }
    }
}

fn complete_block(open: &mut OpenAssembly, location: OutputLocation) -> Result<(), ModelError> {
    let Some(partial) = open.blocks.remove(&location) else {
        return Err(invalid_event("block completed before it started"));
    };
    if matches!(partial, PartialBlock::Complete(_)) {
        open.blocks.insert(location, partial);
        return Err(invalid_event("output block completed more than once"));
    }
    let complete = materialize(partial)?;
    open.blocks
        .insert(location, PartialBlock::Complete(complete));
    Ok(())
}

fn materialize(partial: PartialBlock) -> Result<ContentBlock, ModelError> {
    match partial {
        PartialBlock::Text { phase, bytes } => {
            let text = decode_utf8(bytes, "text block ended with invalid UTF-8")?;
            TextBlock::new(text, phase)
                .map(ContentBlock::Text)
                .map_err(|error| invalid_event(error.to_string()))
        }
        PartialBlock::Reasoning {
            visibility,
            replay,
            bytes,
        } => {
            let Some(visibility) = visibility else {
                return Err(invalid_event(
                    "reasoning block completed before visibility was bound",
                ));
            };
            let text = if bytes.is_empty() {
                None
            } else {
                Some(decode_utf8(
                    bytes,
                    "reasoning block ended with invalid UTF-8",
                )?)
            };
            ReasoningBlock::new(visibility, text, replay)
                .map(ContentBlock::Reasoning)
                .map_err(|error| invalid_event(error.to_string()))
        }
        PartialBlock::ToolCall {
            call_id,
            tool,
            owner,
            state,
            correlation,
            bytes,
        } => {
            let arguments = decode_utf8(bytes, "tool input ended with invalid UTF-8")?;
            let block = ToolCallBlock::new(call_id, tool, owner, state, correlation, arguments)
                .map_err(|error| invalid_event(error.to_string()))?;
            if block.arguments().is_none() {
                return Err(invalid_event("tool input ended with malformed JSON"));
            }
            Ok(ContentBlock::ToolCall(block))
        }
        PartialBlock::Refusal(bytes) => {
            let text = decode_utf8(bytes, "refusal ended with invalid UTF-8")?;
            RefusalBlock::new(text)
                .map(ContentBlock::Refusal)
                .map_err(|error| invalid_event(error.to_string()))
        }
        PartialBlock::Complete(block) => Ok(block),
    }
}

fn decode_utf8(bytes: Vec<u8>, message: &'static str) -> Result<String, ModelError> {
    String::from_utf8(bytes).map_err(|_| invalid_event(message))
}

fn update_usage(open: &mut OpenAssembly, usage: Usage) -> Result<(), ModelError> {
    if open
        .usage
        .is_some_and(|earlier| !usage.is_monotonic_after(earlier))
    {
        return Err(invalid_event(
            "usage updates must be cumulative and monotonic",
        ));
    }
    open.usage = Some(usage);
    Ok(())
}

fn assemble_items(open: &OpenAssembly) -> Result<Vec<ConversationItem>, ModelError> {
    if open
        .blocks
        .values()
        .any(|block| !matches!(block, PartialBlock::Complete(_)))
    {
        return Err(invalid_event(
            "terminal aggregate arrived with an open block",
        ));
    }
    let item_count = open.item_ids.len();
    let mut output = Vec::with_capacity(item_count);
    for item_index in 0..item_count {
        let item_index = u32::try_from(item_index)
            .map_err(|_| invalid_event("too many assistant output items"))?;
        let Some(item_id) = open.item_ids.get(&item_index) else {
            return Err(invalid_event("assistant item indices are not contiguous"));
        };
        let mut blocks = Vec::new();
        for (location, partial) in open
            .blocks
            .range(OutputLocation::new(item_index, 0)..=OutputLocation::new(item_index, u32::MAX))
        {
            let expected = u32::try_from(blocks.len())
                .map_err(|_| invalid_event("too many blocks in assistant output item"))?;
            if location.block_index != expected {
                return Err(invalid_event("output block indices are not contiguous"));
            }
            let PartialBlock::Complete(block) = partial else {
                return Err(invalid_event(
                    "terminal aggregate arrived with an open block",
                ));
            };
            blocks.push(block.clone());
        }
        let item =
            ConversationItem::new(*item_id, Actor::Assistant, blocks, Provenance::ModelOutput)
                .map_err(|error| invalid_event(error.to_string()))?;
        output.push(item);
    }
    Ok(output)
}

fn invalid_event(message: impl Into<String>) -> ModelError {
    ModelError::new(ModelErrorKind::InvalidEvent, message)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests;
