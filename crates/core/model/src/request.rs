//! Canonical tool definitions, portable policies, and owned model requests.

use std::collections::BTreeSet;
use std::fmt;
use std::mem::size_of;
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::semantic_json::canonicalize_normalized_prevalidated;
use crate::{
    ConfiguredTool, ContextAdmissionRequirement, ConversationView, Digest, DigestDomain,
    DigestError, EffectiveProviderConfigurationId, HistoryError, HostedTool,
    ProcessingTierDisposition, SelectedProviderConfigurationId, SemanticJsonError,
    ToolCallerDisposition, ToolExecutionOwner, ToolName,
};

mod policy;
mod projection;
mod tool;

pub use policy::{
    CachePolicy, CapabilityRequirement, GenerationLimits, InstructionBlock, InstructionSource,
    NativeOptions, OutputFormat, OutputPolicy, ReasoningEffort, ReasoningPolicy, RequestMetadata,
    ToolChoice,
};
pub use projection::{
    CompiledToolProjectionV2, ProjectedCustomToolFormatV2, ProjectedCustomToolGrammarSyntaxV2,
    ProjectedToolDefinitionV2, ProjectedToolPresentationV2, ToolProjectionTargetV2,
};
pub use tool::{
    ModelContentContractV1, ToolConcurrency, ToolDefinition, ToolEffect, ToolInputContract,
    ToolOutputContract, Utf8TextContractV1,
};

