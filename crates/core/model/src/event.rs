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

mod retry;
pub use retry::{
    ModelDiagnostic, ModelNetworkFailure, ModelNetworkTransmission, ModelRetry, ModelRetryCause,
    ModelRetryDeclineReason, ModelRetryLoss, ModelRetryPhase, ModelStreamActivity,
    ModelTransportFault, PromptCacheDiagnosis, PromptCacheMissReason,
};
mod assembly;

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