/// An error constructing a canonical request value.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum RequestError {
    /// A required field was empty.
    #[error("{field} cannot be empty")]
    Empty {
        /// The semantic field that was empty.
        field: &'static str,
    },
    /// A text field exceeded its portable byte budget.
    #[error("{field} exceeds {maximum} bytes")]
    TooLong {
        /// The semantic field that exceeded its budget.
        field: &'static str,
        /// The maximum accepted size.
        maximum: usize,
    },
    /// A JSON Schema value was invalid or outside the minimal subset.
    #[error("invalid JSON Schema: {reason}")]
    InvalidSchema {
        /// A stable human-readable reason.
        reason: String,
    },
    /// Semantic JSON or portable schema validation failed.
    #[error(transparent)]
    SemanticJson(#[from] SemanticJsonError),
    /// A request contained duplicate tool names.
    #[error("duplicate tool name: {0}")]
    DuplicateTool(ToolName),
    /// A specific tool choice did not name an exposed tool.
    #[error("selected tool is not exposed: {0}")]
    UnknownTool(ToolName),
    /// Tool use was required but the request exposed no tools.
    #[error("tool use cannot be required without an exposed tool")]
    RequiredToolWithoutTools,
    /// Too many stop sequences were supplied.
    #[error("at most 16 stop sequences are portable")]
    TooManyStopSequences,
    /// Too many ordered instructions were supplied.
    #[error("at most 256 instruction blocks are portable")]
    TooManyInstructions,
    /// Too many model-facing tools were supplied.
    #[error("at most 128 tool definitions are portable")]
    TooManyTools,
    /// Hosted-tool selection is duplicated or disagrees with configured caller dispositions.
    #[error("provider-hosted tool selection is invalid")]
    InvalidHostedTools,
    /// Too many native-option namespaces were supplied.
    #[error("at most 64 native-option namespaces are accepted")]
    TooManyNativeOptions,
    /// A JSON Schema exceeded the supported structural depth.
    #[error("JSON Schema exceeds the maximum depth of 64")]
    SchemaTooDeep,
    /// Canonical JSON serialization failed.
    #[error("canonical JSON serialization failed: {reason}")]
    CanonicalJson {
        /// The serialization failure.
        reason: String,
    },
    /// A digest could not be computed.
    #[error(transparent)]
    Digest(#[from] DigestError),
    /// A serialized integrity digest did not match reconstructed content.
    #[error("serialized {field} digest does not match its content")]
    IntegrityMismatch {
        /// The semantic value whose integrity check failed.
        field: &'static str,
    },
    /// Tool concurrency metadata was empty, duplicated, or oversized.
    #[error("invalid tool concurrency metadata: {reason}")]
    InvalidToolConcurrency {
        /// Stable reason suitable for configuration diagnostics.
        reason: &'static str,
    },
    /// Canonical history or its bounded projection was invalid.
    #[error(transparent)]
    History(#[from] HistoryError),
    /// A supplied resolved harness-plan identity used the wrong digest domain.
    #[error("resolved harness-plan identity is invalid")]
    InvalidHarnessPlanIdentity,
    /// A compiled V2 tool projection was malformed or did not match the request binding.
    #[error("invalid compiled tool projection: {reason}")]
    InvalidToolProjection {
        /// Stable content-free reason.
        reason: &'static str,
    },
}

#[derive(Debug, PartialEq)]
struct StableRequestPrefixInner {
    selected_provider_configuration: SelectedProviderConfigurationId,
    effective_provider_configuration: EffectiveProviderConfigurationId,
    processing_tier: ProcessingTierDisposition,
    instructions: Vec<InstructionBlock>,
    tools: Vec<ConfiguredTool>,
    hosted_tools: Vec<HostedTool>,
    tool_projection: Option<CompiledToolProjectionV2>,
    output: OutputPolicy,
    reasoning: ReasoningPolicy,
    tool_choice: ToolChoice,
    cache: CachePolicy,
    limits: GenerationLimits,
    context_admission_requirement: ContextAdmissionRequirement,
    native: NativeOptions,
    resolved_harness_plan: Option<Digest>,
    identity: Digest,
    reuse_identity: Digest,
    canonical_bytes: Arc<[u8]>,
}

/// Immutable provider-neutral request fields shared unchanged between rounds.
#[derive(Clone, Debug, PartialEq)]
pub struct StableRequestPrefix {
    inner: Arc<StableRequestPrefixInner>,
}

#[derive(Deserialize, Serialize)]
struct StableRequestPrefixWire {
    selected_provider_configuration: SelectedProviderConfigurationId,
    effective_provider_configuration: EffectiveProviderConfigurationId,
    processing_tier: ProcessingTierDisposition,
    instructions: Vec<InstructionBlock>,
    tools: Vec<ConfiguredTool>,
    hosted_tools: Vec<HostedTool>,
    tool_projection: Option<CompiledToolProjectionV2>,
    output: OutputPolicy,
    reasoning: ReasoningPolicy,
    tool_choice: ToolChoice,
    cache: CachePolicy,
    limits: GenerationLimits,
    context_admission_requirement: ContextAdmissionRequirement,
    native: NativeOptions,
    resolved_harness_plan: Option<Digest>,
    identity: Digest,
}

/// Separates [`StableRequestPrefix::reuse_identity`] from the full identity,
/// which hashes one part where this hashes two.
const STABLE_PREFIX_REUSE_LABEL: &[u8] = b"plak-stable-prefix-reuse-without-tool-choice";

#[derive(Serialize)]
struct StableRequestPrefixContent<'a> {
    selected_provider_configuration: SelectedProviderConfigurationId,
    effective_provider_configuration: EffectiveProviderConfigurationId,
    processing_tier: &'a ProcessingTierDisposition,
    instructions: &'a [InstructionBlock],
    tools: &'a [ConfiguredTool],
    hosted_tools: &'a [HostedTool],
    tool_projection: Option<&'a CompiledToolProjectionV2>,
    output: &'a OutputPolicy,
    reasoning: ReasoningPolicy,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'a ToolChoice>,
    cache: CachePolicy,
    limits: &'a GenerationLimits,
    context_admission_requirement: ContextAdmissionRequirement,
    native: &'a NativeOptions,
    resolved_harness_plan: Option<Digest>,
}

impl StableRequestPrefix {
    /// Validates and freezes all request fields that remain stable across rounds.
    ///
    /// # Errors
    ///
    /// Returns an error for duplicate tools, unsatisfiable tool selection,
    /// structural limits, or canonical serialization failure.
    #[allow(
        clippy::too_many_arguments,
        reason = "the fields are the closed stable request algebra"
    )]
    pub fn new(
        selected_provider_configuration: SelectedProviderConfigurationId,
        effective_provider_configuration: EffectiveProviderConfigurationId,
        processing_tier: ProcessingTierDisposition,
        instructions: Vec<InstructionBlock>,
        tools: Vec<ConfiguredTool>,
        hosted_tools: Vec<HostedTool>,
        output: OutputPolicy,
        reasoning: ReasoningPolicy,
        tool_choice: ToolChoice,
        cache: CachePolicy,
        limits: GenerationLimits,
        native: NativeOptions,
        resolved_harness_plan: Option<Digest>,
    ) -> Result<Self, RequestError> {
        Self::seal(
            selected_provider_configuration,
            effective_provider_configuration,
            processing_tier,
            instructions,
            tools,
            hosted_tools,
            None,
            output,
            reasoning,
            tool_choice,
            cache,
            limits,
            ContextAdmissionRequirement::AnyBounded,
            native,
            resolved_harness_plan,
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the fields are the closed stable request algebra"
    )]
    fn seal(
        selected_provider_configuration: SelectedProviderConfigurationId,
        effective_provider_configuration: EffectiveProviderConfigurationId,
        processing_tier: ProcessingTierDisposition,
        instructions: Vec<InstructionBlock>,
        tools: Vec<ConfiguredTool>,
        hosted_tools: Vec<HostedTool>,
        tool_projection: Option<CompiledToolProjectionV2>,
        output: OutputPolicy,
        reasoning: ReasoningPolicy,
        tool_choice: ToolChoice,
        cache: CachePolicy,
        limits: GenerationLimits,
        context_admission_requirement: ContextAdmissionRequirement,
        native: NativeOptions,
        resolved_harness_plan: Option<Digest>,
    ) -> Result<Self, RequestError> {
        validate_stable_prefix(&instructions, &tools, &hosted_tools, &tool_choice, &native)?;
        if let Some(projection) = &tool_projection {
            if projection.validate().is_err() {
                return Err(RequestError::InvalidToolProjection {
                    reason: "projection catalog invalid",
                });
            }
            if projection.target().effective_provider_configuration()
                != effective_provider_configuration
            {
                return Err(RequestError::InvalidToolProjection {
                    reason: "projection effective-configuration drift",
                });
            }
            if projection.definitions().iter().any(|entry| {
                entry
                    .covered_canonical_names()
                    .iter()
                    .any(|covered| !tools.iter().any(|tool| tool.definition().name() == covered))
            }) {
                return Err(RequestError::InvalidToolProjection {
                    reason: "projection covers unbound canonical tools",
                });
            }
        }
        if resolved_harness_plan.is_some_and(|identity| {
            identity.domain() != DigestDomain::ResolvedHarnessPlan || identity.version() == 0
        }) {
            return Err(RequestError::InvalidHarnessPlanIdentity);
        }
        let mut content = StableRequestPrefixContent {
            selected_provider_configuration,
            effective_provider_configuration,
            processing_tier: &processing_tier,
            instructions: &instructions,
            tools: &tools,
            hosted_tools: &hosted_tools,
            tool_projection: tool_projection.as_ref(),
            output: &output,
            reasoning,
            tool_choice: Some(&tool_choice),
            cache,
            limits: &limits,
            context_admission_requirement,
            native: &native,
            resolved_harness_plan,
        };
        let canonical_bytes = canonical_serialize(&content)?;
        let identity = Digest::sha256(DigestDomain::StableRequestPrefix, 6, &[&canonical_bytes])?;
        content.tool_choice = None;
        let reuse_identity = Digest::sha256(
            DigestDomain::StableRequestPrefix,
            6,
            &[STABLE_PREFIX_REUSE_LABEL, &canonical_serialize(&content)?],
        )?;
        Ok(Self {
            inner: Arc::new(StableRequestPrefixInner {
                selected_provider_configuration,
                effective_provider_configuration,
                processing_tier,
                instructions,
                tools,
                hosted_tools,
                tool_projection,
                output,
                reasoning,
                tool_choice,
                cache,
                limits,
                context_admission_requirement,
                native,
                resolved_harness_plan,
                identity,
                reuse_identity,
                canonical_bytes: canonical_bytes.into(),
            }),
        })
    }

    /// Returns ordered stable instructions.
    #[must_use]
    pub fn instructions(&self) -> &[InstructionBlock] {
        &self.inner.instructions
    }

    /// Returns the complete caller-selected provider configuration identity.
    #[must_use]
    pub fn selected_provider_configuration_id(&self) -> SelectedProviderConfigurationId {
        self.inner.selected_provider_configuration
    }

    /// Returns the complete compiled provider configuration identity.
    #[must_use]
    pub fn effective_provider_configuration_id(&self) -> EffectiveProviderConfigurationId {
        self.inner.effective_provider_configuration
    }

    /// Returns the exact provider processing/service-tier selection.
    #[must_use]
    pub fn processing_tier(&self) -> &ProcessingTierDisposition {
        &self.inner.processing_tier
    }

    /// Returns ordered configured function tools.
    #[must_use]
    pub fn tools(&self) -> &[ConfiguredTool] {
        &self.inner.tools
    }

    /// Returns exact identity-bearing provider-hosted tool selections.
    #[must_use]
    pub fn hosted_tools(&self) -> &[HostedTool] {
        &self.inner.hosted_tools
    }

    /// Returns an exact precompiled V2 view when the upstream registry supplied one.
    #[must_use]
    pub fn tool_projection(&self) -> Option<&CompiledToolProjectionV2> {
        self.inner.tool_projection.as_ref()
    }

    /// Binds an upstream-compiled V2 view into this immutable prefix.
    ///
    /// # Errors
    ///
    /// Returns an error when the projection does not bind this prefix's exact
    /// effective provider configuration and canonical tools.
    pub fn with_tool_projection(
        &self,
        projection: CompiledToolProjectionV2,
    ) -> Result<Self, RequestError> {
        Self::seal(
            self.selected_provider_configuration_id(),
            self.effective_provider_configuration_id(),
            self.processing_tier().clone(),
            self.instructions().to_vec(),
            self.tools().to_vec(),
            self.hosted_tools().to_vec(),
            Some(projection),
            self.output().clone(),
            self.reasoning(),
            self.tool_choice().clone(),
            self.cache(),
            self.limits().clone(),
            self.context_admission_requirement(),
            self.native().clone(),
            self.resolved_harness_plan_identity(),
        )
    }

    /// Returns stable output intent.
    #[must_use]
    pub fn output(&self) -> &OutputPolicy {
        &self.inner.output
    }

    /// Returns stable reasoning intent.
    #[must_use]
    pub fn reasoning(&self) -> ReasoningPolicy {
        self.inner.reasoning
    }

    /// Returns stable tool-selection intent.
    #[must_use]
    pub fn tool_choice(&self) -> &ToolChoice {
        &self.inner.tool_choice
    }

    /// Returns stable cache intent.
    #[must_use]
    pub fn cache(&self) -> CachePolicy {
        self.inner.cache
    }

    /// Returns stable portable generation limits.
    #[must_use]
    pub fn limits(&self) -> &GenerationLimits {
        &self.inner.limits
    }

    /// Returns the `evidence_class` policy required before provider-visible work.
    #[must_use]
    pub fn context_admission_requirement(&self) -> ContextAdmissionRequirement {
        self.inner.context_admission_requirement
    }

    /// Rebinds this prefix to an exact context-evidence policy and fresh identity.
    ///
    /// # Errors
    ///
    /// Returns an error only if canonical identity construction fails.
    pub fn with_context_admission_requirement(
        &self,
        requirement: ContextAdmissionRequirement,
    ) -> Result<Self, RequestError> {
        Self::seal(
            self.selected_provider_configuration_id(),
            self.effective_provider_configuration_id(),
            self.processing_tier().clone(),
            self.instructions().to_vec(),
            self.tools().to_vec(),
            self.hosted_tools().to_vec(),
            self.tool_projection().cloned(),
            self.output().clone(),
            self.reasoning(),
            self.tool_choice().clone(),
            self.cache(),
            self.limits().clone(),
            requirement,
            self.native().clone(),
            self.resolved_harness_plan_identity(),
        )
    }

    /// Returns stable advanced namespaced options.
    #[must_use]
    pub fn native(&self) -> &NativeOptions {
        &self.inner.native
    }

    /// Returns the core-resolved harness-plan identity bound at construction.
    #[must_use]
    pub fn resolved_harness_plan_identity(&self) -> Option<Digest> {
        self.inner.resolved_harness_plan
    }

    /// Returns the complete versioned prefix identity.
    #[must_use]
    pub fn identity(&self) -> Digest {
        self.inner.identity
    }

    /// Returns the identity of this prefix with tool selection left out.
    ///
    /// Native continuation state binds this identity, not [`Self::identity`]:
    /// a round that only changes `tool_choice` (the operation-budget wrap-up)
    /// still continues the state the previous round produced, while every
    /// other field difference makes the state a different prefix's.
    #[must_use]
    pub fn reuse_identity(&self) -> Digest {
        self.inner.reuse_identity
    }

    /// Returns exact deterministic bytes for every stable request field.
    #[must_use]
    pub fn canonical_bytes(&self) -> &[u8] {
        &self.inner.canonical_bytes
    }

    /// Returns a conservative retained-memory bound for this shared prefix.
    #[must_use]
    pub fn retained_bytes_upper_bound(&self) -> usize {
        self.inner
            .canonical_bytes
            .len()
            .saturating_mul(2)
            .saturating_add(size_of::<StableRequestPrefixInner>())
    }

    /// Reports whether two handles share the exact immutable prefix allocation.
    #[must_use]
    pub fn shares_storage_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Names the fields whose values differ from `other`, without content.
    #[must_use]
    pub fn drift_from(&self, other: &Self) -> StableRequestPrefixDrift {
        use StableRequestPrefixField as Field;
        let (a, b) = (&*self.inner, &*other.inner);
        [
            (
                a.selected_provider_configuration != b.selected_provider_configuration,
                Field::SelectedProviderConfiguration,
            ),
            (
                a.effective_provider_configuration != b.effective_provider_configuration,
                Field::EffectiveProviderConfiguration,
            ),
            (
                a.processing_tier != b.processing_tier,
                Field::ProcessingTier,
            ),
            (a.instructions != b.instructions, Field::Instructions),
            (a.tools != b.tools, Field::Tools),
            (a.hosted_tools != b.hosted_tools, Field::HostedTools),
            (
                a.tool_projection != b.tool_projection,
                Field::ToolProjection,
            ),
            (a.output != b.output, Field::Output),
            (a.reasoning != b.reasoning, Field::Reasoning),
            (a.tool_choice != b.tool_choice, Field::ToolChoice),
            (a.cache != b.cache, Field::Cache),
            (a.limits != b.limits, Field::Limits),
            (
                a.context_admission_requirement != b.context_admission_requirement,
                Field::ContextAdmissionRequirement,
            ),
            (a.native != b.native, Field::Native),
            (
                a.resolved_harness_plan != b.resolved_harness_plan,
                Field::ResolvedHarnessPlan,
            ),
        ]
        .into_iter()
        .filter(|(differs, _)| *differs)
        .fold(StableRequestPrefixDrift::default(), |drift, (_, field)| {
            drift.with(field)
        })
    }
}

/// One field of a [`StableRequestPrefix`], named for content-free diagnostics.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum StableRequestPrefixField {
    /// The caller-selected provider configuration identity.
    SelectedProviderConfiguration,
    /// The compiled provider configuration identity.
    EffectiveProviderConfiguration,
    /// The provider processing/service-tier selection.
    ProcessingTier,
    /// Ordered stable instructions.
    Instructions,
    /// Locally executed tool definitions.
    Tools,
    /// Provider-hosted tools.
    HostedTools,
    /// The compiled provider-visible tool projection.
    ToolProjection,
    /// Output intent.
    Output,
    /// Reasoning intent.
    Reasoning,
    /// Tool-selection intent.
    ToolChoice,
    /// Cache intent.
    Cache,
    /// Generation limits.
    Limits,
    /// The required context-evidence class.
    ContextAdmissionRequirement,
    /// Provider-native options.
    Native,
    /// The resolved harness-plan identity.
    ResolvedHarnessPlan,
}

impl StableRequestPrefixField {
    /// Every field, in declaration order.
    pub const ALL: [Self; 15] = [
        Self::SelectedProviderConfiguration,
        Self::EffectiveProviderConfiguration,
        Self::ProcessingTier,
        Self::Instructions,
        Self::Tools,
        Self::HostedTools,
        Self::ToolProjection,
        Self::Output,
        Self::Reasoning,
        Self::ToolChoice,
        Self::Cache,
        Self::Limits,
        Self::ContextAdmissionRequirement,
        Self::Native,
        Self::ResolvedHarnessPlan,
    ];

    /// Stable diagnostic spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SelectedProviderConfiguration => "selected-provider-configuration",
            Self::EffectiveProviderConfiguration => "effective-provider-configuration",
            Self::ProcessingTier => "processing-tier",
            Self::Instructions => "instructions",
            Self::Tools => "tools",
            Self::HostedTools => "hosted-tools",
            Self::ToolProjection => "tool-projection",
            Self::Output => "output",
            Self::Reasoning => "reasoning",
            Self::ToolChoice => "tool-choice",
            Self::Cache => "cache",
            Self::Limits => "limits",
            Self::ContextAdmissionRequirement => "context-admission-requirement",
            Self::Native => "native",
            Self::ResolvedHarnessPlan => "harness-plan",
        }
    }

    const fn bit(self) -> u16 {
        1 << self as u16
    }
}

/// The set of [`StableRequestPrefixField`]s that differ between two prefixes.
///
/// `Copy` and content-free, so it can travel in telemetry and error text.
/// Displays as the comma-separated field names in declaration order.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct StableRequestPrefixDrift {
    fields: u16,
}

impl StableRequestPrefixDrift {
    /// Returns this set with `field` added.
    #[must_use]
    pub const fn with(self, field: StableRequestPrefixField) -> Self {
        Self {
            fields: self.fields | field.bit(),
        }
    }

    /// Reports whether no field differs.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.fields == 0
    }

    /// Reports whether `field` differs.
    #[must_use]
    pub const fn contains(self, field: StableRequestPrefixField) -> bool {
        self.fields & field.bit() != 0
    }

    /// Iterates the differing fields in declaration order.
    pub fn fields(self) -> impl Iterator<Item = StableRequestPrefixField> {
        StableRequestPrefixField::ALL
            .into_iter()
            .filter(move |field| self.contains(*field))
    }
}

impl fmt::Display for StableRequestPrefixDrift {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return formatter.write_str("none");
        }
        for (index, field) in self.fields().enumerate() {
            if index > 0 {
                formatter.write_str(",")?;
            }
            formatter.write_str(field.as_str())?;
        }
        Ok(())
    }
}

impl Serialize for StableRequestPrefix {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        StableRequestPrefixWire {
            selected_provider_configuration: self.selected_provider_configuration_id(),
            effective_provider_configuration: self.effective_provider_configuration_id(),
            processing_tier: self.processing_tier().clone(),
            instructions: self.instructions().to_vec(),
            tools: self.tools().to_vec(),
            hosted_tools: self.hosted_tools().to_vec(),
            tool_projection: self.tool_projection().cloned(),
            output: self.output().clone(),
            reasoning: self.reasoning(),
            tool_choice: self.tool_choice().clone(),
            cache: self.cache(),
            limits: self.limits().clone(),
            context_admission_requirement: self.context_admission_requirement(),
            native: self.native().clone(),
            resolved_harness_plan: self.resolved_harness_plan_identity(),
            identity: self.identity(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for StableRequestPrefix {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = StableRequestPrefixWire::deserialize(deserializer)?;
        let prefix = Self::seal(
            wire.selected_provider_configuration,
            wire.effective_provider_configuration,
            wire.processing_tier,
            wire.instructions,
            wire.tools,
            wire.hosted_tools,
            wire.tool_projection,
            wire.output,
            wire.reasoning,
            wire.tool_choice,
            wire.cache,
            wire.limits,
            wire.context_admission_requirement,
            wire.native,
            wire.resolved_harness_plan,
        )
        .map_err(serde::de::Error::custom)?;
        if prefix.identity() != wire.identity {
            return Err(serde::de::Error::custom(RequestError::IntegrityMismatch {
                field: "stable request prefix",
            }));
        }
        Ok(prefix)
    }
}

/// Who initiated a root turn, independent of subsequent steering or tool rounds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnOrigin {
    /// A user submitted a prompt.
    UserPrompt,
    /// The host woke the agent (child results, or host job notices).
    HostWake,
}

impl TurnOrigin {
    /// Content-free telemetry spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UserPrompt => "user_prompt",
            Self::HostWake => "host_wake",
        }
    }
}

/// A private-fielded, validated, provider-neutral model request.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelRequest {
    prefix: StableRequestPrefix,
    input: ConversationView,
    metadata: RequestMetadata,
    model_input_identity: Digest,
    identity: Digest,
    canonical_bytes: Arc<[u8]>,
}

#[derive(Serialize)]
struct ModelRequestWireRef<'a> {
    prefix: &'a StableRequestPrefix,
    input: &'a ConversationView,
    metadata: RequestMetadata,
    identity: Digest,
}

#[derive(Serialize)]
struct ModelRequestIdentityContent {
    prefix: Digest,
    input: Digest,
    metadata: RequestMetadata,
}

#[derive(Serialize)]
struct ModelInputIdentityContent {
    prefix: Digest,
    input: Digest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelRequestWire {
    prefix: StableRequestPrefix,
    input: ConversationView,
    metadata: RequestMetadata,
    identity: Digest,
}

impl<'de> Deserialize<'de> for ModelRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ModelRequestWire::deserialize(deserializer)?;
        let request = Self::freeze(wire.prefix, wire.input, wire.metadata)
            .map_err(serde::de::Error::custom)?;
        if request.identity() != wire.identity {
            return Err(serde::de::Error::custom(RequestError::IntegrityMismatch {
                field: "model request",
            }));
        }
        Ok(request)
    }
}

impl Serialize for ModelRequest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        ModelRequestWireRef {
            prefix: &self.prefix,
            input: &self.input,
            metadata: self.metadata,
            identity: self.identity,
        }
        .serialize(serializer)
    }
}

impl ModelRequest {
    /// Derives the root origin from its most recent intent, not later steering.
    /// Returns `None` for projections without a retained prompt identity.
    #[must_use]
    pub fn turn_origin(&self) -> Option<TurnOrigin> {
        Some(
            if self.turn_prompt()?.provenance() == crate::Provenance::UserInput {
                TurnOrigin::UserPrompt
            } else {
                TurnOrigin::HostWake
            },
        )
    }

    /// The item that opened the current root turn: the prompt or host-wake
    /// instruction in the most recent intent segment, never later steering or
    /// a note appended inside the turn. A projection without an intent
    /// segment falls back to its last user text.
    #[must_use]
    pub fn turn_prompt(&self) -> Option<&crate::ConversationItem> {
        let prompt = |item: &&crate::ConversationItem| {
            item.actor() == crate::Actor::User
                && matches!(
                    item.provenance(),
                    crate::Provenance::UserInput | crate::Provenance::Instruction
                )
                && item
                    .blocks()
                    .iter()
                    .any(|block| matches!(block, crate::ContentBlock::Text(_)))
        };
        let intent = self
            .input
            .history()
            .segments()
            .iter()
            .chain(self.input.working().segments())
            .filter(|segment| segment.kind() == crate::HistorySegmentKind::RecentIntent)
            .last();
        if let Some(intent) = intent {
            intent.items().iter().rev().find(prompt)
        } else {
            self.input.items().into_iter().filter(prompt).last()
        }
    }

    /// Starts a validated request builder with owned history and metadata.
    #[must_use]
    pub fn builder(
        selected_provider_configuration: SelectedProviderConfigurationId,
        effective_provider_configuration: EffectiveProviderConfigurationId,
        input: ConversationView,
        metadata: RequestMetadata,
    ) -> ModelRequestBuilder {
        ModelRequestBuilder {
            selected_provider_configuration,
            effective_provider_configuration,
            processing_tier: ProcessingTierDisposition::Standard,
            instructions: Vec::new(),
            input,
            tools: Vec::new(),
            hosted_tools: Vec::new(),
            output: OutputPolicy::default(),
            reasoning: ReasoningPolicy::default(),
            tool_choice: ToolChoice::default(),
            cache: CachePolicy::default(),
            limits: GenerationLimits::default(),
            context_admission_requirement: ContextAdmissionRequirement::AnyBounded,
            metadata,
            native: NativeOptions::default(),
            resolved_harness_plan: None,
        }
    }

    /// Constructs a request from an already validated shared prefix and a new projection.
    ///
    /// This is the normal round boundary when canonical history advances: it
    /// preserves the immutable prefix allocation while validating the complete
    /// replacement projection and deriving fresh request identity bytes.
    ///
    /// # Errors
    ///
    /// Returns an error when the projection contains an incomplete protected
    /// group or deterministic identity construction fails.
    pub fn from_parts(
        prefix: StableRequestPrefix,
        input: ConversationView,
        metadata: RequestMetadata,
    ) -> Result<Self, RequestError> {
        Self::freeze(prefix, input, metadata)
    }

    fn freeze(
        prefix: StableRequestPrefix,
        input: ConversationView,
        metadata: RequestMetadata,
    ) -> Result<Self, RequestError> {
        input.validate_model_visible()?;
        let model_input_bytes = canonical_serialize(&ModelInputIdentityContent {
            prefix: prefix.identity(),
            input: input.identity(),
        })?;
        let model_input_identity =
            Digest::sha256(DigestDomain::RequestPlan, 7, &[&model_input_bytes])?;
        let canonical_bytes = canonical_serialize(&ModelRequestIdentityContent {
            prefix: prefix.identity(),
            input: input.identity(),
            metadata,
        })?;
        let identity = Digest::sha256(DigestDomain::RequestPlan, 6, &[&canonical_bytes])?;
        Ok(Self {
            prefix,
            input,
            metadata,
            model_input_identity,
            identity,
            canonical_bytes: canonical_bytes.into(),
        })
    }

    /// Returns the immutable stable prefix shared by this request.
    #[must_use]
    pub const fn stable_prefix(&self) -> &StableRequestPrefix {
        &self.prefix
    }

    /// Constructs a new round while sharing the complete stable prefix and history projection.
    ///
    /// # Errors
    ///
    /// Returns an error only if deterministic request identity construction fails.
    pub fn for_round(&self, metadata: RequestMetadata) -> Result<Self, RequestError> {
        Self::from_parts(self.prefix.clone(), self.input.clone(), metadata)
    }

    /// Returns the complete deterministic request identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }

    /// Returns the exact identity of every model-visible input fact.
    ///
    /// Unlike [`Self::identity`], this excludes attempt metadata that is not
    /// sent to the model.
    #[must_use]
    pub const fn model_input_identity(&self) -> Digest {
        self.model_input_identity
    }

    /// Returns deterministic identity bytes for all request-affecting artifacts.
    #[must_use]
    pub fn canonical_bytes(&self) -> &[u8] {
        &self.canonical_bytes
    }

    /// Returns a conservative retained-memory bound without flattening history.
    #[must_use]
    pub fn retained_bytes_upper_bound(&self) -> usize {
        self.prefix
            .retained_bytes_upper_bound()
            .saturating_add(self.input.retained_bytes_upper_bound())
            .saturating_add(self.canonical_bytes.len())
    }

    /// Returns ordered instructions.
    #[must_use]
    pub fn instructions(&self) -> &[InstructionBlock] {
        self.prefix.instructions()
    }

    /// Returns the immutable conversation snapshot.
    #[must_use]
    pub const fn input(&self) -> &ConversationView {
        &self.input
    }

    /// Returns ordered configured function tools.
    #[must_use]
    pub fn tools(&self) -> &[ConfiguredTool] {
        self.prefix.tools()
    }

    /// Returns exact identity-bearing provider-hosted tool selections.
    #[must_use]
    pub fn hosted_tools(&self) -> &[HostedTool] {
        self.prefix.hosted_tools()
    }

    /// Returns the exact upstream-compiled V2 provider-visible tool view.
    #[must_use]
    pub fn tool_projection(&self) -> Option<&CompiledToolProjectionV2> {
        self.prefix.tool_projection()
    }

    /// The runtime owner a protocol decodes a call to the provider-visible
    /// `exposed` tool with: the MCP server its configured tool names, or the
    /// local runtime for every other locally dispatched name.
    #[must_use]
    pub fn local_tool_owner(&self, exposed: &ToolName) -> ToolExecutionOwner {
        let canonical = self
            .tool_projection()
            .and_then(|projection| {
                projection
                    .definitions()
                    .iter()
                    .find(|entry| entry.exposed_name() == exposed)
            })
            .map_or(exposed, ProjectedToolDefinitionV2::canonical_name);
        self.tools()
            .iter()
            .find(|tool| tool.definition().name() == canonical)
            .and_then(ConfiguredTool::local_owner)
            .unwrap_or(ToolExecutionOwner::Local)
    }

    /// Returns output intent.
    #[must_use]
    pub fn output(&self) -> &OutputPolicy {
        &self.prefix.inner.output
    }

    /// Returns reasoning intent.
    #[must_use]
    pub fn reasoning(&self) -> ReasoningPolicy {
        self.prefix.inner.reasoning
    }

    /// Returns tool-selection intent.
    #[must_use]
    pub fn tool_choice(&self) -> &ToolChoice {
        &self.prefix.inner.tool_choice
    }

    /// Returns cache intent.
    #[must_use]
    pub fn cache(&self) -> CachePolicy {
        self.prefix.inner.cache
    }

    /// Returns portable generation limits.
    #[must_use]
    pub fn limits(&self) -> &GenerationLimits {
        &self.prefix.inner.limits
    }

    /// Returns stable request metadata.
    #[must_use]
    pub const fn metadata(&self) -> RequestMetadata {
        self.metadata
    }

    /// Returns advanced namespaced options.
    #[must_use]
    pub fn native(&self) -> &NativeOptions {
        &self.prefix.inner.native
    }

    /// Returns the core-resolved harness-plan identity carried by this request.
    #[must_use]
    pub fn resolved_harness_plan_identity(&self) -> Option<Digest> {
        self.prefix.resolved_harness_plan_identity()
    }

    /// Returns the complete caller-selected provider configuration identity.
    #[must_use]
    pub fn selected_provider_configuration_id(&self) -> SelectedProviderConfigurationId {
        self.prefix.selected_provider_configuration_id()
    }

    /// Returns the complete compiled provider configuration identity.
    #[must_use]
    pub fn effective_provider_configuration_id(&self) -> EffectiveProviderConfigurationId {
        self.prefix.effective_provider_configuration_id()
    }

    /// Returns the exact provider processing/service-tier selection.
    #[must_use]
    pub fn processing_tier(&self) -> &ProcessingTierDisposition {
        self.prefix.processing_tier()
    }

    /// Returns the context-evidence policy bound into this request identity.
    #[must_use]
    pub fn context_admission_requirement(&self) -> ContextAdmissionRequirement {
        self.prefix.context_admission_requirement()
    }
}

/// Builder for a validated [`ModelRequest`].
#[derive(Debug)]
pub struct ModelRequestBuilder {
    selected_provider_configuration: SelectedProviderConfigurationId,
    effective_provider_configuration: EffectiveProviderConfigurationId,
    processing_tier: ProcessingTierDisposition,
    instructions: Vec<InstructionBlock>,
    input: ConversationView,
    tools: Vec<ConfiguredTool>,
    hosted_tools: Vec<HostedTool>,
    output: OutputPolicy,
    reasoning: ReasoningPolicy,
    tool_choice: ToolChoice,
    cache: CachePolicy,
    limits: GenerationLimits,
    context_admission_requirement: ContextAdmissionRequirement,
    metadata: RequestMetadata,
    native: NativeOptions,
    resolved_harness_plan: Option<Digest>,
}

impl ModelRequestBuilder {
    /// Sets the exact provider processing/service tier.
    #[must_use]
    pub fn processing_tier(mut self, processing_tier: ProcessingTierDisposition) -> Self {
        self.processing_tier = processing_tier;
        self
    }

    /// Sets ordered instructions.
    #[must_use]
    pub fn instructions(mut self, instructions: Vec<InstructionBlock>) -> Self {
        self.instructions = instructions;
        self
    }

    /// Sets ordered configured function tools.
    #[must_use]
    pub fn tools(mut self, tools: Vec<ConfiguredTool>) -> Self {
        self.tools = tools;
        self
    }

    /// Sets exact identity-bearing provider-hosted tools.
    #[must_use]
    pub fn hosted_tools(mut self, hosted_tools: Vec<HostedTool>) -> Self {
        self.hosted_tools = hosted_tools;
        self
    }

    /// Sets output intent.
    #[must_use]
    pub fn output(mut self, output: OutputPolicy) -> Self {
        self.output = output;
        self
    }

    /// Sets reasoning intent.
    #[must_use]
    pub fn reasoning(mut self, reasoning: ReasoningPolicy) -> Self {
        self.reasoning = reasoning;
        self
    }

    /// Sets tool-selection intent.
    #[must_use]
    pub fn tool_choice(mut self, tool_choice: ToolChoice) -> Self {
        self.tool_choice = tool_choice;
        self
    }

    /// Sets cache intent.
    #[must_use]
    pub fn cache(mut self, cache: CachePolicy) -> Self {
        self.cache = cache;
        self
    }

    /// Sets portable generation limits.
    #[must_use]
    pub fn limits(mut self, limits: GenerationLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Requires an exact context-evidence class before provider-visible work.
    #[must_use]
    pub const fn context_admission_requirement(
        mut self,
        requirement: ContextAdmissionRequirement,
    ) -> Self {
        self.context_admission_requirement = requirement;
        self
    }

    /// Sets advanced namespaced options.
    #[must_use]
    pub fn native(mut self, native: NativeOptions) -> Self {
        self.native = native;
        self
    }

    /// Binds the exact core-resolved harness plan that produced this request.
    #[must_use]
    pub fn resolved_harness_plan_identity(mut self, identity: Digest) -> Self {
        self.resolved_harness_plan = Some(identity);
        self
    }

    /// Validates and freezes the request.
    ///
    /// # Errors
    ///
    /// Returns an error for duplicate tool names or an unsatisfiable tool choice.
    pub fn build(self) -> Result<ModelRequest, RequestError> {
        let prefix = StableRequestPrefix::new(
            self.selected_provider_configuration,
            self.effective_provider_configuration,
            self.processing_tier,
            self.instructions,
            self.tools,
            self.hosted_tools,
            self.output,
            self.reasoning,
            self.tool_choice,
            self.cache,
            self.limits,
            self.native,
            self.resolved_harness_plan,
        )?
        .with_context_admission_requirement(self.context_admission_requirement)?;
        ModelRequest::freeze(prefix, self.input, self.metadata)
    }
}

fn validate_stable_prefix(
    instructions: &[InstructionBlock],
    tools: &[ConfiguredTool],
    hosted_tools: &[HostedTool],
    tool_choice: &ToolChoice,
    native: &NativeOptions,
) -> Result<(), RequestError> {
    if instructions.len() > 256 {
        return Err(RequestError::TooManyInstructions);
    }
    if tools.len() > 128 {
        return Err(RequestError::TooManyTools);
    }
    if native.entries().len() > 64 {
        return Err(RequestError::TooManyNativeOptions);
    }
    for value in native.entries().values() {
        if canonicalize_normalized_prevalidated(value.clone(), usize::MAX)?.len() > 1024 * 1024 {
            return Err(RequestError::TooLong {
                field: "native option",
                maximum: 1024 * 1024,
            });
        }
    }
    let mut names = BTreeSet::new();
    for tool in tools {
        if !names.insert(tool.definition().name().clone()) {
            return Err(RequestError::DuplicateTool(
                tool.definition().name().clone(),
            ));
        }
    }
    let mut hosted_tools = hosted_tools.to_vec();
    hosted_tools.sort_unstable();
    if hosted_tools.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(RequestError::InvalidHostedTools);
    }
    let programmatic_selected = hosted_tools.contains(&HostedTool::ProgrammaticComposition);
    let has_programmatic_caller = tools
        .iter()
        .any(|tool| !matches!(tool.caller(), ToolCallerDisposition::DirectOnly));
    if programmatic_selected != has_programmatic_caller {
        return Err(RequestError::InvalidHostedTools);
    }
    match tool_choice {
        ToolChoice::Required if tools.is_empty() => Err(RequestError::RequiredToolWithoutTools),
        ToolChoice::Specific(name) if !names.contains(name) => {
            Err(RequestError::UnknownTool(name.clone()))
        }
        _ => Ok(()),
    }
}

/// Canonical bytes of an identity document. The document is bounded by its
/// owner (tool count, description and schema caps, instruction parts), not by
/// the tool-schema limits: a wildcard MCP catalog alone exceeds their node
/// count. Content valid under [`crate::SemanticJsonV1`] keeps its bytes.
fn canonical_serialize(value: &impl Serialize) -> Result<Vec<u8>, RequestError> {
    let value = serde_json::to_value(value).map_err(|error| RequestError::CanonicalJson {
        reason: error.to_string(),
    })?;
    Ok(canonicalize_normalized_prevalidated(
        value,
        MAX_IDENTITY_DOCUMENT_BYTES,
    )?)
}

/// Memory guard for one canonical identity document; its owner's own caps
/// bind it far below this.
const MAX_IDENTITY_DOCUMENT_BYTES: usize = 256 * 1024 * 1024;

fn ensure_nonempty(value: &str, field: &'static str) -> Result<(), RequestError> {
    if value.is_empty() {
        Err(RequestError::Empty { field })
    } else {
        Ok(())
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests;
