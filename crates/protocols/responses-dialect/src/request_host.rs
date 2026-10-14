//! Shared canonical-request lowering for Responses-family dialects.
//!
//! The host borrows one immutable [`ModelRequest`] and closed dialect data. It
//! materializes only the credential-free wire body and bounded tool/replay
//! indexes; it never clones or retains the request, stable prefix, or complete
//! segmented history.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;

use plakous_model::{
    ActiveFamilyArtifact, Actor, CachePolicy, CallId, CompactionBlock,
    CompiledProviderConfiguration, CompiledToolProjectionV2, ConfiguredTool, ContentBlock,
    ConversationItem, Digest, DigestDomain, EffectiveProviderConfiguration, FamilyPolicyEnvelope,
    HostedTool, ImageDetailHint, MediaSource, ModelFamilyId, ModelId, ModelImplementationIdentity,
    ModelRequest, OpaqueFallback, OutputBoundDisposition, OutputFormat, ParallelToolDisposition,
    ProcessingTierDisposition, ProjectedToolDefinitionV2, ProtocolId, Provenance,
    ProviderConfigurationError, ProviderStorageDisposition, ReasoningEffort,
    ReasoningEffortDisposition, ReasoningIncludeDisposition, ReasoningPolicy,
    ReasoningSummaryDisposition, ReplayDisposition, ReplayEnvelope, ReplayKindId,
    ResolvedModelDescriptor, ResolvedProviderAuthorityBinding, ResponseStateDisposition,
    RetentionDisposition, SelectedProviderConfiguration, Timestamp, ToolCallState,
    ToolCallerDisposition, ToolCallerLineage, ToolChoice, ToolDefinition, ToolExecutionDisposition,
    ToolExecutionOwner, ToolInputContract, ToolName, ToolResultBlock, ToolResultKind,
    ToolResultPart, ToolResultUpdateBlock, ValidatedRequestPlan,
};
use plakous_responses::{
    FunctionToolChoiceWire, FunctionToolWire, RESPONSES_ARTIFACT_CODEC_REVISION,
    RESPONSES_ARTIFACT_SCHEMA_REVISION, ReasoningEffortWire, ResponsesArtifact,
    ResponsesArtifactBinding, ResponsesArtifactCodec, ResponsesReplayPurpose,
    ResponsesSemanticRevisions, ResponsesStorageMode, function_call_item,
    function_call_output_item, input_text_part, join_instruction_texts, message_item,
    output_text_part,
};
use serde::Serialize;
use serde_json::{Map, Value, json, value::RawValue};
use thiserror::Error;

use crate::ResponsesCanonicalTool;
use crate::project_host::validate_output_semantic_pair;
use crate::response_host::{ResponsesHostDecodeError, parse_output_item};

const MAX_REPLAY_KIND_BYTES: usize = 256;
const MAX_REPLAY_PREFIX_BINDINGS: usize = 4_096;
const MAX_RESPONSE_ID_BYTES: usize = 1_024;
/// Canonical Responses family-policy schema revision.
pub const RESPONSES_FAMILY_POLICY_SCHEMA_REVISION: u32 = 1;
/// Selected-policy compiler revision.
pub const RESPONSES_FAMILY_POLICY_COMPILER_REVISION: u32 = 1;
/// Closed Responses request-policy execution revision.
pub const RESPONSES_REQUEST_POLICY_EXECUTION_REVISION: u32 = 10;

/// Requires the exact Responses family-policy revisions implemented by this release.
///
/// This is the Responses-owned construction/admission seam: the neutral
/// [`FamilyPolicyEnvelope`] remains provider-agnostic, while this family host
/// certifies the tuple it can execute. The canonical payload repeats the
/// schema/compiler revisions and carries the separate execution and artifact
/// schema/codec revisions, so missing, defaulted, contradictory, or unknown
/// values fail closed.
///
/// # Errors
///
/// Returns an unsupported-revision error unless both the envelope and its
/// canonical payload name the exact currently supported tuple.
pub fn validate_supported_family_policy(
    expected_family: &ModelFamilyId,
    envelope: &FamilyPolicyEnvelope,
) -> Result<(), ResponsesProviderConfigurationError> {
    if envelope.family() != expected_family {
        return Err(ProviderConfigurationError::ForeignTarget.into());
    }
    if envelope.schema_revision() != RESPONSES_FAMILY_POLICY_SCHEMA_REVISION
        || envelope.compiler_revision() != RESPONSES_FAMILY_POLICY_COMPILER_REVISION
        || envelope.dialect_revision() != RESPONSES_REQUEST_POLICY_EXECUTION_REVISION
    {
        return Err(ProviderConfigurationError::UnsupportedRevision.into());
    }
    let canonical: Value = serde_json::from_slice(envelope.canonical_bytes())
        .map_err(|_| ProviderConfigurationError::InvalidFamilyPolicy)?;
    let revision = |field| canonical.get(field).and_then(Value::as_u64);
    if revision("schema_revision") != Some(u64::from(RESPONSES_FAMILY_POLICY_SCHEMA_REVISION))
        || revision("compiler_revision")
            != Some(u64::from(RESPONSES_FAMILY_POLICY_COMPILER_REVISION))
        || revision("execution_revision")
            != Some(u64::from(RESPONSES_REQUEST_POLICY_EXECUTION_REVISION))
        || revision("artifact_schema_revision")
            != Some(u64::from(RESPONSES_ARTIFACT_SCHEMA_REVISION))
        || revision("artifact_codec_revision") != Some(u64::from(RESPONSES_ARTIFACT_CODEC_REVISION))
    {
        return Err(ProviderConfigurationError::UnsupportedRevision.into());
    }
    Ok(())
}

/// Whether `max_output_tokens` is emitted when the canonical limit is present.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MaxOutputTokensPolicy {
    /// Emit the canonical limit.
    Emit,
    /// Omit the field even when a portable limit is present.
    Omit,
}

/// Whether and how function definitions emit provider schema strictness.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FunctionStrictPolicy {
    /// Emit `strict: true`.
    IncludeTrue,
    /// Explicitly preserve non-strict function schemas and their optional fields.
    IncludeFalse,
    /// Emit the OpenRouter-documented `strict: null` default.
    IncludeNull,
    /// Omit `strict`.
    Omit,
}

/// How an exact `{ "type": "string" }` tool schema is represented.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CustomToolPolicy {
    /// Use the Responses `custom` tool/call/result grammar.
    StringSchema,
    /// Refuse the custom-tool convention.
    Reject,
}

/// When the top-level `tool_choice` field is emitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolChoiceFieldPolicy {
    /// Always emit the field, including tool-less requests.
    Always,
    /// Omit the field when no tools are advertised.
    WhenToolsPresent,
}

/// Provider-side response storage behavior.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponsesStoragePolicy {
    /// Emit `store: true` and permit admitted response-ID continuation.
    ProviderStored,
    /// Emit `store: false`; canonical replay remains authoritative.
    Stateless,
}

/// Whether the route emits the top-level Responses `store` field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreFieldPolicy {
    /// Emit the field from [`ResponsesStoragePolicy`].
    Emit,
    /// Omit the field while retaining stateless local replay authority.
    Omit,
}

/// How a canonical prompt-cache request is interpreted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromptCachePolicy {
    /// No exact lowering exists; enabled cache intent fails closed.
    Unsupported,
    /// The route provides implicit caching with no request field.
    Implicit,
    /// Emit Responses `prompt_cache_key` from branch cache-affinity identity.
    AffinityKey,
    /// Emit OpenRouter `session_id` from branch cache-affinity identity.
    GatewaySession,
}

/// How structured output is represented.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StructuredOutputPolicy {
    /// Emit the strict Responses JSON-schema format.
    StrictJsonSchema,
    /// Refuse structured output.
    Unsupported,
}

/// Whether the Responses `parallel_tool_calls` wire field is emitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParallelToolCallsFieldPolicy {
    /// Emit the field whenever tools are advertised.
    WhenToolsPresent,
    /// Omit the field even when serial tools are advertised.
    Omit,
}

/// Closed set of named reasoning efforts admitted by one dialect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReasoningEffortPolicy {
    /// Admit every named Responses effort represented by the canonical model.
    AllNamed,
    /// Admit only omitted, low, medium, and high effort.
    LowMediumHigh,
    /// Admit omitted, `none`, low, medium, high, and `xhigh` effort: the xAI
    /// Platform ladder. Which of them one model takes is the catalog's call.
    NoneThroughXHigh,
}

impl ReasoningEffortPolicy {
    /// Whether this dialect puts `effort` on the wire as `reasoning.effort`.
    #[must_use]
    pub const fn admits(self, effort: ReasoningEffort) -> bool {
        match self {
            Self::AllNamed => matches!(
                effort,
                ReasoningEffort::None
                    | ReasoningEffort::Low
                    | ReasoningEffort::Medium
                    | ReasoningEffort::High
                    | ReasoningEffort::XHigh
                    | ReasoningEffort::Max
                    | ReasoningEffort::Ultra
            ),
            Self::LowMediumHigh => matches!(
                effort,
                ReasoningEffort::Low | ReasoningEffort::Medium | ReasoningEffort::High
            ),
            Self::NoneThroughXHigh => matches!(
                effort,
                ReasoningEffort::None
                    | ReasoningEffort::Low
                    | ReasoningEffort::Medium
                    | ReasoningEffort::High
                    | ReasoningEffort::XHigh
            ),
        }
    }
}

/// Exact `reasoning.summary` disposition for one dialect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReasoningSummaryPolicy {
    /// Omit `reasoning.summary`.
    Omit,
    /// Emit the documented OpenAI Platform `auto` value.
    Auto,
    /// Emit the pinned Grok proxy reference's `concise` value.
    Concise,
}

/// Exact top-level encrypted-reasoning include disposition for one dialect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReasoningIncludePolicy {
    /// Do not emit a reasoning include selector.
    Omit,
    /// Emit one exact selector on every request.
    Always(&'static str),
    /// Emit one exact selector only when reasoning is enabled.
    WhenEnabled(&'static str),
}

/// How canonical reasoning and opaque blocks consume exact native replay.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponsesReplayPolicy {
    /// Every native replay block must remain exact and replayable.
    Strict,
    /// Drop a reasoning item whose exact replay lacks encrypted content.
    /// Opaque blocks and all other replay remain strict.
    DropUnreplayableReasoning,
}

/// Exact top-level Responses `service_tier` admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ServiceTierPolicy {
    StandardOnly,
    ExactNamed(&'static str),
}

/// Where harness/profile instruction text is lowered on the wire.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstructionPlacementPolicy {
    /// Top-level `instructions` string (OpenAI/Codex default).
    TopLevelInstructions,
    /// First `input[]` item as `{role:"system", type:"message", content:[...]}` (SuperGrok).
    InputSystemMessageFirst,
    /// Codex Responses Lite puts tools and developer instructions in input.
    CodexResponsesLite,
}

/// Closed request-wire policy supplied by an admitted Responses dialect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResponsesRequestPolicy {
    service_tier: ServiceTierPolicy,
    max_output_tokens: MaxOutputTokensPolicy,
    function_strict: FunctionStrictPolicy,
    custom_tools: CustomToolPolicy,
    tool_choice: ToolChoiceFieldPolicy,
    storage: ResponsesStoragePolicy,
    store_field: StoreFieldPolicy,
    prompt_cache: PromptCachePolicy,
    structured_output: StructuredOutputPolicy,
    image_input: bool,
    parallel_tool_calls: bool,
    parallel_tool_calls_field: ParallelToolCallsFieldPolicy,
    reasoning_effort: ReasoningEffortPolicy,
    reasoning_summary: ReasoningSummaryPolicy,
    reasoning_include: ReasoningIncludePolicy,
    replay: ResponsesReplayPolicy,
    instruction_placement: InstructionPlacementPolicy,
}

impl ResponsesRequestPolicy {
    /// Builds an explicit closed policy from admitted route/model facts.
    #[allow(
        clippy::too_many_arguments,
        reason = "these fields are the closed Responses request-delta table"
    )]
    #[must_use]
    pub(crate) const fn new(
        max_output_tokens: MaxOutputTokensPolicy,
        function_strict: FunctionStrictPolicy,
        custom_tools: CustomToolPolicy,
        tool_choice: ToolChoiceFieldPolicy,
        storage: ResponsesStoragePolicy,
        store_field: StoreFieldPolicy,
        prompt_cache: PromptCachePolicy,
        structured_output: StructuredOutputPolicy,
        image_input: bool,
        parallel_tool_calls: bool,
        reasoning_effort: ReasoningEffortPolicy,
        reasoning_summary: ReasoningSummaryPolicy,
        reasoning_include: ReasoningIncludePolicy,
        replay: ResponsesReplayPolicy,
    ) -> Self {
        Self {
            service_tier: ServiceTierPolicy::StandardOnly,
            max_output_tokens,
            function_strict,
            custom_tools,
            tool_choice,
            storage,
            store_field,
            prompt_cache,
            structured_output,
            image_input,
            parallel_tool_calls,
            parallel_tool_calls_field: ParallelToolCallsFieldPolicy::WhenToolsPresent,
            reasoning_effort,
            reasoning_summary,
            reasoning_include,
            replay,
            instruction_placement: InstructionPlacementPolicy::TopLevelInstructions,
        }
    }

    /// OpenAI Platform request deltas after route/model admission.
    ///
    /// Function schemas retain optional fields; provider strict normalization must
    /// not change their input contract. Canonical argument validation still applies.
    #[must_use]
    pub const fn openai_platform(
        storage: ResponsesStoragePolicy,
        image_input: bool,
        parallel_tool_calls: bool,
    ) -> Self {
        Self::new(
            MaxOutputTokensPolicy::Emit,
            FunctionStrictPolicy::IncludeFalse,
            CustomToolPolicy::StringSchema,
            ToolChoiceFieldPolicy::Always,
            storage,
            StoreFieldPolicy::Emit,
            PromptCachePolicy::Unsupported,
            StructuredOutputPolicy::StrictJsonSchema,
            image_input,
            parallel_tool_calls,
            ReasoningEffortPolicy::AllNamed,
            ReasoningSummaryPolicy::Auto,
            ReasoningIncludePolicy::Omit,
            ResponsesReplayPolicy::Strict,
        )
    }

    /// Stateless OpenAI Platform execution retains encrypted reasoning for exact local replay.
    #[must_use]
    pub const fn openai_platform_stateless() -> Self {
        let mut policy = Self::openai_platform(ResponsesStoragePolicy::Stateless, false, false);
        policy.reasoning_include =
            ReasoningIncludePolicy::WhenEnabled("reasoning.encrypted_content");
        policy
    }

    /// ChatGPT Codex subscription request deltas after route/model admission.
    ///
    /// Codex is stateless, omits `max_output_tokens`, sends function `strict: false`, and
    /// retains OpenAI custom tools and the explicit `tool_choice` field. It
    /// asks for `reasoning.summary: "auto"` exactly as the official client's
    /// default (`ReasoningSummary::Auto`, sent for every catalog model that
    /// admits the parameter): that is the only wire the summaries the TUI
    /// renders as thought blocks arrive on.
    #[must_use]
    pub const fn openai_codex(
        image_input: bool,
        parallel_tool_calls: bool,
        encrypted_reasoning_include: &'static str,
    ) -> Self {
        let policy = Self::new(
            MaxOutputTokensPolicy::Omit,
            FunctionStrictPolicy::IncludeFalse,
            CustomToolPolicy::StringSchema,
            ToolChoiceFieldPolicy::Always,
            ResponsesStoragePolicy::Stateless,
            StoreFieldPolicy::Emit,
            PromptCachePolicy::AffinityKey,
            StructuredOutputPolicy::StrictJsonSchema,
            image_input,
            parallel_tool_calls,
            ReasoningEffortPolicy::AllNamed,
            ReasoningSummaryPolicy::Auto,
            ReasoningIncludePolicy::Always(encrypted_reasoning_include),
            ResponsesReplayPolicy::Strict,
        );
        Self {
            service_tier: ServiceTierPolicy::ExactNamed("priority"),
            ..policy
        }
    }

    /// Codex Responses Lite keeps client tools and instructions in the replayed input prefix.
    #[must_use]
    pub const fn openai_codex_lite(encrypted_reasoning_include: &'static str) -> Self {
        let mut policy = Self::openai_codex(false, false, encrypted_reasoning_include);
        policy.instruction_placement = InstructionPlacementPolicy::CodexResponsesLite;
        policy
    }

    /// Whether this request uses the authenticated Codex Responses Lite wire.
    #[must_use]
    pub const fn is_codex_responses_lite(self) -> bool {
        matches!(
            self.instruction_placement,
            InstructionPlacementPolicy::CodexResponsesLite
        )
    }

    /// Stateless SuperGrok request deltas, including its exact encrypted-reasoning selector.
    #[must_use]
    pub const fn supergrok_stateless() -> Self {
        let policy = Self::new(
            MaxOutputTokensPolicy::Emit,
            FunctionStrictPolicy::Omit,
            CustomToolPolicy::Reject,
            ToolChoiceFieldPolicy::WhenToolsPresent,
            ResponsesStoragePolicy::Stateless,
            StoreFieldPolicy::Emit,
            PromptCachePolicy::Unsupported,
            StructuredOutputPolicy::Unsupported,
            false,
            false,
            ReasoningEffortPolicy::AllNamed,
            ReasoningSummaryPolicy::Concise,
            ReasoningIncludePolicy::Always("reasoning.encrypted_content"),
            ResponsesReplayPolicy::Strict,
        );
        Self {
            parallel_tool_calls_field: ParallelToolCallsFieldPolicy::Omit,
            instruction_placement: InstructionPlacementPolicy::InputSystemMessageFirst,
            ..policy
        }
    }

    /// Stateless xAI Platform Grok 4.5 request deltas for offline qualification.
    ///
    /// This policy is request-lowering authority only. It deliberately does
    /// not select a stream lifecycle profile or authorize physical dispatch.
    #[must_use]
    pub const fn xai_platform_stateless(encrypted_reasoning_include: &'static str) -> Self {
        Self::new(
            MaxOutputTokensPolicy::Emit,
            FunctionStrictPolicy::Omit,
            CustomToolPolicy::Reject,
            ToolChoiceFieldPolicy::WhenToolsPresent,
            ResponsesStoragePolicy::Stateless,
            StoreFieldPolicy::Emit,
            PromptCachePolicy::Unsupported,
            StructuredOutputPolicy::Unsupported,
            false,
            false,
            ReasoningEffortPolicy::NoneThroughXHigh,
            ReasoningSummaryPolicy::Omit,
            ReasoningIncludePolicy::WhenEnabled(encrypted_reasoning_include),
            ResponsesReplayPolicy::Strict,
        )
    }

    /// Stateless OpenRouter Responses beta request deltas after exact route admission.
    ///
    /// OpenRouter requires an explicit output bound, always emits `tool_choice`,
    /// emits function `strict: null` (not omitted), omits parallel-tool fields,
    /// disables provider storage on wire, and requests encrypted reasoning only
    /// when reasoning is enabled. GatewaySession is `session_id` only.
    /// The route-owned `provider` routing object is applied separately by the
    /// dialect after this family lowering step.
    #[must_use]
    pub const fn openrouter_stateless() -> Self {
        let policy = Self::new(
            MaxOutputTokensPolicy::Emit,
            FunctionStrictPolicy::IncludeNull,
            CustomToolPolicy::Reject,
            ToolChoiceFieldPolicy::WhenToolsPresent,
            ResponsesStoragePolicy::Stateless,
            StoreFieldPolicy::Emit,
            PromptCachePolicy::GatewaySession,
            StructuredOutputPolicy::Unsupported,
            false,
            false,
            ReasoningEffortPolicy::LowMediumHigh,
            ReasoningSummaryPolicy::Omit,
            ReasoningIncludePolicy::Omit,
            ResponsesReplayPolicy::DropUnreplayableReasoning,
        );
        Self {
            parallel_tool_calls_field: ParallelToolCallsFieldPolicy::Omit,
            ..policy
        }
    }

    /// Returns provider-side storage behavior.
    #[must_use]
    pub const fn storage(self) -> ResponsesStoragePolicy {
        self.storage
    }

    /// Returns the exact top-level `store` field disposition.
    #[must_use]
    pub const fn store_field(self) -> StoreFieldPolicy {
        self.store_field
    }

    /// Returns the exact admitted reasoning-effort labels.
    #[must_use]
    pub const fn reasoning_effort(self) -> ReasoningEffortPolicy {
        self.reasoning_effort
    }

    /// Returns the exact `reasoning.summary` disposition.
    #[must_use]
    pub const fn reasoning_summary(self) -> ReasoningSummaryPolicy {
        self.reasoning_summary
    }

    /// Returns the exact encrypted-reasoning include disposition.
    #[must_use]
    pub const fn reasoning_include(self) -> ReasoningIncludePolicy {
        self.reasoning_include
    }

    /// Returns the exact native replay disposition.
    #[must_use]
    pub const fn replay(self) -> ResponsesReplayPolicy {
        self.replay
    }

    /// Whether this route can accept native compaction input, subject to exact producing binding.
    #[must_use]
    pub fn authorizes_native_compaction_item_replay(self) -> bool {
        matches!(self.service_tier, ServiceTierPolicy::ExactNamed("priority"))
            && matches!(self.max_output_tokens, MaxOutputTokensPolicy::Omit)
            && matches!(self.function_strict, FunctionStrictPolicy::IncludeFalse)
            && matches!(self.custom_tools, CustomToolPolicy::StringSchema)
            && matches!(self.storage, ResponsesStoragePolicy::Stateless)
            && matches!(self.reasoning_summary, ReasoningSummaryPolicy::Auto)
            && matches!(
                self.reasoning_include,
                ReasoningIncludePolicy::Always("reasoning.encrypted_content")
            )
    }

    pub(crate) const fn requires_closed_wire_occurrence_accounting(self) -> bool {
        matches!(self.max_output_tokens, MaxOutputTokensPolicy::Emit)
            && matches!(self.function_strict, FunctionStrictPolicy::Omit)
            && matches!(self.custom_tools, CustomToolPolicy::Reject)
            && matches!(self.tool_choice, ToolChoiceFieldPolicy::WhenToolsPresent)
            && matches!(self.storage, ResponsesStoragePolicy::Stateless)
            && matches!(self.store_field, StoreFieldPolicy::Emit)
            && matches!(self.prompt_cache, PromptCachePolicy::Unsupported)
            && matches!(self.structured_output, StructuredOutputPolicy::Unsupported)
            && !self.image_input
            && !self.parallel_tool_calls
            && matches!(
                self.parallel_tool_calls_field,
                ParallelToolCallsFieldPolicy::Omit
            )
            && matches!(self.reasoning_effort, ReasoningEffortPolicy::AllNamed)
            && matches!(self.reasoning_summary, ReasoningSummaryPolicy::Concise)
            && matches!(self.reasoning_include, ReasoningIncludePolicy::Always(_))
            && matches!(self.replay, ResponsesReplayPolicy::Strict)
    }

    /// SuperGrok `cli-chat-proxy` rejects output-only fields on the next-turn
    /// input. Grok Build clears reasoning `status` and function_call `id` /
    /// `status` before POST `/v1/responses`.
    pub(crate) const fn omits_output_only_input_fields(self) -> bool {
        self.requires_closed_wire_occurrence_accounting()
    }

    /// Computes the infallible-at-call-sites identity of every execution-bearing field.
    ///
    /// # Errors
    ///
    /// Returns the digest construction error if the pinned digest revision is invalid.
    pub fn execution_identity(self) -> Result<Digest, plakous_model::DigestError> {
        let max_output = match self.max_output_tokens {
            MaxOutputTokensPolicy::Emit => b"emit".as_slice(),
            MaxOutputTokensPolicy::Omit => b"omit".as_slice(),
        };
        let (service_tier_kind, service_tier_name) = match self.service_tier {
            ServiceTierPolicy::StandardOnly => (b"standard-only".as_slice(), ""),
            ServiceTierPolicy::ExactNamed(name) => (b"exact-named".as_slice(), name),
        };
        let strict = match self.function_strict {
            FunctionStrictPolicy::IncludeTrue => b"include-true".as_slice(),
            FunctionStrictPolicy::IncludeFalse => b"include-false".as_slice(),
            FunctionStrictPolicy::IncludeNull => b"include-null".as_slice(),
            FunctionStrictPolicy::Omit => b"omit".as_slice(),
        };
        let custom = match self.custom_tools {
            CustomToolPolicy::StringSchema => b"string-schema".as_slice(),
            CustomToolPolicy::Reject => b"reject".as_slice(),
        };
        let tool_choice = match self.tool_choice {
            ToolChoiceFieldPolicy::Always => b"always".as_slice(),
            ToolChoiceFieldPolicy::WhenToolsPresent => b"when-tools-present".as_slice(),
        };
        let storage = match self.storage {
            ResponsesStoragePolicy::ProviderStored => b"provider-stored".as_slice(),
            ResponsesStoragePolicy::Stateless => b"stateless".as_slice(),
        };
        let store_field = match self.store_field {
            StoreFieldPolicy::Emit => b"emit".as_slice(),
            StoreFieldPolicy::Omit => b"omit".as_slice(),
        };
        let prompt_cache = match self.prompt_cache {
            PromptCachePolicy::Unsupported => b"unsupported".as_slice(),
            PromptCachePolicy::Implicit => b"implicit".as_slice(),
            PromptCachePolicy::AffinityKey => b"affinity-key".as_slice(),
            PromptCachePolicy::GatewaySession => b"gateway-session".as_slice(),
        };
        let structured = match self.structured_output {
            StructuredOutputPolicy::StrictJsonSchema => b"strict-json-schema".as_slice(),
            StructuredOutputPolicy::Unsupported => b"unsupported".as_slice(),
        };
        let parallel_field = match self.parallel_tool_calls_field {
            ParallelToolCallsFieldPolicy::WhenToolsPresent => b"when-tools-present".as_slice(),
            ParallelToolCallsFieldPolicy::Omit => b"omit".as_slice(),
        };
        let effort = match self.reasoning_effort {
            ReasoningEffortPolicy::AllNamed => b"all-named".as_slice(),
            ReasoningEffortPolicy::LowMediumHigh => b"low-medium-high".as_slice(),
            ReasoningEffortPolicy::NoneThroughXHigh => b"none-through-xhigh".as_slice(),
        };
        let summary = match self.reasoning_summary {
            ReasoningSummaryPolicy::Omit => b"omit".as_slice(),
            ReasoningSummaryPolicy::Auto => b"auto".as_slice(),
            ReasoningSummaryPolicy::Concise => b"concise".as_slice(),
        };
        let (include_kind, include_value) = match self.reasoning_include {
            ReasoningIncludePolicy::Omit => (b"omit".as_slice(), ""),
            ReasoningIncludePolicy::Always(value) => (b"always".as_slice(), value),
            ReasoningIncludePolicy::WhenEnabled(value) => (b"when-enabled".as_slice(), value),
        };
        let replay = match self.replay {
            ResponsesReplayPolicy::Strict => b"strict".as_slice(),
            ResponsesReplayPolicy::DropUnreplayableReasoning => {
                b"drop-unreplayable-reasoning".as_slice()
            }
        };
        let flags = [
            u8::from(self.image_input),
            u8::from(self.parallel_tool_calls),
            u8::from(self.is_codex_responses_lite()),
        ];
        Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[
                b"responses-request-policy-execution-v6",
                service_tier_kind,
                service_tier_name.as_bytes(),
                max_output,
                strict,
                custom,
                tool_choice,
                storage,
                store_field,
                prompt_cache,
                structured,
                parallel_field,
                effort,
                summary,
                include_kind,
                include_value.as_bytes(),
                replay,
                &flags,
            ],
        )
    }
}

/// Failure to compile a selected provider configuration through the Responses policy.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum ResponsesProviderConfigurationError {
    /// One selected behavior dimension is not the exact behavior admitted by the policy.
    #[error("selected Responses behavior does not match policy dimension `{0}`")]
    PolicyMismatch(&'static str),
    /// Canonical family-policy serialization or identity construction failed.
    #[error("Responses family policy identity construction failed")]
    Identity,
    /// Neutral selected-to-effective compilation rejected the resolved facts.
    #[error(transparent)]
    ProviderConfiguration(#[from] ProviderConfigurationError),
}

mod policy;
#[cfg(test)]
use policy::validate_tools;
pub use policy::{
    compile_provider_configuration, compile_provider_configuration_with_implementation,
    compile_provider_configuration_with_implementation_and_router_policy,
};

/// One versioned replay-envelope kind admitted by the selected dialect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResponsesReplayKind<'a> {
    name: &'a str,
    version: u32,
}

impl<'a> ResponsesReplayKind<'a> {
    /// Creates a borrowed replay-kind pin. Validation occurs at lowering.
    #[must_use]
    pub const fn new(name: &'a str, version: u32) -> Self {
        Self { name, version }
    }

    /// Returns the exact replay kind.
    #[must_use]
    pub const fn name(self) -> &'a str {
        self.name
    }

    /// Returns the exact replay payload revision.
    #[must_use]
    pub const fn version(self) -> u32 {
        self.version
    }
}

/// Exact current-operation authority used to derive replay bindings.
///
/// Construction proves that the admitted target binding names this request's
/// stable prefix and complete history projection. The identity additionally
/// binds route, account, model/profile, tool plan, semantic revisions, and
/// storage policy. It contains no credential material.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponsesRequestAuthority {
    binding: ResponsesArtifactBinding,
    storage: ResponsesStoragePolicy,
    request: Digest,
    plan: Digest,
    identity: Digest,
}

impl ResponsesRequestAuthority {
    /// Constructs authority including the runtime-owned branch cache affinity.
    ///
    /// # Errors
    ///
    /// Rejects incomplete admitted facts, a foreign cache-affinity digest, or
    /// an authority whose exact identity cannot be constructed.
    pub fn from_admitted_operation(
        descriptor: &ResolvedModelDescriptor,
        plan: &ValidatedRequestPlan,
        request: &ModelRequest,
        account: Digest,
        revisions: ResponsesSemanticRevisions,
        cache_affinity: Option<Digest>,
        storage: ResponsesStoragePolicy,
    ) -> Result<Self, ResponsesReplayError> {
        let binding = ResponsesArtifactBinding::from_admitted_operation(
            descriptor,
            plan,
            request,
            account,
            revisions,
            cache_affinity,
        )
        .map_err(|error| match error {
            plakous_responses::ResponsesArtifactError::MissingHarnessPlanIdentity => {
                ResponsesReplayError::IncompleteAuthority
            }
            plakous_responses::ResponsesArtifactError::AdmissionMismatch => {
                ResponsesReplayError::AuthorityMismatch
            }
            _ => ResponsesReplayError::AuthorityConstruction,
        })?;
        let request_identity = request.identity();
        let plan_identity = plan.digest();
        let identity =
            operation_authority_digest(&binding, storage, request_identity, plan_identity)?;
        Ok(Self {
            binding,
            storage,
            request: request_identity,
            plan: plan_identity,
            identity,
        })
    }

    /// Returns the exact producing binding for an artifact created by this operation.
    #[must_use]
    pub const fn artifact_binding(&self) -> &ResponsesArtifactBinding {
        &self.binding
    }

    /// Returns the runtime-owned branch cache-affinity identity, when present.
    #[must_use]
    pub const fn cache_affinity(&self) -> Option<Digest> {
        self.binding.cache_affinity()
    }

    /// Returns the derived current-operation authority identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
}

/// One recomputable canonical-prefix integrity binding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResponsesPrefixBinding {
    prefix_items: usize,
    prefix: Digest,
    binding: Digest,
}

impl ResponsesPrefixBinding {
    /// Derives one binding from the immutable request and current authority.
    ///
    /// # Errors
    ///
    /// Returns a bounded request error when authority is stale, the prefix is
    /// outside the request projection, or canonical serialization fails.
    pub fn for_request(
        request: &ModelRequest,
        prefix_items: usize,
        authority: &ResponsesRequestAuthority,
    ) -> Result<Self, ResponsesRequestError> {
        validate_authority(request, authority)?;
        let prefix = request
            .input()
            .canonical_item_prefix_identity(prefix_items)
            .map_err(|_| ResponsesReplayError::MissingPrefixBinding)?
            .identity();
        let binding = prefix_binding_digest(authority.identity, prefix)?;
        Ok(Self {
            prefix_items,
            prefix,
            binding,
        })
    }

    /// Returns the canonical item count covered by the binding.
    #[must_use]
    pub const fn prefix_items(self) -> usize {
        self.prefix_items
    }

    /// Returns the exact prefix binding.
    #[must_use]
    pub const fn binding(self) -> Digest {
        self.binding
    }
}

/// Exact coordinates for one raw official output-item replay envelope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResponsesOutputReplayCoordinate {
    envelope: Digest,
    binding: Digest,
    group: Digest,
    group_start: usize,
    output_index: u32,
    output_count: u32,
    canonical_position: usize,
}

/// Owned, bounded bridge from one validated family artifact to a next request.
///
/// Raw provider payloads remain byte-exact in the artifact. This bridge carries
/// the separate authority and coordinate proof needed to consume them.
#[derive(Debug)]
pub struct ResponsesArtifactReplayBridge {
    authority: ResponsesRequestAuthority,
    output_item: ReplayKindId,
    output_item_version: u32,
    opaque_output: Option<(ReplayKindId, u32)>,
    previous_response: Option<(ReplayKindId, u32)>,
    prefix_bindings: Vec<ResponsesPrefixBinding>,
    output_coordinates: Vec<ResponsesOutputReplayCoordinate>,
    continuation: Option<ResponsesProviderContinuation>,
    hosted_programs: crate::ResponsesHostedProgramContext,
    expires_at: Option<Timestamp>,
    now: Timestamp,
}

#[derive(Debug)]
struct ResponsesProviderContinuation {
    response_id: String,
    prefix_items: usize,
}

/// Borrowed replay-validation data for one request lowering.
///
/// Prefix bindings must be strictly increasing by `prefix_items`. Keeping the
/// bindings outside this host lets the reconstructive family artifact own
/// prefix identity without granting it request-lowering authority.
#[derive(Clone, Copy)]
pub struct ResponsesReplayContext<'a> {
    authority: &'a ResponsesRequestAuthority,
    output_item: ResponsesReplayKind<'a>,
    opaque_output: Option<ResponsesReplayKind<'a>>,
    previous_response: Option<ResponsesReplayKind<'a>>,
    prefix_bindings: &'a [ResponsesPrefixBinding],
    output_coordinates: &'a [ResponsesOutputReplayCoordinate],
    continuation: Option<&'a ResponsesProviderContinuation>,
    expires_at: Option<Timestamp>,
    now: Timestamp,
}

impl<'a> ResponsesReplayContext<'a> {
    /// Creates borrowed replay data. Structural validation occurs at lowering.
    #[must_use]
    #[allow(
        clippy::too_many_arguments,
        reason = "the context borrows independent closed authority tables without allocating"
    )]
    const fn new(
        authority: &'a ResponsesRequestAuthority,
        output_item: ResponsesReplayKind<'a>,
        previous_response: Option<ResponsesReplayKind<'a>>,
        prefix_bindings: &'a [ResponsesPrefixBinding],
        output_coordinates: &'a [ResponsesOutputReplayCoordinate],
        continuation: Option<&'a ResponsesProviderContinuation>,
        expires_at: Option<Timestamp>,
        now: Timestamp,
    ) -> Self {
        Self {
            authority,
            output_item,
            opaque_output: None,
            previous_response,
            prefix_bindings,
            output_coordinates,
            continuation,
            expires_at,
            now,
        }
    }

    const fn with_optional_opaque_output(mut self, kind: Option<ResponsesReplayKind<'a>>) -> Self {
        self.opaque_output = kind;
        self
    }

    fn output_for(
        self,
        canonical_position: usize,
        envelope: Digest,
    ) -> Option<ResponsesOutputReplayCoordinate> {
        self.output_coordinates
            .binary_search_by_key(&canonical_position, |coordinate| {
                coordinate.canonical_position
            })
            .ok()
            .map(|index| self.output_coordinates[index])
            .filter(|coordinate| coordinate.envelope == envelope)
    }
}

impl fmt::Debug for ResponsesReplayContext<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponsesReplayContext")
            .field("authority", &self.authority.identity)
            .field("output_item", &self.output_item)
            .field("previous_response", &self.previous_response)
            .field("prefix_bindings", &"<redacted exact bindings>")
            .field("output_coordinates", &self.output_coordinates.len())
            .field("expires_at", &self.expires_at)
            .field("now", &self.now)
            .finish()
    }
}

impl ResponsesArtifactReplayBridge {
    /// Admits the sole active neutral Responses artifact for this exact request.
    ///
    /// The neutral wrapper must name the exact release-owned Responses
    /// artifact schema and codec plus the decoded producer binding. Its bytes
    /// remain inert until family-local target, storage, expiry, and
    /// canonical-prefix validation succeeds.
    ///
    /// # Errors
    ///
    /// Returns a bounded request error for an incompatible neutral wrapper,
    /// invalid artifact bytes, target/storage drift, expiry, canonical-prefix
    /// mismatch, or replay-kind drift. Only this validated active artifact can
    /// authorize continuation.
    #[allow(
        clippy::too_many_arguments,
        reason = "all arguments are independent closed replay authorities"
    )]
    pub fn from_active_artifact(
        active: &ActiveFamilyArtifact,
        request: &ModelRequest,
        authority: ResponsesRequestAuthority,
        output_item: ResponsesReplayKind<'_>,
        opaque_output: Option<ResponsesReplayKind<'_>>,
        previous_response: Option<ResponsesReplayKind<'_>>,
        now: Timestamp,
    ) -> Result<Self, ResponsesRequestError> {
        validate_authority(request, &authority)?;
        if active.family() != "responses"
            || active.schema_revision() != RESPONSES_ARTIFACT_SCHEMA_REVISION
            || active.codec_revision() != RESPONSES_ARTIFACT_CODEC_REVISION
        {
            return Err(ResponsesReplayError::InvalidArtifact.into());
        }
        let artifact = ResponsesArtifactCodec::decode_inert(active.bytes())
            .map_err(|_| ResponsesReplayError::InvalidArtifact)?;
        if active.binding() != artifact.binding().identity() {
            return Err(ResponsesReplayError::AuthorityMismatch.into());
        }
        Self::from_latest_artifact(
            &artifact,
            request,
            authority,
            output_item,
            opaque_output,
            previous_response,
            now,
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "all arguments are independent closed replay authorities"
    )]
    fn from_latest_artifact(
        artifact: &ResponsesArtifact,
        request: &ModelRequest,
        authority: ResponsesRequestAuthority,
        output_item: ResponsesReplayKind<'_>,
        opaque_output: Option<ResponsesReplayKind<'_>>,
        previous_response: Option<ResponsesReplayKind<'_>>,
        now: Timestamp,
    ) -> Result<Self, ResponsesRequestError> {
        if artifact.is_expired(now) {
            return Err(ResponsesReplayError::ArtifactExpired.into());
        }
        artifact
            .validate_active(now)
            .map_err(|_| ResponsesReplayError::InvalidArtifact)?;
        if let Some(axis) = artifact_target_mismatch(
            authority.artifact_binding(),
            artifact.binding(),
            authority.storage,
            artifact.storage(),
        ) {
            return Err(ResponsesReplayError::ArtifactTargetMismatch { axis }.into());
        }
        if authority.artifact_binding().stable_prefix() != artifact.binding().stable_prefix() {
            return Err(ResponsesReplayError::ArtifactStablePrefixMismatch.into());
        }
        validate_replay_kind(output_item)?;
        if let Some(kind) = opaque_output {
            validate_replay_kind(kind)?;
        }
        if let Some(kind) = previous_response {
            validate_replay_kind(kind)?;
        }
        validate_artifact_replay_kind(
            artifact,
            ResponsesReplayPurpose::OutputItem,
            Some(output_item),
        )?;
        validate_artifact_replay_kind(
            artifact,
            ResponsesReplayPurpose::OpaqueOutputItem,
            opaque_output,
        )?;
        let reuse = artifact
            .validate_for_reuse(authority.artifact_binding(), request, now)
            .map_err(|error| match error {
                plakous_responses::ResponsesArtifactError::BindingMismatch => {
                    ResponsesReplayError::ArtifactBindingMismatch
                }
                plakous_responses::ResponsesArtifactError::CanonicalCoordinateMismatch
                | plakous_responses::ResponsesArtifactError::UnauthorizedReplayTail { .. } => {
                    ResponsesReplayError::ArtifactCanonicalMismatch
                }
                _ => ResponsesReplayError::InvalidArtifact,
            })?;

        let producing_prefix = usize::try_from(artifact.production().producing_projection_items())
            .map_err(|_| ResponsesReplayError::InvalidOutputCoordinates)?;
        let latest_output_count = usize::try_from(artifact.production().output_items())
            .map_err(|_| ResponsesReplayError::InvalidOutputCoordinates)?;
        let covered_prefix = producing_prefix
            .checked_add(latest_output_count)
            .ok_or(ResponsesReplayError::InvalidOutputCoordinates)?;
        if usize::try_from(artifact.production().covered_projection_items()) != Ok(covered_prefix)
            || covered_prefix > request.input().items().len()
        {
            return Err(ResponsesReplayError::InvalidOutputCoordinates.into());
        }
        let hosted_programs =
            validated_hosted_programs(request, covered_prefix, output_item, &authority)?;

        let prefix = artifact.production().covered_projection();
        let prefix_bindings = vec![ResponsesPrefixBinding {
            prefix_items: covered_prefix,
            prefix,
            binding: prefix_binding_digest(authority.identity, prefix)?,
        }];
        let output_binding = prefix_bindings[0].binding;
        let replay_items = reuse
            .replay()
            .iter()
            .copied()
            .filter(|item| is_output_replay_purpose(item.purpose()))
            .collect::<Vec<_>>();
        bounded_replay_coordinate_count(0, replay_items.len())?;
        let mut segment_starts = BTreeSet::new();
        let mut segment_position = 0_usize;
        for segment in request
            .input()
            .history()
            .segments()
            .iter()
            .chain(request.input().working().segments())
        {
            segment_starts.insert(segment_position);
            segment_position = segment_position
                .checked_add(segment.items().len())
                .ok_or(ResponsesReplayError::InvalidOutputCoordinates)?;
        }
        for pair in replay_items.windows(2) {
            let previous = pair[0].canonical_position();
            let current = pair[1].canonical_position();
            if current != previous.saturating_add(1) && !segment_starts.contains(&current) {
                return Err(ResponsesReplayError::InvalidOutputCoordinates.into());
            }
        }
        if producing_prefix > 0
            && replay_items
                .iter()
                .any(|item| item.canonical_position() == producing_prefix.saturating_sub(1))
            && !segment_starts.contains(&producing_prefix)
        {
            return Err(ResponsesReplayError::InvalidOutputCoordinates.into());
        }
        let mut output_coordinates = Vec::with_capacity(replay_items.len());
        let mut cursor = 0_usize;
        while cursor < replay_items.len() {
            let group_start = replay_items[cursor].canonical_position();
            let mut group_end = cursor.saturating_add(1);
            while group_end < replay_items.len()
                && replay_items[group_end].canonical_position()
                    == replay_items[group_end - 1]
                        .canonical_position()
                        .saturating_add(1)
                && !segment_starts.contains(&replay_items[group_end].canonical_position())
            {
                group_end = group_end.saturating_add(1);
            }
            let output_count = u32::try_from(group_end.saturating_sub(cursor))
                .map_err(|_| ResponsesReplayError::InvalidOutputCoordinates)?;
            let group = canonical_replay_group(artifact, group_start, output_count)?;
            for (relative, replay_item) in replay_items[cursor..group_end].iter().enumerate() {
                let envelope = replay_item.envelope();
                let replay_kind =
                    ResponsesReplayKind::new(envelope.kind().as_str(), envelope.version());
                let expected_kind = match replay_item.purpose() {
                    ResponsesReplayPurpose::OutputItem => Some(output_item),
                    ResponsesReplayPurpose::OpaqueOutputItem => opaque_output,
                    ResponsesReplayPurpose::EncryptedReasoning => None,
                };
                if expected_kind != Some(replay_kind)
                    || !envelope.is_compatible(
                        authority.binding.dialect(),
                        authority.binding.model_family(),
                        authority.binding.model(),
                    )
                {
                    return Err(ResponsesReplayError::IncompatibleEnvelope.into());
                }
                let output_index = u32::try_from(relative)
                    .map_err(|_| ResponsesReplayError::InvalidOutputCoordinates)?;
                let canonical_position = replay_item.canonical_position();
                let canonical_item = request
                    .input()
                    .items()
                    .get(canonical_position)
                    .ok_or(ResponsesReplayError::MissingOutputCoordinate)?;
                let canonical_envelope = exact_replay_for_kind(Some(canonical_item), replay_kind)?
                    .ok_or(ResponsesReplayError::MissingOutputCoordinate)?;
                if canonical_envelope.digest() != envelope.digest() {
                    return Err(ResponsesReplayError::MissingOutputCoordinate.into());
                }
                // Compatible historical envelopes in the bound prefix may still
                // be replayed by a latest stateless artifact, but they were
                // produced under the tool-plan that sealed them. After a
                // composition rebind the current catalog must not semantic-pair
                // those names (Direct `read_file` vs js-ptc `exec_js`/`wait_js`).
                // Pair only this response's output window.
                if canonical_position >= producing_prefix {
                    let native: Value = serde_json::from_str(envelope.payload().get())
                        .map_err(|_| ResponsesReplayError::MalformedEnvelope)?;
                    let raw = parse_output_item::<ResponsesHostDecodeError>(&native)
                        .map_err(|_| ResponsesReplayError::MalformedEnvelope)?;
                    validate_output_semantic_pair(&raw, envelope, canonical_item, request)
                        .map_err(|_| ResponsesReplayError::SemanticPairMismatch)?;
                }
                output_coordinates.push(ResponsesOutputReplayCoordinate {
                    envelope: envelope.digest(),
                    binding: output_binding,
                    group,
                    group_start,
                    output_index,
                    output_count,
                    canonical_position,
                });
            }
            cursor = group_end;
        }

        let continuation = match (reuse.continuation(), previous_response) {
            (Some(continuation), Some(_)) => Some(ResponsesProviderContinuation {
                response_id: continuation.response_id().to_owned(),
                prefix_items: covered_prefix,
            }),
            (Some(_), None) => return Err(ResponsesReplayError::IncompatibleEnvelope.into()),
            (None, _) => None,
        };
        Ok(Self {
            authority,
            output_item: ReplayKindId::new(output_item.name)
                .map_err(|_| ResponsesReplayError::InvalidKind)?,
            output_item_version: output_item.version,
            opaque_output: opaque_output
                .map(|kind| {
                    ReplayKindId::new(kind.name)
                        .map(|name| (name, kind.version))
                        .map_err(|_| ResponsesReplayError::InvalidKind)
                })
                .transpose()?,
            previous_response: previous_response
                .map(|kind| {
                    ReplayKindId::new(kind.name)
                        .map(|name| (name, kind.version))
                        .map_err(|_| ResponsesReplayError::InvalidKind)
                })
                .transpose()?,
            prefix_bindings,
            output_coordinates,
            continuation,
            hosted_programs,
            expires_at: artifact.expires_at(),
            now,
        })
    }

    /// Returns the exact current-operation authority.
    #[must_use]
    pub const fn authority(&self) -> &ResponsesRequestAuthority {
        &self.authority
    }

    /// Returns the exact live hosted-program state certified by this artifact.
    #[must_use]
    pub const fn hosted_programs(&self) -> &crate::ResponsesHostedProgramContext {
        &self.hosted_programs
    }

    /// Borrows this bridge as lowering context without copying replay payloads.
    #[must_use]
    pub fn context(&self) -> ResponsesReplayContext<'_> {
        ResponsesReplayContext::new(
            &self.authority,
            ResponsesReplayKind::new(self.output_item.as_str(), self.output_item_version),
            self.previous_response
                .as_ref()
                .map(|(name, version)| ResponsesReplayKind::new(name.as_str(), *version)),
            &self.prefix_bindings,
            &self.output_coordinates,
            self.continuation.as_ref(),
            self.expires_at,
            self.now,
        )
        .with_optional_opaque_output(
            self.opaque_output
                .as_ref()
                .map(|(name, version)| ResponsesReplayKind::new(name.as_str(), *version)),
        )
    }
}

/// Borrowed target and policy data for one request lowering.
#[derive(Clone, Copy, Debug)]
pub struct ResponsesRequestConfig<'a> {
    protocol: &'a ProtocolId,
    model_family: Option<&'a ModelFamilyId>,
    model: &'a ModelId,
    policy: ResponsesRequestPolicy,
    replay: Option<ResponsesReplayContext<'a>>,
    cache_affinity: Option<&'a Digest>,
    account: Option<&'a Digest>,
}

impl<'a> ResponsesRequestConfig<'a> {
    /// Creates exact target data without retaining route, auth, or secret state.
    #[must_use]
    pub const fn new(
        protocol: &'a ProtocolId,
        model_family: Option<&'a ModelFamilyId>,
        model: &'a ModelId,
        policy: ResponsesRequestPolicy,
        replay: Option<ResponsesReplayContext<'a>>,
    ) -> Self {
        Self {
            protocol,
            model_family,
            model,
            policy,
            replay,
            cache_affinity: None,
            account: None,
        }
    }

    /// Binds the exact admitted account independently of aggregate replay.
    #[must_use]
    pub const fn with_account_binding(mut self, account: &'a Digest) -> Self {
        self.account = Some(account);
        self
    }

    /// Binds the runtime branch cache-affinity identity for dialect lowering.
    #[must_use]
    pub const fn with_cache_affinity(mut self, cache_affinity: Option<&'a Digest>) -> Self {
        self.cache_affinity = cache_affinity;
        self
    }
}

/// Exact unsupported canonical-content reasons.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ResponsesRequestContentError {
    /// The actor is outside the portable Responses role table.
    #[error("unknown actor")]
    UnknownActor,
    /// Assistant phase text requires exact provider output replay.
    #[error("assistant phase without exact output-item replay")]
    AssistantPhaseWithoutReplay,
    /// The admitted route/model does not support image input.
    #[error("image input excluded by admitted route/model facts")]
    ImageInputUnsupported,
    /// Inline image bytes are not admitted by this host revision.
    #[error("inline image bytes")]
    InlineImageBytes,
    /// Assistant refusal requires exact provider output replay.
    #[error("assistant refusal without exact output-item replay")]
    AssistantRefusalWithoutReplay,
    /// Provider-hosted calls require exact replay material.
    #[error("provider-hosted call without replay envelope")]
    ProviderHostedCallWithoutReplay,
    /// Compatible-replay content was the item's only semantic representation.
    #[error("opaque-only canonical item has no compatible replay target")]
    OpaqueOnlyWithoutCompatibleReplay,
    /// Canonical custom-tool arguments must be one exact JSON string.
    #[error("custom-tool input is not encoded as a canonical JSON string")]
    InvalidCustomToolArguments,
    /// Tool-result images are outside this host revision.
    #[error("tool-result image excluded by admitted route/model facts")]
    ToolResultImage,
    /// Only text tool-result parts are currently representable.
    #[error("non-text tool result")]
    NonTextToolResult,
    /// Reasoning without exact output replay cannot be reconstructed.
    #[error("reasoning without an exact output-item replay envelope")]
    ReasoningWithoutReplay,
    /// Document/audio input is not representable.
    #[error("document or audio")]
    DocumentOrAudio,
    /// Standalone citations are output replay data, not input messages.
    #[error("standalone citation replay")]
    StandaloneCitation,
    /// A compaction marker needs family-native replay material.
    #[error("compaction without replay")]
    CompactionWithoutReplay,
    /// A future canonical block has no admitted lowering.
    #[error("unknown canonical block")]
    UnknownCanonicalBlock,
}

/// Exact unsupported request-option reasons.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ResponsesRequestOptionError {
    /// Provider-native option namespaces are not silently forwarded.
    #[error("native options")]
    NativeOptions,
    /// Enabled cache intent has no exact lowering in the selected policy.
    #[error("prompt cache policy")]
    PromptCache,
    /// Affinity-key or gateway-session lowering requires branch cache affinity.
    #[error("prompt cache affinity")]
    PromptCacheAffinity,
    /// A future cache policy has no admitted lowering.
    #[error("unknown prompt cache policy")]
    UnknownPromptCache,
    /// Responses has no shared portable stop-sequence field in this revision.
    #[error("stop sequences")]
    StopSequences,
    /// Custom-tool schema was requested under a policy that refuses it.
    #[error("custom tools")]
    CustomTools,
    /// The selected tool is not advertised.
    #[error("specific tool choice is not advertised")]
    SpecificToolNotAdvertised,
    /// Duplicate matching definitions made selection ambiguous.
    #[error("specific tool choice is ambiguous")]
    SpecificToolAmbiguous,
    /// A future tool-selection policy has no admitted lowering.
    #[error("unknown tool choice")]
    UnknownToolChoice,
    /// A standard process capability has no exact enrolled model projection.
    #[error("process tool projection is unavailable for the exact target")]
    ProcessProjectionUnavailable,
    /// Portable reasoning token budgets have no Responses effort mapping.
    #[error("reasoning token budget")]
    ReasoningTokenBudget,
    /// A future reasoning effort has no admitted mapping.
    #[error("unknown reasoning effort")]
    UnknownReasoningEffort,
    /// A future reasoning policy has no admitted mapping.
    #[error("unknown reasoning policy")]
    UnknownReasoningPolicy,
    /// Structured output is not admitted by the selected policy.
    #[error("structured output")]
    StructuredOutput,
    /// A future output format has no admitted lowering.
    #[error("unknown output format")]
    UnknownOutputFormat,
}

/// Exact protected tool-call/result correlation failures.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ResponsesProtectedToolError {
    /// A canonical tool call was not emitted by the assistant.
    #[error("tool call must be owned by an assistant item")]
    InvalidCallActor,
    /// A canonical tool result was not emitted by the tool actor.
    #[error("tool result must be owned by a tool item")]
    InvalidResultActor,
    /// A canonical call was not in the pre-execution requested state.
    #[error("tool call must be in requested state")]
    InvalidCallState,
    /// Call/result execution owners disagreed or were not consumer-owned.
    #[error("tool call/result execution owner mismatch")]
    OwnerMismatch,
    /// Call/result canonical identities disagreed despite a remote-ID collision.
    #[error("tool call/result canonical identity mismatch")]
    CallIdentityMismatch,
    /// Two canonical or replayed calls reused one remote call identity.
    #[error("tool call identity was reused")]
    DuplicateCall,
    /// A result had no authoritative prior call.
    #[error("tool result has no authoritative prior call")]
    ResultWithoutCall,
    /// A result preceded its call.
    #[error("tool result precedes its authoritative call")]
    ResultBeforeCall,
    /// One call received more than one result.
    #[error("tool call has more than one result")]
    DuplicateResult,
    /// One protected call had no result in the request projection.
    #[error("protected tool call has no result")]
    MissingResult,
}

/// Exact replay/continuation validation failures.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ResponsesReplayError {
    /// A configured replay-kind name was empty or oversized.
    #[error("replay kind is empty or exceeds 256 bytes")]
    InvalidKind,
    /// Replay revision zero is reserved.
    #[error("replay kind version must be nonzero")]
    InvalidVersion,
    /// The borrowed prefix-binding or output-coordinate table exceeded its fixed bound.
    #[error("replay prefix bindings or output coordinates exceed the limit of 4096")]
    TooManyPrefixBindings,
    /// Prefix bindings were duplicated or unordered.
    #[error("replay prefix bindings must be strictly ordered")]
    PrefixBindingsNotStrictlyOrdered,
    /// Exact admitted operation facts were absent or did not name this request.
    #[error("replay authority is incomplete for the current operation")]
    IncompleteAuthority,
    /// The replay authority did not match the selected target or artifact.
    #[error("replay authority does not match the current operation")]
    AuthorityMismatch,
    /// One exact reusable provider-target axis differed from the current request.
    #[error("Responses artifact {axis} binding differs from the current request")]
    ArtifactTargetMismatch {
        /// Closed, non-secret target axis that differed.
        axis: &'static str,
    },
    /// The artifact was produced under a different stable request prefix.
    ///
    /// Every hard target axis matched; only the stable-prefix binding
    /// differs. The current request is not that artifact's continuation, so it
    /// is prepared fresh. The binding carries only the prefix digest, so the
    /// differing field is named by the runtime that holds both prefixes.
    #[error("Responses artifact stable-prefix binding differs from the current request")]
    ArtifactStablePrefixMismatch,
    /// A versioned authority digest could not be constructed.
    #[error("replay authority digest construction failed")]
    AuthorityConstruction,
    /// A validated artifact failed integrity or structural revalidation.
    #[error("Responses artifact is invalid for replay")]
    InvalidArtifact,
    /// The artifact's reusable target axes differed from the current request.
    #[error("Responses artifact reuse binding differs from the current request")]
    ArtifactBindingMismatch,
    /// The artifact's covered canonical prefix differed from the current request.
    #[error("Responses artifact canonical coverage differs from the current request")]
    ArtifactCanonicalMismatch,
    /// The artifact aggregate expired before replay lowering consumed it.
    #[error("Responses artifact expired before replay lowering")]
    ArtifactExpired,
    /// A raw official replay envelope lacked its exact artifact coordinate.
    #[error("raw output replay lacks an exact artifact coordinate")]
    MissingOutputCoordinate,
    /// A replay payload referenced an unbound canonical prefix.
    #[error("replay payload references an unbound canonical prefix")]
    MissingPrefixBinding,
    /// More than one exact output replay appeared in one canonical item.
    #[error("canonical item contains multiple exact output replays")]
    MultipleOutputReplay,
    /// Raw provider output and canonical effect semantics disagreed.
    #[error("raw output replay disagrees with canonical effect semantics")]
    SemanticPairMismatch,
    /// The envelope kind/scope/model did not match this target.
    #[error("replay envelope is incompatible with the selected target")]
    IncompatibleEnvelope,
    /// The payload was malformed or contradicted its binding.
    #[error("replay envelope payload is malformed or binding-invalid")]
    MalformedEnvelope,
    /// Strict replay requires completed reasoning with non-empty encrypted content.
    #[error("strict reasoning replay is incomplete or lacks encrypted content")]
    StrictEncryptedReasoning,
    /// A provider response identity was empty or oversized.
    #[error("replay response identity is empty or exceeds 1024 bytes")]
    InvalidResponseId,
    /// Output item coordinates were invalid.
    #[error("replayed output item coordinates are invalid")]
    InvalidOutputCoordinates,
    /// Output items from one response were not contiguous and complete.
    #[error("replayed response output group is non-contiguous or incomplete")]
    IncompleteOutputGroup,
    /// A completed response group reappeared later in history.
    #[error("replayed response output group was repeated")]
    RepeatedOutputGroup,
}

/// Request lowering failure before any physical attempt.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ResponsesRequestError {
    /// Canonical content cannot be represented.
    #[error("unsupported canonical Responses request content: {0}")]
    Content(#[from] ResponsesRequestContentError),
    /// A request option has no admitted exact wire representation.
    #[error("unsupported Responses request option: {0}")]
    Option(#[from] ResponsesRequestOptionError),
    /// A protected tool exchange was incomplete or ambiguous.
    #[error("invalid protected Responses tool exchange: {0}")]
    ProtectedTool(#[from] ResponsesProtectedToolError),
    /// Replay or continuation material failed closed validation.
    #[error("invalid Responses replay: {0}")]
    Replay(#[from] ResponsesReplayError),
    /// JSON body construction unexpectedly failed.
    #[error("Responses request JSON serialization failed")]
    Serialization,
}

impl ResponsesRequestError {
    /// Reports whether this error only means the active artifact does not
    /// belong to *this* request, so the request should be prepared fresh.
    ///
    /// An active artifact is the continuation of one exact canonical prefix. A
    /// request that covers a different prefix — a compaction fold over a sealed
    /// cut, a qualification probe, a turn whose oldest results were replaced — is
    /// simply not that continuation, and there is nothing wrong with it. Before
    /// this distinction existed the route refused the whole preparation, so on
    /// the Responses wire every compaction attempted while an artifact was
    /// active failed with "canonical coverage differs from the current
    /// request" or "reuse binding differs from the current request", and the
    /// next model round (and the fold rung) died for the life of the session.
    ///
    /// A different stable prefix is the same kind of miss: the operation-budget
    /// wrap-up round withdraws tool selection, so it and the round after it
    /// legitimately differ from the artifact's producer in `tool_choice`.
    /// Before this was included, the wrap-up round of every long Responses
    /// turn failed the whole turn instead of preparing fresh.
    ///
    /// `ArtifactTargetMismatch` is **not** included: that says the artifact
    /// belongs to another account, route, model, adapter, revision, tool plan
    /// or storage disposition, which is an authority problem and still fails
    /// closed.
    #[must_use]
    pub const fn artifact_is_not_for_this_request(&self) -> bool {
        matches!(
            self,
            Self::Replay(
                ResponsesReplayError::ArtifactCanonicalMismatch
                    | ResponsesReplayError::ArtifactStablePrefixMismatch
                    | ResponsesReplayError::ArtifactBindingMismatch
                    | ResponsesReplayError::ArtifactExpired
                    | ResponsesReplayError::InvalidOutputCoordinates
            )
        )
    }
}

/// Strict provider-router request extension used by routed Responses origins.
///
/// The dialect remains responsible for selecting and evidencing the endpoint;
/// this value only owns the common wire shape and refuses fallback-capable or
/// unbounded routing data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponsesRouterPolicy {
    kind: ResponsesRouterPolicyKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ResponsesRouterPolicyKind {
    StrictPinned {
        endpoint: String,
        data_collection: String,
        zdr: bool,
    },
    GatewayManaged {
        data_collection: Option<String>,
        zdr: Option<bool>,
    },
}

impl ResponsesRouterPolicy {
    /// Builds a one-endpoint, no-fallback, require-all-parameters routing block.
    ///
    /// # Errors
    ///
    /// Rejects empty, oversized, reserved, or non-canonical router values.
    pub fn strict(
        endpoint: impl Into<String>,
        data_collection: impl Into<String>,
        zdr: bool,
    ) -> Result<Self, ResponsesRequestError> {
        let endpoint = endpoint.into();
        let data_collection = data_collection.into();
        let reserved_segment = endpoint.split(['/', ':']).any(|segment| {
            matches!(
                segment,
                "auto" | "free" | "latest" | "router" | "routers" | "preset" | "presets"
            )
        });
        if endpoint.is_empty()
            || endpoint.len() > 256
            || data_collection.is_empty()
            || data_collection.len() > 32
            || !endpoint.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/' | b':')
            })
            || !data_collection
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
            || reserved_segment
        {
            return Err(ResponsesRequestOptionError::NativeOptions.into());
        }
        Ok(Self {
            kind: ResponsesRouterPolicyKind::StrictPinned {
                endpoint,
                data_collection,
                zdr,
            },
        })
    }

    /// Builds endpoint-neutral gateway-managed routing with optional retention facts.
    ///
    /// # Errors
    ///
    /// Rejects an explicit data-collection value that is empty, oversized, or non-canonical.
    pub fn gateway_managed(
        data_collection: Option<String>,
        zdr: Option<bool>,
    ) -> Result<Self, ResponsesRequestError> {
        if data_collection.as_deref().is_some_and(|value| {
            value.is_empty()
                || value.len() > 32
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
        }) {
            return Err(ResponsesRequestOptionError::NativeOptions.into());
        }
        Ok(Self {
            kind: ResponsesRouterPolicyKind::GatewayManaged {
                data_collection,
                zdr,
            },
        })
    }

    pub(crate) fn wire_value(&self) -> Value {
        match &self.kind {
            ResponsesRouterPolicyKind::StrictPinned {
                endpoint,
                data_collection,
                zdr,
            } => json!({
                "only": [endpoint.as_str()],
                "order": [endpoint.as_str()],
                "allow_fallbacks": false,
                "require_parameters": true,
                "data_collection": data_collection,
                "zdr": zdr,
            }),
            ResponsesRouterPolicyKind::GatewayManaged {
                data_collection,
                zdr,
            } => {
                let mut object = Map::new();
                object.insert("allow_fallbacks".to_owned(), Value::Bool(false));
                object.insert("require_parameters".to_owned(), Value::Bool(true));
                if let Some(data_collection) = data_collection {
                    object.insert(
                        "data_collection".to_owned(),
                        Value::String(data_collection.clone()),
                    );
                }
                if let Some(zdr) = zdr {
                    object.insert("zdr".to_owned(), Value::Bool(*zdr));
                }
                Value::Object(object)
            }
        }
    }

    pub(crate) fn execution_identity(&self) -> Result<Digest, plakous_model::DigestError> {
        match &self.kind {
            ResponsesRouterPolicyKind::StrictPinned {
                endpoint,
                data_collection,
                zdr,
            } => Digest::sha256(
                DigestDomain::ModelBinding,
                1,
                &[
                    b"responses-router-policy-v1",
                    endpoint.as_bytes(),
                    data_collection.as_bytes(),
                    if *zdr { b"zdr" } else { b"no-zdr" },
                ],
            ),
            ResponsesRouterPolicyKind::GatewayManaged {
                data_collection,
                zdr,
            } => Digest::sha256(
                DigestDomain::ModelBinding,
                1,
                &[
                    b"responses-router-policy-gateway-managed-v1",
                    if data_collection.is_some() {
                        b"data-collection-selected"
                    } else {
                        b"data-collection-absent"
                    },
                    data_collection.as_deref().unwrap_or_default().as_bytes(),
                    match zdr {
                        None => b"zdr-absent",
                        Some(false) => b"zdr-false",
                        Some(true) => b"zdr-true",
                    },
                ],
            ),
        }
    }
}

/// Whether the wire request uses full replay or a stored-response prefix.
#[derive(Clone, Eq, PartialEq)]
pub enum ResponsesReplayMode {
    /// Every canonical input item was lowered.
    Full,
    /// A validated provider response covers an exact canonical prefix.
    PreviousResponse {
        /// Provider response identity, retained only for request inspection.
        response_id: String,
        /// Canonical item count covered by the response.
        prefix_items: usize,
    },
}

impl fmt::Debug for ResponsesReplayMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full => formatter.write_str("Full"),
            Self::PreviousResponse { prefix_items, .. } => formatter
                .debug_struct("PreviousResponse")
                .field("response_id", &"<redacted>")
                .field("prefix_items", prefix_items)
                .finish(),
        }
    }
}

/// Credential-free result of shared Responses request lowering.
pub struct LoweredResponsesRequest {
    body: Value,
    local_tools: BTreeSet<ToolName>,
    custom_tools: BTreeSet<ToolName>,
    replay_mode: ResponsesReplayMode,
}

impl LoweredResponsesRequest {
    /// Returns the complete credential-free JSON body.
    #[must_use]
    pub const fn body(&self) -> &Value {
        &self.body
    }

    /// Serializes the exact body.
    ///
    /// # Errors
    ///
    /// Returns a bounded serialization error if JSON serialization fails.
    pub fn body_bytes(&self) -> Result<Vec<u8>, ResponsesRequestError> {
        serde_json::to_vec(&self.body).map_err(|_| ResponsesRequestError::Serialization)
    }

    /// Applies one revision-bound strict router block after family lowering.
    ///
    /// This is intentionally the only arbitrary top-level dialect extension;
    /// it cannot replace a family-owned field.
    ///
    /// # Errors
    ///
    /// Rejects a non-object body or any existing provider-owned field.
    pub fn with_router_policy(
        mut self,
        policy: &ResponsesRouterPolicy,
    ) -> Result<Self, ResponsesRequestError> {
        let object = self
            .body
            .as_object_mut()
            .ok_or(ResponsesRequestError::Serialization)?;
        if object
            .insert("provider".to_owned(), policy.wire_value())
            .is_some()
        {
            return Err(ResponsesRequestOptionError::NativeOptions.into());
        }
        Ok(self)
    }

    /// Returns exact locally executed tools advertised by this request.
    #[must_use]
    pub const fn local_tools(&self) -> &BTreeSet<ToolName> {
        &self.local_tools
    }

    /// Returns the subset using the free-form custom-tool grammar.
    #[must_use]
    pub const fn custom_tools(&self) -> &BTreeSet<ToolName> {
        &self.custom_tools
    }

    /// Returns the selected continuation/replay strategy.
    #[must_use]
    pub const fn replay_mode(&self) -> &ResponsesReplayMode {
        &self.replay_mode
    }
}

impl fmt::Debug for LoweredResponsesRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LoweredResponsesRequest")
            .field("body", &"<redacted>")
            .field("local_tools", &"<redacted>")
            .field("custom_tools", &"<redacted>")
            .field("replay_mode", &self.replay_mode)
            .finish()
    }
}

/// Reports whether a definition explicitly declares free-form UTF-8 input.
#[must_use]
pub fn is_string_custom_tool_definition(tool: &ToolDefinition) -> bool {
    matches!(tool.input(), ToolInputContract::Utf8(_))
}

/// Reports whether a canonical definition remains a provider-visible custom
/// tool after applying an exact V2 projection.
#[must_use]
pub fn is_unprojected_string_custom_tool_definition(
    request: &ModelRequest,
    tool: &ToolDefinition,
) -> bool {
    is_string_custom_tool_definition(tool)
        && !request.tool_projection_v2().is_some_and(|projection| {
            projection
                .definitions()
                .iter()
                .any(|entry| entry.covers_canonical_name(tool.name()))
        })
}

/// Builds the exact provider-visible local-tool table used by canonical
/// Responses decoding after an optional V2 projection.
#[must_use]
pub fn canonical_local_tools(request: &ModelRequest) -> Vec<ResponsesCanonicalTool> {
    if let Some(projection) = request.tool_projection_v2() {
        return projection
            .definitions()
            .iter()
            .map(|entry| {
                if matches!(
                    entry.presentation(),
                    plakous_model::ProjectedToolPresentationV2::CustomUtf8 { .. }
                ) {
                    ResponsesCanonicalTool::local_custom(entry.exposed_name().clone())
                } else {
                    ResponsesCanonicalTool::local_function(entry.exposed_name().clone())
                }
            })
            .collect();
    }
    let mut tools = Vec::new();
    for configured in request.tools() {
        let canonical = configured.definition();
        tools.push(if is_string_custom_tool_definition(canonical) {
            ResponsesCanonicalTool::local_custom(canonical.name().clone())
        } else {
            ResponsesCanonicalTool::local_function(canonical.name().clone())
        });
    }
    tools
}

/// Reports whether this request is the exact Codex OAuth remote-compact
/// native-option facet.
#[must_use]
pub fn is_native_compaction_request(request: &ModelRequest) -> bool {
    request.native().entries().len() == 1
        && request.native().entries().iter().any(|(namespace, value)| {
            namespace.as_str() == "plakous.compaction"
                && value.as_str() == Some("native_responses_trigger_v1")
        })
}

/// Lowers one immutable canonical request through a closed Responses dialect policy.
///
/// The returned value owns only wire material and bounded tool indexes. The
/// source request/history is borrowed for the duration of this call and is not
/// cloned into the result.
///
/// # Errors
///
/// Fails closed on unsupported options/content, incomplete protected tool
/// groups, incompatible replay, or malformed continuation data.
#[allow(
    clippy::too_many_lines,
    reason = "the ordered top-level Responses request shape remains auditable in one function"
)]
pub fn lower_responses_request(
    request: &ModelRequest,
    config: ResponsesRequestConfig<'_>,
) -> Result<LoweredResponsesRequest, ResponsesRequestError> {
    if let Some(projection) = request.tool_projection_v2() {
        let target = projection.target();
        if target.protocol() != config.protocol.as_str()
            || target.family() != config.model_family.map_or("", ModelFamilyId::as_str)
            || target.requested_model() != config.model.as_str()
            || target.effective_model() != config.model.as_str()
            || target.effective_provider_configuration()
                != request.effective_provider_configuration_id()
        {
            return Err(ResponsesRequestOptionError::ProcessProjectionUnavailable.into());
        }
    }
    validate_replay_context(request, config)?;
    let native_compact = is_native_compaction_request(request);
    if !native_compact && !request.native().entries().is_empty() {
        return Err(ResponsesRequestOptionError::NativeOptions.into());
    }
    match request.cache() {
        CachePolicy::Enabled { .. }
            if config.policy.prompt_cache == PromptCachePolicy::Unsupported =>
        {
            return Err(ResponsesRequestOptionError::PromptCache.into());
        }
        CachePolicy::Disabled | CachePolicy::Enabled { .. } => {}
        _ => return Err(ResponsesRequestOptionError::UnknownPromptCache.into()),
    }
    if !request.limits().stop_sequences().is_empty() {
        return Err(ResponsesRequestOptionError::StopSequences.into());
    }

    let replay_mode = select_replay(request, config)?;
    let prefix = match &replay_mode {
        ResponsesReplayMode::Full => 0,
        ResponsesReplayMode::PreviousResponse { prefix_items, .. } => *prefix_items,
    };
    validate_output_replay_sequence(request, config, prefix)?;
    let input_items = request.input().items();

    let mut custom_tools = request
        .tools()
        .iter()
        .filter(|tool| is_unprojected_string_custom_tool_definition(request, tool.definition()))
        .map(|tool| tool.definition().name().clone())
        .collect::<BTreeSet<_>>();
    if let Some(projection) = request.tool_projection_v2() {
        custom_tools.extend(
            projection
                .definitions()
                .iter()
                .filter(|definition| {
                    matches!(
                        definition.presentation(),
                        plakous_model::ProjectedToolPresentationV2::CustomUtf8 { .. }
                    )
                })
                .map(|definition| definition.exposed_name().clone()),
        );
    }
    if !custom_tools.is_empty() && config.policy.custom_tools == CustomToolPolicy::Reject {
        return Err(ResponsesRequestOptionError::CustomTools.into());
    }
    let tool_calls = derive_tool_calls(request, config, &custom_tools, prefix)?;
    let mut tool_results = ToolResultMatches::new(&tool_calls);
    tool_results.match_prefix(input_items.iter().take(prefix), config)?;

    let mut input = Vec::new();
    for (offset, item) in input_items.iter().skip(prefix).enumerate() {
        lower_item(
            config,
            prefix.saturating_add(offset),
            item,
            &custom_tools,
            &mut tool_results,
            &mut input,
        )?;
    }
    for item in &mut input {
        omit_output_only_input_fields(config.policy, item);
    }
    tool_results.finish()?;

    if native_compact {
        input.push(json!({ "type": "compaction_trigger" }));
    }

    let instruction_text = join_instruction_texts(
        request
            .instructions()
            .iter()
            .map(plakous_model::InstructionBlock::text),
    )
    .filter(|text| !text.is_empty());
    if config.policy.instruction_placement == InstructionPlacementPolicy::InputSystemMessageFirst
        && let Some(text) = instruction_text.as_ref()
    {
        input.insert(0, message_item("system", vec![input_text_part(text)]));
    }

    let mut tools = lower_tool_definitions(
        request.tools(),
        request.tool_projection_v2(),
        config,
        &custom_tools,
    )?;
    for hosted in request.hosted_tools() {
        tools.push(lower_hosted_tool(*hosted));
    }
    let mut body = Map::new();
    body.insert("model".to_owned(), json!(config.model.as_str()));
    body.insert("input".to_owned(), Value::Array(input));
    body.insert("stream".to_owned(), Value::Bool(true));
    match (request.processing_tier(), config.policy.service_tier) {
        (ProcessingTierDisposition::Standard, _) => {}
        (
            ProcessingTierDisposition::ProviderNamed(selected),
            ServiceTierPolicy::ExactNamed(name),
        ) if selected.as_str() == name => {
            body.insert("service_tier".to_owned(), Value::String(name.to_owned()));
        }
        (ProcessingTierDisposition::ProviderNamed(_), _) => {
            return Err(ResponsesRequestOptionError::NativeOptions.into());
        }
    }
    if config.policy.store_field == StoreFieldPolicy::Emit {
        body.insert(
            "store".to_owned(),
            Value::Bool(config.policy.storage == ResponsesStoragePolicy::ProviderStored),
        );
    }
    match (config.policy.prompt_cache, config.cache_affinity) {
        (PromptCachePolicy::AffinityKey, Some(affinity)) => {
            body.insert(
                "prompt_cache_key".to_owned(),
                Value::String(affinity.to_string()),
            );
        }
        (PromptCachePolicy::GatewaySession, Some(affinity)) => {
            body.insert("session_id".to_owned(), Value::String(affinity.to_string()));
        }
        _ => {}
    }
    if config.policy.instruction_placement == InstructionPlacementPolicy::TopLevelInstructions
        && let Some(instructions) = instruction_text.as_ref()
    {
        body.insert(
            "instructions".to_owned(),
            Value::String(instructions.clone()),
        );
    }
    if config.policy.is_codex_responses_lite() {
        if matches!(request.reasoning(), ReasoningPolicy::Disabled) {
            return Err(ResponsesRequestOptionError::UnknownReasoningPolicy.into());
        }
        if !request.hosted_tools().is_empty() {
            return Err(ResponsesRequestOptionError::NativeOptions.into());
        }
        let affinity = config
            .cache_affinity
            .ok_or(ResponsesRequestOptionError::PromptCacheAffinity)?;
        let tool_bytes =
            serde_json::to_vec(&tools).map_err(|_| ResponsesRequestError::Serialization)?;
        let affinity_bytes = affinity.identity_bytes();
        let tool_id = plakous_model::sha256_hex(&[
            b"codex-responses-lite-tools-v1",
            &affinity_bytes,
            &tool_bytes,
        ]);
        let mut prefix = vec![json!({
            "type": "additional_tools",
            "id": format!("at_{}", &tool_id[..32]),
            "role": "developer",
            "tools": if tools.is_empty() {
                Vec::new()
            } else {
                vec![json!({
                    "type": "namespace",
                    "name": "functions",
                    "description": "",
                    "tools": tools,
                })]
            },
        })];
        if let Some(instructions) = instruction_text {
            let instruction_id = plakous_model::sha256_hex(&[
                b"codex-responses-lite-instructions-v1",
                &affinity_bytes,
                instructions.as_bytes(),
            ]);
            let mut message = message_item("developer", vec![input_text_part(&instructions)]);
            message["id"] = json!(format!("msg_{}", &instruction_id[..32]));
            prefix.push(message);
        }
        if let Some(Value::Array(input)) = body.get_mut("input") {
            input.splice(0..0, prefix);
        }
        body.insert("parallel_tool_calls".to_owned(), Value::Bool(false));
    } else if !tools.is_empty() {
        body.insert("tools".to_owned(), Value::Array(tools));
        let parallel = config.policy.parallel_tool_calls;
        if config.policy.parallel_tool_calls_field == ParallelToolCallsFieldPolicy::WhenToolsPresent
        {
            body.insert("parallel_tool_calls".to_owned(), Value::Bool(parallel));
        }
    }
    if config.policy.tool_choice == ToolChoiceFieldPolicy::Always
        || !request.tools().is_empty()
        || !request.hosted_tools().is_empty()
    {
        body.insert(
            "tool_choice".to_owned(),
            lower_projected_tool_choice(request, &custom_tools)?,
        );
    }
    if config.policy.max_output_tokens == MaxOutputTokensPolicy::Emit
        && let Some(limit) = request.limits().max_output_tokens()
    {
        body.insert("max_output_tokens".to_owned(), json!(limit.get()));
    }
    lower_reasoning(request.reasoning(), config.policy, &mut body)?;
    lower_output(request.output().format(), config.policy, &mut body)?;
    if config.policy.is_codex_responses_lite() {
        body.entry("text".to_owned()).or_insert_with(|| json!({}))["verbosity"] = json!("low");
    }
    if let ResponsesReplayMode::PreviousResponse { response_id, .. } = &replay_mode {
        body.insert(
            "previous_response_id".to_owned(),
            Value::String(response_id.clone()),
        );
    }
    Ok(LoweredResponsesRequest {
        body: Value::Object(body),
        local_tools: lowered_local_tool_names(request.tools(), request.tool_projection_v2()),
        custom_tools,
        replay_mode,
    })
}

fn lower_hosted_tool(hosted: HostedTool) -> Value {
    match hosted {
        HostedTool::ProgrammaticComposition => json!({"type":"programmatic_tool_calling"}),
        HostedTool::CodeInterpreter => json!({"type":"code_interpreter"}),
    }
}

fn validate_replay_context(
    request: &ModelRequest,
    config: ResponsesRequestConfig<'_>,
) -> Result<(), ResponsesRequestError> {
    let Some(replay) = config.replay else {
        return Ok(());
    };
    if replay.expires_at.is_some_and(|expiry| expiry <= replay.now) {
        return Err(ResponsesReplayError::ArtifactExpired.into());
    }
    validate_authority_for_config(request, config, replay.authority)?;
    for kind in std::iter::once(replay.output_item)
        .chain(replay.opaque_output)
        .chain(replay.previous_response)
    {
        validate_replay_kind(kind)?;
    }
    if replay.prefix_bindings.len() > MAX_REPLAY_PREFIX_BINDINGS {
        return Err(ResponsesReplayError::TooManyPrefixBindings.into());
    }
    if replay
        .prefix_bindings
        .windows(2)
        .any(|pair| pair[0].prefix_items >= pair[1].prefix_items)
    {
        return Err(ResponsesReplayError::PrefixBindingsNotStrictlyOrdered.into());
    }
    for entry in replay.prefix_bindings {
        if entry.prefix_items > request.input().items().len()
            || entry.binding != prefix_binding_digest(replay.authority.identity, entry.prefix)?
        {
            return Err(ResponsesReplayError::AuthorityMismatch.into());
        }
    }
    if replay.output_coordinates.len() > MAX_REPLAY_PREFIX_BINDINGS {
        return Err(ResponsesReplayError::TooManyPrefixBindings.into());
    }
    let mut prior_position = None;
    for coordinate in replay.output_coordinates {
        let expected_position = usize::try_from(coordinate.output_index)
            .ok()
            .and_then(|index| coordinate.group_start.checked_add(index));
        if prior_position.is_some_and(|prior| prior >= coordinate.canonical_position)
            || coordinate.output_count == 0
            || coordinate.output_index >= coordinate.output_count
            || expected_position != Some(coordinate.canonical_position)
            || !replay
                .prefix_bindings
                .iter()
                .any(|entry| entry.binding == coordinate.binding)
        {
            return Err(ResponsesReplayError::InvalidOutputCoordinates.into());
        }
        prior_position = Some(coordinate.canonical_position);
    }
    Ok(())
}

fn bounded_replay_coordinate_count(
    existing: usize,
    additional: usize,
) -> Result<usize, ResponsesRequestError> {
    let aggregate = existing
        .checked_add(additional)
        .ok_or(ResponsesReplayError::InvalidOutputCoordinates)?;
    if aggregate > MAX_REPLAY_PREFIX_BINDINGS {
        return Err(ResponsesReplayError::TooManyPrefixBindings.into());
    }
    Ok(aggregate)
}

const fn is_output_replay_purpose(purpose: ResponsesReplayPurpose) -> bool {
    matches!(
        purpose,
        ResponsesReplayPurpose::OutputItem | ResponsesReplayPurpose::OpaqueOutputItem
    )
}

fn validate_replay_kind(kind: ResponsesReplayKind<'_>) -> Result<(), ResponsesReplayError> {
    if kind.name.is_empty() || kind.name.len() > MAX_REPLAY_KIND_BYTES {
        return Err(ResponsesReplayError::InvalidKind);
    }
    if kind.version == 0 {
        return Err(ResponsesReplayError::InvalidVersion);
    }
    Ok(())
}

fn validate_artifact_replay_kind(
    artifact: &ResponsesArtifact,
    purpose: ResponsesReplayPurpose,
    expected: Option<ResponsesReplayKind<'_>>,
) -> Result<(), ResponsesReplayError> {
    match (artifact.replay_contract().for_purpose(purpose), expected) {
        (Some(actual), Some(expected))
            if actual.kind().as_str() == expected.name && actual.version() == expected.version =>
        {
            Ok(())
        }
        (None, None) => Ok(()),
        _ => Err(ResponsesReplayError::IncompatibleEnvelope),
    }
}

pub(crate) fn validate_authority_for_config(
    request: &ModelRequest,
    config: ResponsesRequestConfig<'_>,
    authority: &ResponsesRequestAuthority,
) -> Result<(), ResponsesReplayError> {
    validate_authority(request, authority)?;
    if authority.binding.dialect() != config.protocol
        || authority.binding.model_family() != config.model_family
        || authority.binding.model() != config.model
        || config
            .account
            .is_some_and(|account| *account != authority.binding.account())
        || authority.storage != config.policy.storage
    {
        return Err(ResponsesReplayError::AuthorityMismatch);
    }
    Ok(())
}

fn validate_authority(
    request: &ModelRequest,
    authority: &ResponsesRequestAuthority,
) -> Result<(), ResponsesReplayError> {
    let item_prefix = request
        .input()
        .canonical_item_prefix_identity(request.input().items().len())
        .map_err(|_| ResponsesReplayError::AuthorityMismatch)?;
    if authority.request != request.identity()
        || authority.binding.stable_prefix() != request.stable_prefix().reuse_identity()
        || authority.binding.projection() != request.input().identity()
        || authority.binding.item_prefix() != item_prefix.identity()
        || request.resolved_harness_plan_identity() != Some(authority.binding.tool_plan())
        || operation_authority_digest(
            &authority.binding,
            authority.storage,
            authority.request,
            authority.plan,
        )? != authority.identity
    {
        return Err(ResponsesReplayError::AuthorityMismatch);
    }
    Ok(())
}

fn operation_authority_digest(
    binding: &ResponsesArtifactBinding,
    storage: ResponsesStoragePolicy,
    request: Digest,
    plan: Digest,
) -> Result<Digest, ResponsesReplayError> {
    let revisions = binding.revisions();
    let family_kernel = revisions.family_kernel().to_be_bytes();
    let dialect_revision = revisions.dialect().to_be_bytes();
    let adapter_revision = revisions.adapter().to_be_bytes();
    let cache_affinity = binding
        .cache_affinity()
        .map(|cache_affinity| cache_affinity.identity_bytes());
    let cache_affinity_present = [u8::from(cache_affinity.is_some())];
    let family = binding.model_family().map_or("", ModelFamilyId::as_str);
    let storage = match storage {
        ResponsesStoragePolicy::ProviderStored => b"provider-stored".as_slice(),
        ResponsesStoragePolicy::Stateless => b"stateless".as_slice(),
    };
    let stable_prefix = binding.stable_prefix().identity_bytes();
    let projection = binding.projection().identity_bytes();
    let item_prefix = binding.item_prefix().identity_bytes();
    let tool_plan = binding.tool_plan().identity_bytes();
    let account = binding.account().identity_bytes();
    let semantic_model = binding.semantic_model().identity_bytes();
    let request = request.identity_bytes();
    let plan = plan.identity_bytes();
    Digest::sha256(
        DigestDomain::ModelBinding,
        6,
        &[
            b"plak-responses-current-operation-authority-v4",
            binding.dialect().as_str().as_bytes(),
            binding.route().as_str().as_bytes(),
            account.as_slice(),
            binding.model().as_str().as_bytes(),
            family.as_bytes(),
            binding.adapter().as_str().as_bytes(),
            semantic_model.as_slice(),
            &family_kernel,
            &dialect_revision,
            &adapter_revision,
            &cache_affinity_present,
            cache_affinity.as_deref().unwrap_or_default(),
            stable_prefix.as_slice(),
            projection.as_slice(),
            item_prefix.as_slice(),
            tool_plan.as_slice(),
            storage,
            request.as_slice(),
            plan.as_slice(),
        ],
    )
    .map_err(|_| ResponsesReplayError::AuthorityConstruction)
}

fn prefix_binding_digest(
    authority: Digest,
    prefix: Digest,
) -> Result<Digest, ResponsesRequestError> {
    let authority = authority.identity_bytes();
    let prefix = prefix.identity_bytes();
    Digest::sha256(
        DigestDomain::ModelBinding,
        7,
        &[
            b"plak-responses-authorized-item-prefix-v3",
            authority.as_slice(),
            prefix.as_slice(),
        ],
    )
    .map_err(|_| ResponsesReplayError::AuthorityConstruction.into())
}

fn storage_mode(policy: ResponsesStoragePolicy) -> ResponsesStorageMode {
    match policy {
        ResponsesStoragePolicy::ProviderStored => ResponsesStorageMode::ProviderStored,
        ResponsesStoragePolicy::Stateless => ResponsesStorageMode::Stateless,
    }
}

fn canonical_replay_group(
    artifact: &ResponsesArtifact,
    group_start: usize,
    output_count: u32,
) -> Result<Digest, ResponsesReplayError> {
    let group_start = u64::try_from(group_start)
        .map_err(|_| ResponsesReplayError::InvalidOutputCoordinates)?
        .to_be_bytes();
    let output_count = output_count.to_be_bytes();
    let covered = artifact.production().covered_projection().identity_bytes();
    Digest::sha256(
        DigestDomain::ModelBinding,
        6,
        &[
            b"plak-responses-canonical-replay-group-v2",
            artifact.integrity(),
            covered.as_slice(),
            &group_start,
            &output_count,
        ],
    )
    .map_err(|_| ResponsesReplayError::AuthorityConstruction)
}

fn artifact_target_mismatch(
    current: &ResponsesArtifactBinding,
    artifact: &ResponsesArtifactBinding,
    current_storage: ResponsesStoragePolicy,
    artifact_storage: ResponsesStorageMode,
) -> Option<&'static str> {
    [
        (current.dialect() != artifact.dialect(), "dialect"),
        (current.route() != artifact.route(), "route"),
        (current.account() != artifact.account(), "account"),
        (current.model() != artifact.model(), "model"),
        (
            current.model_family() != artifact.model_family(),
            "model-family",
        ),
        (current.adapter() != artifact.adapter(), "adapter"),
        (
            current.semantic_model() != artifact.semantic_model(),
            "semantic-model",
        ),
        (current.revisions() != artifact.revisions(), "revisions"),
        (current.tool_plan() != artifact.tool_plan(), "tool-plan"),
        (
            storage_mode(current_storage) != artifact_storage,
            "storage-mode",
        ),
    ]
    .into_iter()
    .find_map(|(mismatch, axis)| mismatch.then_some(axis))
}

fn exact_replay_for_kind<'a>(
    item: Option<&'a ConversationItem>,
    kind: ResponsesReplayKind<'_>,
) -> Result<Option<&'a ReplayEnvelope>, ResponsesReplayError> {
    let Some(item) = item else {
        return Ok(None);
    };
    let mut exact = None;
    for block in item.blocks() {
        let replay = match block {
            ContentBlock::Opaque(opaque) => Some(opaque.replay()),
            ContentBlock::Reasoning(reasoning) => reasoning.replay(),
            _ => None,
        };
        if let Some(replay) = replay
            && replay.kind().as_str() == kind.name
        {
            if replay.version() != kind.version {
                return Err(ResponsesReplayError::IncompatibleEnvelope);
            }
            if exact.replace(replay).is_some() {
                return Err(ResponsesReplayError::MultipleOutputReplay);
            }
        }
    }
    Ok(exact)
}

fn validated_hosted_programs(
    request: &ModelRequest,
    covered_prefix: usize,
    output_item: ResponsesReplayKind<'_>,
    authority: &ResponsesRequestAuthority,
) -> Result<crate::ResponsesHostedProgramContext, ResponsesRequestError> {
    let mut programs = crate::ResponsesHostedProgramContext::default();
    for item in request.input().items().iter().take(covered_prefix) {
        let Some(replay) = exact_replay_for_kind(Some(item), output_item)? else {
            continue;
        };
        if !replay.is_compatible(
            authority.binding.dialect(),
            authority.binding.model_family(),
            authority.binding.model(),
        ) {
            // A canonical prefix may retain replay from another model before
            // the active artifact's producing projection. Such history cannot
            // authorize a hosted program for this target and is ignored here;
            // every active-artifact replay coordinate remains validated above.
            continue;
        }
        let native = crate::parse_unique_json_value(replay.payload().get())
            .map_err(|_| ResponsesReplayError::MalformedEnvelope)?;
        let output = parse_output_item::<ResponsesHostDecodeError>(&native)
            .map_err(|_| ResponsesReplayError::MalformedEnvelope)?;
        // Each replay was paired under its producing route's canonical policy.
        // This pass only reconstructs hosted-program lineage; the exact Lite
        // functions namespace must remain admissible for that replay.
        programs
            .admit_replayed_output(
                &output,
                crate::ResponsesCanonicalPolicy::openai_platform().with_codex_lite_namespace(),
            )
            .map_err(|_| ResponsesReplayError::SemanticPairMismatch)?;
    }
    Ok(programs)
}

fn select_replay(
    request: &ModelRequest,
    config: ResponsesRequestConfig<'_>,
) -> Result<ResponsesReplayMode, ResponsesRequestError> {
    if config.policy.storage != ResponsesStoragePolicy::ProviderStored {
        return Ok(ResponsesReplayMode::Full);
    }
    let Some(context) = config.replay else {
        return Ok(ResponsesReplayMode::Full);
    };
    let Some(previous_kind) = context.previous_response else {
        return Ok(ResponsesReplayMode::Full);
    };
    let Some(continuation) = context.continuation else {
        return Ok(ResponsesReplayMode::Full);
    };
    if continuation.response_id.is_empty()
        || continuation.response_id.len() > MAX_RESPONSE_ID_BYTES
        || continuation.prefix_items > request.input().items().len()
    {
        return Err(ResponsesReplayError::InvalidResponseId.into());
    }
    let _ = previous_kind;
    Ok(ResponsesReplayMode::PreviousResponse {
        response_id: continuation.response_id.clone(),
        prefix_items: continuation.prefix_items,
    })
}

fn lower_tool_definition(
    configured: &ConfiguredTool,
    policy: ResponsesRequestPolicy,
    custom_tools: &BTreeSet<ToolName>,
) -> Result<Value, ResponsesRequestError> {
    let tool = configured.definition();
    if custom_tools.contains(tool.name()) {
        if policy.custom_tools == CustomToolPolicy::Reject {
            return Err(ResponsesRequestOptionError::CustomTools.into());
        }
        let mut wire = json!({
            "type": "custom",
            "name": tool.name().as_str(),
            "description": tool.description(),
        });
        lower_allowed_callers(&mut wire, configured.caller())?;
        return Ok(wire);
    }
    let mut wire = match policy.function_strict {
        FunctionStrictPolicy::IncludeTrue => FunctionToolWire::openai_strict(
            tool.name().as_str(),
            tool.description(),
            match tool.input() {
                ToolInputContract::Structured(schema) => schema.wire_document(),
                ToolInputContract::Utf8(_) => {
                    return Err(ResponsesRequestOptionError::CustomTools.into());
                }
                _ => return Err(ResponsesRequestOptionError::CustomTools.into()),
            },
        ),
        FunctionStrictPolicy::IncludeFalse
        | FunctionStrictPolicy::IncludeNull
        | FunctionStrictPolicy::Omit => FunctionToolWire::function(
            tool.name().as_str(),
            tool.description(),
            match tool.input() {
                ToolInputContract::Structured(schema) => schema.wire_document(),
                ToolInputContract::Utf8(_) => {
                    return Err(ResponsesRequestOptionError::CustomTools.into());
                }
                _ => return Err(ResponsesRequestOptionError::CustomTools.into()),
            },
        ),
    }
    .to_value();
    if matches!(policy.function_strict, FunctionStrictPolicy::IncludeNull) {
        wire.as_object_mut()
            .ok_or(ResponsesRequestError::Serialization)?
            .insert("strict".to_owned(), Value::Null);
    }
    if matches!(policy.function_strict, FunctionStrictPolicy::IncludeFalse) {
        wire["strict"] = Value::Bool(false);
    }
    lower_allowed_callers(&mut wire, configured.caller())?;
    Ok(wire)
}

fn lower_allowed_callers(
    wire: &mut Value,
    caller: ToolCallerDisposition,
) -> Result<(), ResponsesRequestError> {
    let callers = match caller {
        ToolCallerDisposition::DirectOnly => return Ok(()),
        ToolCallerDisposition::ProviderProgrammaticOnly => json!(["programmatic"]),
        ToolCallerDisposition::DirectAndProviderProgrammatic => {
            json!(["direct", "programmatic"])
        }
    };
    wire.as_object_mut()
        .ok_or(ResponsesRequestError::Serialization)?
        .insert("allowed_callers".to_owned(), callers);
    Ok(())
}

fn lower_projected_tool_definition(
    tool: &ProjectedToolDefinitionV2,
    policy: ResponsesRequestPolicy,
) -> Result<Value, ResponsesRequestError> {
    match tool.presentation() {
        plakous_model::ProjectedToolPresentationV2::StructuredFunction { input_schema } => {
            let mut wire = json!({
                "type": "function",
                "name": tool.exposed_name().as_str(),
                "description": tool.description(),
                "parameters": input_schema.wire_document()
            });
            match policy.function_strict {
                FunctionStrictPolicy::IncludeTrue => wire["strict"] = Value::Bool(true),
                FunctionStrictPolicy::IncludeFalse => wire["strict"] = Value::Bool(false),
                FunctionStrictPolicy::IncludeNull => wire["strict"] = Value::Null,
                FunctionStrictPolicy::Omit => {}
            }
            Ok(wire)
        }
        plakous_model::ProjectedToolPresentationV2::CustomUtf8 { format, .. } => {
            if policy.custom_tools == CustomToolPolicy::Reject {
                return Err(ResponsesRequestOptionError::CustomTools.into());
            }
            Ok(json!({
                "type": "custom",
                "name": tool.exposed_name().as_str(),
                "description": tool.description(),
                "format": {
                    "type": "grammar",
                    "syntax": match format.syntax() {
                        plakous_model::ProjectedCustomToolGrammarSyntaxV2::Lark => "lark",
                    },
                    "definition": format.definition(),
                }
            }))
        }
    }
}

fn lower_tool_definitions(
    tools: &[ConfiguredTool],
    projection: Option<&CompiledToolProjectionV2>,
    config: ResponsesRequestConfig<'_>,
    custom_tools: &BTreeSet<ToolName>,
) -> Result<Vec<Value>, ResponsesRequestError> {
    if let Some(projection) = projection {
        return projection
            .definitions()
            .iter()
            .map(|entry| {
                let callers = projected_caller_disposition(entry, tools)?;
                let mut wire = lower_projected_tool_definition(entry, config.policy)?;
                lower_allowed_callers(&mut wire, callers)?;
                Ok(wire)
            })
            .collect::<Result<Vec<_>, _>>();
    }
    let mut lowered = Vec::new();
    for tool in tools {
        lowered.push(lower_tool_definition(tool, config.policy, custom_tools)?);
    }
    Ok(lowered)
}

fn projected_caller_disposition(
    projected: &ProjectedToolDefinitionV2,
    tools: &[ConfiguredTool],
) -> Result<ToolCallerDisposition, ResponsesRequestError> {
    let mut dispositions = tools
        .iter()
        .filter(|tool| projected.covers_canonical_name(tool.definition().name()))
        .map(ConfiguredTool::caller);
    let first = dispositions
        .next()
        .ok_or(ResponsesRequestOptionError::ProcessProjectionUnavailable)?;
    if dispositions.any(|caller| caller != first) {
        return Err(ResponsesRequestOptionError::ProcessProjectionUnavailable.into());
    }
    Ok(first)
}

fn lowered_local_tool_names(
    tools: &[ConfiguredTool],
    projection: Option<&CompiledToolProjectionV2>,
) -> BTreeSet<ToolName> {
    if let Some(projection) = projection {
        return projection
            .definitions()
            .iter()
            .map(|entry| entry.exposed_name().clone())
            .collect();
    }
    tools
        .iter()
        .map(|tool| tool.definition().name().clone())
        .collect()
}

fn lower_tool_choice(
    choice: &ToolChoice,
    tools: &[ConfiguredTool],
    custom_tools: &BTreeSet<ToolName>,
) -> Result<Value, ResponsesRequestError> {
    match choice {
        ToolChoice::None => Ok(FunctionToolChoiceWire::None.to_value()),
        ToolChoice::Required => Ok(FunctionToolChoiceWire::Required.to_value()),
        ToolChoice::Specific(name) => {
            let mut selected = tools.iter().filter(|tool| tool.definition().name() == name);
            let tool = selected
                .next()
                .ok_or(ResponsesRequestOptionError::SpecificToolNotAdvertised)?;
            if selected.next().is_some() {
                return Err(ResponsesRequestOptionError::SpecificToolAmbiguous.into());
            }
            if custom_tools.contains(tool.definition().name()) {
                Ok(json!({"type": "custom", "name": name.as_str()}))
            } else {
                Ok(FunctionToolChoiceWire::Function(name.as_str().to_owned()).to_value())
            }
        }
        ToolChoice::Auto => Ok(FunctionToolChoiceWire::Auto.to_value()),
        _ => Err(ResponsesRequestOptionError::UnknownToolChoice.into()),
    }
}

fn lower_projected_tool_choice(
    request: &ModelRequest,
    custom_tools: &BTreeSet<ToolName>,
) -> Result<Value, ResponsesRequestError> {
    if let ToolChoice::Specific(name) = request.tool_choice()
        && let Some(projection) = request.tool_projection_v2()
    {
        let mut matches = projection
            .definitions()
            .iter()
            .filter(|entry| entry.covers_canonical_name(name));
        if let Some(entry) = matches.next() {
            if matches.next().is_some() {
                return Err(ResponsesRequestOptionError::SpecificToolAmbiguous.into());
            }
            let kind = if matches!(
                entry.presentation(),
                plakous_model::ProjectedToolPresentationV2::CustomUtf8 { .. }
            ) {
                "custom"
            } else {
                "function"
            };
            return Ok(json!({"type":kind,"name":entry.exposed_name().as_str()}));
        }
    }
    lower_tool_choice(request.tool_choice(), request.tools(), custom_tools)
}

fn lower_reasoning(
    reasoning: ReasoningPolicy,
    policy: ResponsesRequestPolicy,
    body: &mut Map<String, Value>,
) -> Result<(), ResponsesRequestError> {
    let effort = match reasoning {
        ReasoningPolicy::Disabled => {
            lower_reasoning_include(policy.reasoning_include, false, body)?;
            return Ok(());
        }
        ReasoningPolicy::Enabled { effort, .. } => effort,
        _ => return Err(ResponsesRequestOptionError::UnknownReasoningPolicy.into()),
    };
    let effort = match effort {
        None => None,
        Some(ReasoningEffort::TokenBudget(_)) => {
            return Err(ResponsesRequestOptionError::ReasoningTokenBudget.into());
        }
        Some(effort) if !policy.reasoning_effort.admits(effort) => {
            return Err(ResponsesRequestOptionError::UnknownReasoningEffort.into());
        }
        Some(ReasoningEffort::Low) => Some(ReasoningEffortWire::Low),
        Some(ReasoningEffort::Medium) => Some(ReasoningEffortWire::Medium),
        Some(ReasoningEffort::High) => Some(ReasoningEffortWire::High),
        Some(ReasoningEffort::XHigh) => Some(ReasoningEffortWire::XHigh),
        Some(ReasoningEffort::Max) => Some(ReasoningEffortWire::Max),
        Some(ReasoningEffort::Ultra) => Some(ReasoningEffortWire::Ultra),
        Some(ReasoningEffort::None) => Some(ReasoningEffortWire::None),
        Some(_) => return Err(ResponsesRequestOptionError::UnknownReasoningEffort.into()),
    };
    let mut reasoning = Map::new();
    if let Some(effort) = effort {
        reasoning.insert("effort".to_owned(), json!(effort.as_str()));
    }
    match policy.reasoning_summary {
        ReasoningSummaryPolicy::Omit => {}
        ReasoningSummaryPolicy::Auto => {
            reasoning.insert("summary".to_owned(), json!("auto"));
        }
        ReasoningSummaryPolicy::Concise => {
            reasoning.insert("summary".to_owned(), json!("concise"));
        }
    }
    if policy.is_codex_responses_lite() {
        reasoning.insert("context".to_owned(), json!("all_turns"));
    }
    body.insert("reasoning".to_owned(), Value::Object(reasoning));
    lower_reasoning_include(policy.reasoning_include, true, body)?;
    Ok(())
}

fn lower_reasoning_include(
    policy: ReasoningIncludePolicy,
    reasoning_enabled: bool,
    body: &mut Map<String, Value>,
) -> Result<(), ResponsesRequestError> {
    let selector = match policy {
        ReasoningIncludePolicy::Always(selector) => selector,
        ReasoningIncludePolicy::WhenEnabled(selector) if reasoning_enabled => selector,
        ReasoningIncludePolicy::Omit | ReasoningIncludePolicy::WhenEnabled(_) => return Ok(()),
    };
    if selector.is_empty() || selector.len() > MAX_REPLAY_KIND_BYTES {
        return Err(ResponsesRequestOptionError::UnknownReasoningPolicy.into());
    }
    body.insert("include".to_owned(), json!([selector]));
    Ok(())
}

fn lower_output(
    format: &OutputFormat,
    policy: ResponsesRequestPolicy,
    body: &mut Map<String, Value>,
) -> Result<(), ResponsesRequestError> {
    match format {
        OutputFormat::Text => Ok(()),
        OutputFormat::Structured(schema) => {
            if policy.structured_output == StructuredOutputPolicy::Unsupported {
                return Err(ResponsesRequestOptionError::StructuredOutput.into());
            }
            body.insert(
                "text".to_owned(),
                json!({
                    "format": {
                        "type": "json_schema",
                        "name": "plak_response",
                        "schema": schema.document().value(),
                        "strict": true,
                    }
                }),
            );
            Ok(())
        }
        _ => Err(ResponsesRequestOptionError::UnknownOutputFormat.into()),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ToolCallKind {
    Function,
    Custom,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct CanonicalPosition {
    item_index: usize,
    block_index: usize,
}

impl CanonicalPosition {
    const fn new(item_index: usize, block_index: usize) -> Self {
        Self {
            item_index,
            block_index,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ToolCallSource {
    position: CanonicalPosition,
    kind: ToolCallKind,
    call_id: CallId,
    owner: ToolExecutionOwner,
    name: ToolName,
    caller_lineage: ToolCallerLineage,
}

struct ToolResultMatches<'a> {
    tool_calls: &'a BTreeMap<String, ToolCallSource>,
    matched: BTreeSet<String>,
    /// Yield texts of a UTF-8 cell that this target replays as a plain
    /// `function_call` (the tool is not one of its custom tools). A function
    /// call owns exactly one `function_call_output`, so its updates are folded
    /// into that output in source order instead of being sent as
    /// `custom_tool_call_output` items the wire would reject.
    folded_updates: BTreeMap<String, Vec<String>>,
}

impl<'a> ToolResultMatches<'a> {
    fn new(tool_calls: &'a BTreeMap<String, ToolCallSource>) -> Self {
        Self {
            tool_calls,
            matched: BTreeSet::new(),
            folded_updates: BTreeMap::new(),
        }
    }

    /// Takes the yields folded for `remote_id`, if the call was replayed as a
    /// function call.
    fn take_folded_updates(&mut self, remote_id: &str) -> Option<Vec<String>> {
        self.folded_updates.remove(remote_id)
    }

    fn match_prefix<'b>(
        &mut self,
        items: impl IntoIterator<Item = &'b ConversationItem>,
        config: ResponsesRequestConfig<'_>,
    ) -> Result<(), ResponsesRequestError> {
        for (item_index, item) in items.into_iter().enumerate() {
            if exact_output_replay(item_index, item, config)?.is_some() {
                continue;
            }
            for (block_index, block) in item.blocks().iter().enumerate() {
                match block {
                    ContentBlock::ToolResult(result) => {
                        let remote_id = result
                            .correlation()
                            .remote_call_id()
                            .map_or_else(|| result.call_id().to_string(), ToOwned::to_owned);
                        self.match_result(
                            &remote_id,
                            CanonicalPosition::new(item_index, block_index),
                            result,
                        )?;
                    }
                    ContentBlock::ToolResultUpdate(update) => {
                        let remote_id = update
                            .correlation()
                            .remote_call_id()
                            .map_or_else(|| update.call_id().to_string(), ToOwned::to_owned);
                        self.match_update(
                            &remote_id,
                            CanonicalPosition::new(item_index, block_index),
                            update,
                        )?;
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn match_result(
        &mut self,
        remote_id: &str,
        result_position: CanonicalPosition,
        result: &ToolResultBlock,
    ) -> Result<ToolCallKind, ResponsesRequestError> {
        let source = self
            .tool_calls
            .get(remote_id)
            .ok_or(ResponsesProtectedToolError::ResultWithoutCall)?;
        if source.position >= result_position {
            return Err(ResponsesProtectedToolError::ResultBeforeCall.into());
        }
        if source.call_id != result.call_id() {
            return Err(ResponsesProtectedToolError::CallIdentityMismatch.into());
        }
        if &source.owner != result.owner()
            || &source.caller_lineage != result.correlation().caller_lineage()
            || !matches!(
                result.owner(),
                ToolExecutionOwner::PlakLocal | ToolExecutionOwner::PlakMcp { .. }
            )
        {
            return Err(ResponsesProtectedToolError::OwnerMismatch.into());
        }
        if !matches!(
            result.kind(),
            ToolResultKind::Success | ToolResultKind::Failure
        ) {
            return Err(ResponsesProtectedToolError::CallIdentityMismatch.into());
        }
        if !self.matched.insert(remote_id.to_owned()) {
            return Err(ResponsesProtectedToolError::DuplicateResult.into());
        }
        Ok(source.kind)
    }

    /// Matches a yield to its call. Returns the custom tool name when the wire
    /// carries the yield as its own `custom_tool_call_output`; returns `None`
    /// after folding it when the call is replayed as a function call.
    fn match_update(
        &mut self,
        remote_id: &str,
        update_position: CanonicalPosition,
        update: &ToolResultUpdateBlock,
    ) -> Result<Option<ToolName>, ResponsesRequestError> {
        let source = self
            .tool_calls
            .get(remote_id)
            .ok_or(ResponsesProtectedToolError::ResultWithoutCall)?;
        if source.position >= update_position {
            return Err(ResponsesProtectedToolError::ResultBeforeCall.into());
        }
        if source.call_id != update.call_id() {
            return Err(ResponsesProtectedToolError::CallIdentityMismatch.into());
        }
        if &source.owner != update.owner() {
            return Err(ResponsesProtectedToolError::OwnerMismatch.into());
        }
        match source.kind {
            ToolCallKind::Custom => Ok(Some(source.name.clone())),
            ToolCallKind::Function => {
                self.folded_updates
                    .entry(remote_id.to_owned())
                    .or_default()
                    .push(update.text().text().to_owned());
                Ok(None)
            }
        }
    }

    fn finish(&self) -> Result<(), ResponsesRequestError> {
        if self.matched.len() != self.tool_calls.len() {
            return Err(ResponsesProtectedToolError::MissingResult.into());
        }
        Ok(())
    }
}

fn derive_tool_calls(
    request: &ModelRequest,
    config: ResponsesRequestConfig<'_>,
    custom_tools: &BTreeSet<ToolName>,
    first_replay_position: usize,
) -> Result<BTreeMap<String, ToolCallSource>, ResponsesRequestError> {
    let mut calls = BTreeMap::new();
    for (item_index, item) in request.input().items().iter().enumerate() {
        if item_index >= first_replay_position
            && let Some((block_index, replay)) = exact_output_replay(item_index, item, config)?
        {
            if let Some(replayed) = replay_output_for_policy(config, item_index, replay)? {
                remember_replayed_call(
                    &replayed,
                    CanonicalPosition::new(item_index, block_index),
                    item,
                    &mut calls,
                )?;
            }
            continue;
        }
        for (block_index, block) in item.blocks().iter().enumerate() {
            let ContentBlock::ToolCall(call) = block else {
                continue;
            };
            if item.actor() != Actor::Assistant {
                return Err(ResponsesProtectedToolError::InvalidCallActor.into());
            }
            if call.state() != ToolCallState::Requested {
                return Err(ResponsesProtectedToolError::InvalidCallState.into());
            }
            if !matches!(
                call.owner(),
                ToolExecutionOwner::PlakLocal | ToolExecutionOwner::PlakMcp { .. }
            ) {
                return Err(ResponsesProtectedToolError::OwnerMismatch.into());
            }
            let remote_id = call
                .correlation()
                .remote_call_id()
                .map_or_else(|| call.call_id().to_string(), ToOwned::to_owned);
            let kind = if custom_tools.contains(call.exposed_name()) {
                ToolCallKind::Custom
            } else {
                ToolCallKind::Function
            };
            remember_tool_call(
                &mut calls,
                remote_id,
                ToolCallSource {
                    position: CanonicalPosition::new(item_index, block_index),
                    kind,
                    call_id: call.call_id(),
                    owner: call.owner().clone(),
                    name: call.exposed_name().clone(),
                    caller_lineage: call.correlation().caller_lineage().clone(),
                },
            )?;
        }
    }
    Ok(calls)
}

fn remember_replayed_call(
    replayed: &Value,
    position: CanonicalPosition,
    item: &ConversationItem,
    calls: &mut BTreeMap<String, ToolCallSource>,
) -> Result<(), ResponsesRequestError> {
    let kind = match replayed.get("type").and_then(Value::as_str) {
        Some("function_call") => ToolCallKind::Function,
        Some("custom_tool_call") => ToolCallKind::Custom,
        _ => return Ok(()),
    };
    let call_id = replayed
        .get("call_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= MAX_RESPONSE_ID_BYTES)
        .ok_or(ResponsesReplayError::MalformedEnvelope)?;
    let mut canonical_calls = item.blocks().iter().filter_map(|block| match block {
        ContentBlock::ToolCall(call) => Some(call),
        _ => None,
    });
    let call = canonical_calls
        .next()
        .ok_or(ResponsesProtectedToolError::ResultWithoutCall)?;
    if canonical_calls.next().is_some()
        || item.actor() != Actor::Assistant
        || call.state() != ToolCallState::Requested
    {
        return Err(ResponsesProtectedToolError::InvalidCallActor.into());
    }
    remember_tool_call(
        calls,
        call_id.to_owned(),
        ToolCallSource {
            position,
            kind,
            call_id: call.call_id(),
            owner: call.owner().clone(),
            name: call.exposed_name().clone(),
            caller_lineage: call.correlation().caller_lineage().clone(),
        },
    )
}

fn remember_tool_call(
    calls: &mut BTreeMap<String, ToolCallSource>,
    call_id: String,
    source: ToolCallSource,
) -> Result<(), ResponsesRequestError> {
    if calls.insert(call_id, source).is_some() {
        return Err(ResponsesProtectedToolError::DuplicateCall.into());
    }
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "one exhaustive canonical-block match preserves source order"
)]
fn lower_item(
    config: ResponsesRequestConfig<'_>,
    item_index: usize,
    item: &ConversationItem,
    custom_tools: &BTreeSet<ToolName>,
    tool_results: &mut ToolResultMatches<'_>,
    output: &mut Vec<Value>,
) -> Result<(), ResponsesRequestError> {
    if let Some((_, replay)) = exact_output_replay(item_index, item, config)? {
        if let Some(replayed) = replay_output_for_policy(config, item_index, replay)? {
            output.push(replayed);
        }
        return Ok(());
    }
    let role = match item.actor() {
        Actor::Developer => "developer",
        Actor::System => "system",
        Actor::User | Actor::Tool | Actor::Harness => "user",
        Actor::Assistant => "assistant",
        _ => return Err(ResponsesRequestContentError::UnknownActor.into()),
    };
    let mut message_parts = Vec::new();
    for (block_index, block) in item.blocks().iter().enumerate() {
        match block {
            ContentBlock::Text(text) => {
                if item.actor() == Actor::Assistant {
                    message_parts.push(output_text_part(text.text()));
                } else {
                    message_parts.push(input_text_part(text.text()));
                }
            }
            ContentBlock::Image(image) => {
                if !config.policy.image_input {
                    return Err(ResponsesRequestContentError::ImageInputUnsupported.into());
                }
                let Some(uri) = image.source().as_uri() else {
                    return Err(ResponsesRequestContentError::InlineImageBytes.into());
                };
                message_parts.push(json!({"type": "input_image", "image_url": uri}));
            }
            ContentBlock::Refusal(_) if item.actor() == Actor::Assistant => {
                return Err(ResponsesRequestContentError::AssistantRefusalWithoutReplay.into());
            }
            ContentBlock::ToolCall(call) => {
                flush_message(role, &mut message_parts, output);
                if item.actor() != Actor::Assistant {
                    return Err(ResponsesProtectedToolError::InvalidCallActor.into());
                }
                if call.state() != ToolCallState::Requested {
                    return Err(ResponsesProtectedToolError::InvalidCallState.into());
                }
                if !matches!(
                    call.owner(),
                    ToolExecutionOwner::PlakLocal | ToolExecutionOwner::PlakMcp { .. }
                ) {
                    return Err(
                        ResponsesRequestContentError::ProviderHostedCallWithoutReplay.into(),
                    );
                }
                let remote_id = call
                    .correlation()
                    .remote_call_id()
                    .map_or_else(|| call.call_id().to_string(), ToOwned::to_owned);
                if custom_tools.contains(call.exposed_name()) {
                    let custom_input = call
                        .arguments()
                        .and_then(Value::as_str)
                        .ok_or(ResponsesRequestContentError::InvalidCustomToolArguments)?;
                    let mut wire = json!({
                        "type": "custom_tool_call",
                        "call_id": remote_id,
                        "name": call.exposed_name().as_str(),
                        "input": custom_input,
                    });
                    lower_program_caller(&mut wire, call.correlation().caller_lineage())?;
                    output.push(wire);
                } else {
                    // A UTF-8 cell (Codex Code Mode `exec`) replayed onto a target
                    // that does not expose the tool as custom: the function call
                    // carries the cell's original string input verbatim. The
                    // canonical block keeps that input as a JSON string literal
                    // so `arguments()` is `Some(String)` exactly for this shape;
                    // a structured call is always a JSON object.
                    let arguments = match call.arguments() {
                        Some(Value::String(input)) => input.as_str(),
                        _ => call.raw_arguments(),
                    };
                    let mut wire =
                        function_call_item(remote_id, call.exposed_name().as_str(), arguments);
                    lower_program_caller(&mut wire, call.correlation().caller_lineage())?;
                    output.push(wire);
                }
            }
            ContentBlock::ToolResult(result) => {
                flush_message(role, &mut message_parts, output);
                if item.actor() != Actor::Tool {
                    return Err(ResponsesProtectedToolError::InvalidResultActor.into());
                }
                let remote_id = result
                    .correlation()
                    .remote_call_id()
                    .map_or_else(|| result.call_id().to_string(), ToOwned::to_owned);
                let parts = lower_tool_result(result.parts())?;
                match tool_results.match_result(
                    &remote_id,
                    CanonicalPosition::new(item_index, block_index),
                    result,
                )? {
                    ToolCallKind::Custom => {
                        let mut wire = json!({
                        "type": "custom_tool_call_output",
                        "call_id": remote_id,
                        "output": parts,
                        });
                        lower_program_caller(&mut wire, result.correlation().caller_lineage())?;
                        output.push(wire);
                    }
                    ToolCallKind::Function => {
                        let parts = match tool_results.take_folded_updates(&remote_id) {
                            Some(updates) => fold_updates_into_output(updates, parts),
                            None => parts,
                        };
                        let mut wire = function_call_output_item(remote_id, parts);
                        lower_program_caller(&mut wire, result.correlation().caller_lineage())?;
                        output.push(wire);
                    }
                }
            }
            ContentBlock::ToolResultUpdate(update) => {
                flush_message(role, &mut message_parts, output);
                if item.actor() != Actor::Tool {
                    return Err(ResponsesProtectedToolError::InvalidResultActor.into());
                }
                let remote_id = update
                    .correlation()
                    .remote_call_id()
                    .map_or_else(|| update.call_id().to_string(), ToOwned::to_owned);
                if let Some(name) = tool_results.match_update(
                    &remote_id,
                    CanonicalPosition::new(item_index, block_index),
                    update,
                )? {
                    output.push(json!({
                        "type": "custom_tool_call_output",
                        "call_id": remote_id,
                        "name": name.as_str(),
                        "output": update.text().text(),
                    }));
                }
            }
            ContentBlock::Reasoning(reasoning) => {
                flush_message(role, &mut message_parts, output);
                if config.policy.replay == ResponsesReplayPolicy::DropUnreplayableReasoning {
                    continue;
                }
                let Some(replay) = reasoning.replay() else {
                    continue;
                };
                if replay_authority(config, item_index, replay)? == ReplayAuthority::Active {
                    let replayed = replay_output_for_policy(config, item_index, replay)?
                        .ok_or(ResponsesReplayError::StrictEncryptedReasoning)?;
                    output.push(replayed);
                }
            }
            ContentBlock::Opaque(opaque) => {
                if replay_authority(config, item_index, opaque.replay())? == ReplayAuthority::Active
                {
                    flush_message(role, &mut message_parts, output);
                    output.push(replay_output_item(config, item_index, opaque.replay())?);
                } else {
                    match opaque.fallback() {
                        OpaqueFallback::Omit => {}
                        OpaqueFallback::SemanticSummary(summary) => {
                            if item.actor() == Actor::Assistant {
                                message_parts.push(output_text_part(summary));
                            } else {
                                message_parts.push(input_text_part(summary));
                            }
                        }
                        OpaqueFallback::CompatibleReplay => {
                            if !has_complete_portable_item_semantics(item) {
                                return Err(
                                    ResponsesRequestContentError::OpaqueOnlyWithoutCompatibleReplay
                                        .into(),
                                );
                            }
                        }
                        _ => {
                            return Err(ResponsesRequestContentError::UnknownCanonicalBlock.into());
                        }
                    }
                }
            }
            ContentBlock::Document(_) | ContentBlock::Audio(_) => {
                return Err(ResponsesRequestContentError::DocumentOrAudio.into());
            }
            ContentBlock::Citation(_) => {
                return Err(ResponsesRequestContentError::StandaloneCitation.into());
            }
            ContentBlock::Compaction(compaction) => {
                flush_message(role, &mut message_parts, output);
                if let Some(wire) = bound_native_compaction_item(config, compaction)? {
                    if item.actor() != Actor::Assistant {
                        return Err(ResponsesRequestContentError::CompactionWithoutReplay.into());
                    }
                    output.push(wire);
                    continue;
                }
                if compaction.native_item().is_some() {
                    return Err(ResponsesRequestContentError::CompactionWithoutReplay.into());
                }
                if item.actor() == Actor::Assistant {
                    message_parts.push(output_text_part(compaction.summary()));
                } else {
                    return Err(ResponsesRequestContentError::CompactionWithoutReplay.into());
                }
            }
            _ => return Err(ResponsesRequestContentError::UnknownCanonicalBlock.into()),
        }
    }
    flush_message(role, &mut message_parts, output);
    Ok(())
}

/// Binding-checked next-turn replay of a native compact item.
///
/// Reconstructs `{type:compaction,encrypted_content}` from the stored
/// ciphertext after exact producing-target, `type` and ciphertext checks. Extra native keys
/// (for example `id`) are not forwarded. Other Responses policies stay
/// fail-closed on `native_item`.
fn bound_native_compaction_item(
    config: ResponsesRequestConfig<'_>,
    compaction: &CompactionBlock,
) -> Result<Option<Value>, ResponsesRequestError> {
    let Some(native_item) = compaction.native_item() else {
        return Ok(None);
    };
    if !config.policy.authorizes_native_compaction_item_replay()
        || !config
            .account
            .copied()
            .zip(compaction.native_binding())
            .is_some_and(|(account, binding)| {
                binding.is_compatible(config.protocol, config.model_family, config.model, account)
            })
    {
        return Err(ResponsesRequestContentError::CompactionWithoutReplay.into());
    }
    let parsed: Value = serde_json::from_str(native_item)
        .map_err(|_| ResponsesRequestContentError::CompactionWithoutReplay)?;
    let object = parsed
        .as_object()
        .ok_or(ResponsesRequestContentError::CompactionWithoutReplay)?;
    if object.get("type").and_then(Value::as_str) != Some("compaction") {
        return Err(ResponsesRequestContentError::CompactionWithoutReplay.into());
    }
    let encrypted = object
        .get("encrypted_content")
        .and_then(Value::as_str)
        .ok_or(ResponsesRequestContentError::CompactionWithoutReplay)?;
    if encrypted.is_empty() || encrypted != compaction.summary() {
        return Err(ResponsesRequestContentError::CompactionWithoutReplay.into());
    }
    Ok(Some(json!({
        "type": "compaction",
        "encrypted_content": compaction.summary(),
    })))
}

fn lower_program_caller(
    wire: &mut Value,
    lineage: &ToolCallerLineage,
) -> Result<(), ResponsesRequestError> {
    let (kind, execution_id) = match lineage {
        ToolCallerLineage::Direct => return Ok(()),
        ToolCallerLineage::LocalProgrammatic { .. } => {
            return Err(ResponsesRequestContentError::ProviderHostedCallWithoutReplay.into());
        }
        ToolCallerLineage::ProviderProgrammatic { kind, execution_id } => (kind, execution_id),
    };
    if kind.as_str() != "openai.responses.program" {
        return Err(ResponsesRequestContentError::ProviderHostedCallWithoutReplay.into());
    }
    wire.as_object_mut()
        .ok_or(ResponsesRequestError::Serialization)?
        .insert(
            "caller".to_owned(),
            json!({"type":"program", "caller_id":execution_id}),
        );
    Ok(())
}

/// An unbound or foreign native output item may be omitted only when the same
/// canonical item independently carries all semantics this target is allowed
/// to project. Exact target-bound replay returns before this predicate is used.
fn has_complete_portable_item_semantics(item: &ConversationItem) -> bool {
    if item.actor() != Actor::Assistant || item.provenance() != Provenance::ModelOutput {
        return false;
    }
    let mut semantic_blocks = 0_usize;
    for block in item.blocks() {
        match block {
            ContentBlock::Opaque(_) => {}
            ContentBlock::Text(text)
                if text.phase().is_none()
                    || text.phase() == Some(plakous_model::TextPhase::Final)
                    || text.phase() == Some(plakous_model::TextPhase::Commentary) =>
            {
                semantic_blocks = semantic_blocks.saturating_add(1);
            }
            ContentBlock::ToolCall(call)
                if call.state() == ToolCallState::Requested
                    && matches!(
                        call.owner(),
                        ToolExecutionOwner::PlakLocal | ToolExecutionOwner::PlakMcp { .. }
                    ) =>
            {
                semantic_blocks = semantic_blocks.saturating_add(1);
            }
            ContentBlock::Reasoning(_) | ContentBlock::Compaction(_) => {
                semantic_blocks = semantic_blocks.saturating_add(1);
            }
            _ => return false,
        }
    }
    semantic_blocks != 0
}

fn flush_message(role: &str, parts: &mut Vec<Value>, output: &mut Vec<Value>) {
    if !parts.is_empty() {
        output.push(message_item(role, std::mem::take(parts)));
    }
}

/// Prepends folded cell yields to a function call's output, in source order.
fn fold_updates_into_output(updates: Vec<String>, output: Value) -> Value {
    let mut parts: Vec<Value> = updates
        .into_iter()
        .map(|text| input_text_part(&text))
        .collect();
    match output {
        Value::Array(rest) => parts.extend(rest),
        Value::String(text) => parts.push(input_text_part(&text)),
        other => parts.push(other),
    }
    Value::Array(parts)
}

fn lower_tool_result(parts: &[ToolResultPart]) -> Result<Value, ResponsesRequestError> {
    if let [ToolResultPart::Text(text)] = parts {
        return Ok(Value::String(text.text().to_owned()));
    }
    let mut output = Vec::with_capacity(parts.len());
    for part in parts {
        match part {
            ToolResultPart::Text(text) => output.push(input_text_part(text.text())),
            ToolResultPart::Image(image) => {
                let image_url = exact_media_url(image.source())?;
                let detail = image.detail().map(image_detail_wire).transpose()?;
                let mut item = json!({"type": "input_image", "image_url": image_url});
                if let Some(detail) = detail {
                    item["detail"] = Value::String(detail.to_owned());
                }
                output.push(item);
            }
            ToolResultPart::Audio(audio) => output.push(json!({
                "type": "input_audio",
                "audio_url": exact_media_url(audio.source())?,
            })),
            ToolResultPart::Document(_) | ToolResultPart::Opaque(_) => {
                return Err(ResponsesRequestContentError::NonTextToolResult.into());
            }
            _ => return Err(ResponsesRequestContentError::NonTextToolResult.into()),
        }
    }
    Ok(Value::Array(output))
}

fn exact_media_url(source: &MediaSource) -> Result<&str, ResponsesRequestError> {
    source
        .as_data_url()
        .or_else(|| source.as_uri())
        .ok_or_else(|| ResponsesRequestContentError::NonTextToolResult.into())
}

fn image_detail_wire(detail: ImageDetailHint) -> Result<&'static str, ResponsesRequestError> {
    match detail {
        ImageDetailHint::Auto => Ok("auto"),
        ImageDetailHint::Low => Ok("low"),
        ImageDetailHint::High => Ok("high"),
        ImageDetailHint::Original => Ok("original"),
        _ => Err(ResponsesRequestContentError::ToolResultImage.into()),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReplayAuthority {
    Active,
    Unbound,
}

fn replay_authority(
    config: ResponsesRequestConfig<'_>,
    canonical_position: usize,
    replay: &ReplayEnvelope,
) -> Result<ReplayAuthority, ResponsesRequestError> {
    if !replay.is_compatible(config.protocol, config.model_family, config.model) {
        return Ok(ReplayAuthority::Unbound);
    }
    let Some(context) = config.replay else {
        return Ok(ReplayAuthority::Unbound);
    };
    let coordinate = context
        .output_coordinates
        .binary_search_by_key(&canonical_position, |coordinate| {
            coordinate.canonical_position
        })
        .ok()
        .map(|index| context.output_coordinates[index]);
    match coordinate {
        Some(coordinate) if coordinate.envelope == replay.digest() => Ok(ReplayAuthority::Active),
        Some(_) => Err(ResponsesReplayError::MissingOutputCoordinate.into()),
        None => Ok(ReplayAuthority::Unbound),
    }
}

fn exact_output_replay<'a>(
    item_index: usize,
    item: &'a ConversationItem,
    config: ResponsesRequestConfig<'_>,
) -> Result<Option<(usize, &'a ReplayEnvelope)>, ResponsesRequestError> {
    let Some(context) = config.replay else {
        return Ok(None);
    };
    let mut exact = None;
    for (block_index, block) in item.blocks().iter().enumerate() {
        let replay = match block {
            ContentBlock::Opaque(opaque) => Some(opaque.replay()),
            ContentBlock::Reasoning(reasoning) => reasoning.replay(),
            _ => None,
        };
        if let Some(replay) = replay
            && replay_authority(config, item_index, replay)? == ReplayAuthority::Active
            && (replay.kind().as_str() == context.output_item.name
                || context
                    .opaque_output
                    .is_some_and(|kind| replay.kind().as_str() == kind.name))
            && exact.replace((block_index, replay)).is_some()
        {
            return Err(ResponsesReplayError::MultipleOutputReplay.into());
        }
    }
    Ok(exact)
}

fn replay_output_item(
    config: ResponsesRequestConfig<'_>,
    item_index: usize,
    replay: &ReplayEnvelope,
) -> Result<Value, ResponsesRequestError> {
    let bound = bound_output_item(config, item_index, replay)?;
    serde_json::from_str(bound.item.get())
        .map_err(|_| ResponsesReplayError::MalformedEnvelope.into())
}

/// Grok Build: `status` is output-only and rejected on input; function_call
/// `id`/`status` are likewise omitted. Reasoning `content` parts need the
/// `reasoning_text` discriminator when present.
fn omit_output_only_input_fields(policy: ResponsesRequestPolicy, item: &mut Value) {
    if !policy.omits_output_only_input_fields() {
        return;
    }
    let Some(kind) = item.get("type").and_then(Value::as_str).map(str::to_owned) else {
        return;
    };
    let Some(object) = item.as_object_mut() else {
        return;
    };
    match kind.as_str() {
        "reasoning" => {
            object.remove("status");
            if let Some(content) = object.get_mut("content").and_then(Value::as_array_mut) {
                for part in content {
                    if let Some(part) = part.as_object_mut() {
                        part.entry("type")
                            .or_insert_with(|| Value::String("reasoning_text".to_owned()));
                    }
                }
            }
        }
        "function_call" | "function_call_output" => {
            object.remove("id");
            object.remove("status");
        }
        _ => {}
    }
}

fn replay_output_for_policy(
    config: ResponsesRequestConfig<'_>,
    item_index: usize,
    replay: &ReplayEnvelope,
) -> Result<Option<Value>, ResponsesRequestError> {
    let replayed = replay_output_item(config, item_index, replay)?;
    if replayed.get("type").and_then(Value::as_str) == Some("reasoning") {
        // Codex's reference-client reasoning item has no `status` field,
        // while SuperGrok's exact proxy item does. A present status must be
        // terminal; absence is valid only because the dialect-specific
        // projection decides whether status is required before promotion.
        let terminal = replayed
            .get("status")
            .and_then(Value::as_str)
            .is_none_or(|status| status == "completed");
        let replayable = terminal
            && replayed
                .get("encrypted_content")
                .and_then(Value::as_str)
                .is_some_and(|value| !value.is_empty());
        return match config.policy.replay {
            ResponsesReplayPolicy::Strict if !replayable => {
                Err(ResponsesReplayError::StrictEncryptedReasoning.into())
            }
            ResponsesReplayPolicy::DropUnreplayableReasoning => Ok(replayable.then_some(replayed)),
            ResponsesReplayPolicy::Strict => Ok(Some(replayed)),
        };
    }
    Ok(Some(replayed))
}

struct ValidatedOutputItem<'a> {
    group: Digest,
    output_index: u32,
    output_count: u32,
    item: &'a RawValue,
}

fn bound_output_item<'a>(
    config: ResponsesRequestConfig<'_>,
    canonical_position: usize,
    replay: &'a ReplayEnvelope,
) -> Result<ValidatedOutputItem<'a>, ResponsesRequestError> {
    let context = config
        .replay
        .ok_or(ResponsesReplayError::IncompatibleEnvelope)?;
    if context
        .expires_at
        .is_some_and(|expiry| expiry <= context.now)
    {
        return Err(ResponsesReplayError::ArtifactExpired.into());
    }
    let kind_matches = (replay.version() == context.output_item.version
        && replay.kind().as_str() == context.output_item.name)
        || context.opaque_output.is_some_and(|kind| {
            replay.version() == kind.version && replay.kind().as_str() == kind.name
        });
    if !kind_matches || !replay.is_compatible(config.protocol, config.model_family, config.model) {
        return Err(ResponsesReplayError::IncompatibleEnvelope.into());
    }
    if let Some(coordinate) = context.output_for(canonical_position, replay.digest()) {
        if !context
            .prefix_bindings
            .iter()
            .any(|entry| entry.binding == coordinate.binding)
            || coordinate.group_start > coordinate.canonical_position
        {
            return Err(ResponsesReplayError::InvalidOutputCoordinates.into());
        }
        return Ok(ValidatedOutputItem {
            group: coordinate.group,
            output_index: coordinate.output_index,
            output_count: coordinate.output_count,
            item: replay.payload(),
        });
    }
    Err(ResponsesReplayError::MissingOutputCoordinate.into())
}

fn validate_output_replay_sequence(
    request: &ModelRequest,
    config: ResponsesRequestConfig<'_>,
    first_position: usize,
) -> Result<(), ResponsesRequestError> {
    let mut completed_responses = HashSet::with_capacity(
        config
            .replay
            .map_or(0, |context| context.output_coordinates.len()),
    );
    let mut active: Option<(Digest, u32, u32)> = None;
    for (canonical_position, item) in request
        .input()
        .items()
        .iter()
        .enumerate()
        .skip(first_position)
    {
        let Some((_, replay)) = exact_output_replay(canonical_position, item, config)? else {
            close_replay_group(&mut active, &mut completed_responses)?;
            continue;
        };
        let bound = bound_output_item(config, canonical_position, replay)?;
        match &mut active {
            Some((group, next_index, output_count)) if *group == bound.group => {
                if *next_index != bound.output_index || *output_count != bound.output_count {
                    return Err(ResponsesReplayError::IncompleteOutputGroup.into());
                }
                *next_index = next_index
                    .checked_add(1)
                    .ok_or(ResponsesReplayError::InvalidOutputCoordinates)?;
            }
            Some(_) => {
                close_replay_group(&mut active, &mut completed_responses)?;
                start_replay_group(&mut active, &completed_responses, &bound)?;
            }
            None => start_replay_group(&mut active, &completed_responses, &bound)?,
        }
    }
    close_replay_group(&mut active, &mut completed_responses)
}

fn start_replay_group(
    active: &mut Option<(Digest, u32, u32)>,
    completed_responses: &HashSet<Digest>,
    bound: &ValidatedOutputItem<'_>,
) -> Result<(), ResponsesRequestError> {
    if completed_responses.contains(&bound.group) {
        return Err(ResponsesReplayError::RepeatedOutputGroup.into());
    }
    if bound.output_index != 0 {
        return Err(ResponsesReplayError::IncompleteOutputGroup.into());
    }
    *active = Some((bound.group, 1, bound.output_count));
    Ok(())
}

fn close_replay_group(
    active: &mut Option<(Digest, u32, u32)>,
    completed_responses: &mut HashSet<Digest>,
) -> Result<(), ResponsesRequestError> {
    let Some((group, next_index, output_count)) = active.take() else {
        return Ok(());
    };
    if next_index != output_count {
        return Err(ResponsesReplayError::IncompleteOutputGroup.into());
    }
    if !completed_responses.insert(group) {
        return Err(ResponsesReplayError::RepeatedOutputGroup.into());
    }
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit fixtures use expect/unwrap for clarity"
)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::num::NonZeroU32;

    #[test]
    fn binding_mismatch_is_treated_as_not_this_request() {
        let binding = ResponsesRequestError::Replay(ResponsesReplayError::ArtifactBindingMismatch);
        assert!(binding.artifact_is_not_for_this_request());
        let canonical =
            ResponsesRequestError::Replay(ResponsesReplayError::ArtifactCanonicalMismatch);
        assert!(canonical.artifact_is_not_for_this_request());
        let expired = ResponsesRequestError::Replay(ResponsesReplayError::ArtifactExpired);
        assert!(expired.artifact_is_not_for_this_request());
        let coords = ResponsesRequestError::Replay(ResponsesReplayError::InvalidOutputCoordinates);
        assert!(coords.artifact_is_not_for_this_request());
        let prefix =
            ResponsesRequestError::Replay(ResponsesReplayError::ArtifactStablePrefixMismatch);
        assert!(prefix.artifact_is_not_for_this_request());
        let foreign = ResponsesRequestError::Replay(ResponsesReplayError::ArtifactTargetMismatch {
            axis: "model",
        });
        assert!(!foreign.artifact_is_not_for_this_request());
    }

    use plakous_model::{
        AttemptId, BranchId, CallId, CanonicalHistory, Capability, CapabilityConstraints,
        CapabilityRequirement, CapabilitySet, CompactionBlock, ConfiguredTool,
        ContextAdmissionRequirement, ConversationView, CredentialSelectionId, DataClassification,
        Digest, DigestDomain, EntryId, FactProvenance, FactSource, FeatureSupport,
        GenerationLimits, HistorySegment, HistorySegmentKind, InstructionBlock, InstructionSource,
        LocalDurabilityDisposition, ModelAdapterProfileId, ModelCacheAffinity, NamespaceId,
        NativeOptions, OpaqueBlock, OpaqueFallback, OutputPolicy, ProviderAuthorityClassId,
        ProviderBehaviorConfiguration, ProviderProfileRevisionId, ProviderRouteSelection,
        ProviderSemanticEvidence, ProviderServiceTierId, ReasoningBlock, ReasoningVisibility,
        ReplayKindId, ReplayScope, RequestMetadata, ResolvedModelDescriptor, RouteId, RunId,
        RuntimeModelAuthority, SessionId, SessionWorkflowRootId, Support, TextBlock, TextPhase,
        ToolCallBlock, ToolCallState, ToolCallerDisposition, ToolCallerLineage, ToolConcurrency,
        ToolCorrelation, ToolEffect, ToolExecutionDisposition, ToolInputContract,
        ToolOutputContract, ToolResultBlock, ToolResultKind, TurnId, ValidationMode,
        WorkingProjection,
    };
    use plakous_responses::{
        ResponsesArtifactCodec, ResponsesArtifactProduction, ResponsesArtifactReplayKind,
        ResponsesContinuation, ResponsesReplayContract, ResponsesReplayItem,
        ResponsesSemanticRevisions,
    };
    use uuid::Uuid;

    use crate::project_host::{
        ResponsesProviderRequestBinding, ResponsesReplayProjection, project_terminal_response,
    };
    use crate::response_host::{ResponsesResponse, ResponsesResponseLimits, ResponsesUsagePolicy};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn hosted_tool_and_allowed_caller_wire_shapes_are_exact() -> TestResult {
        assert_eq!(
            lower_hosted_tool(HostedTool::ProgrammaticComposition),
            json!({"type":"programmatic_tool_calling"})
        );
        assert_eq!(
            lower_hosted_tool(HostedTool::CodeInterpreter),
            json!({"type":"code_interpreter"})
        );
        for (caller, expected) in [
            (
                ToolCallerDisposition::DirectOnly,
                json!({"type":"function"}),
            ),
            (
                ToolCallerDisposition::ProviderProgrammaticOnly,
                json!({"type":"function","allowed_callers":["programmatic"]}),
            ),
            (
                ToolCallerDisposition::DirectAndProviderProgrammatic,
                json!({"type":"function","allowed_callers":["direct","programmatic"]}),
            ),
        ] {
            let mut wire = json!({"type":"function"});
            lower_allowed_callers(&mut wire, caller)?;
            assert_eq!(wire, expected);
        }
        Ok(())
    }

    #[test]
    fn local_programmatic_lineage_is_rejected_at_provider_wire_boundary() -> TestResult {
        let lineage = ToolCallerLineage::local_programmatic(
            1,
            1,
            "cell-1",
            1,
            plakous_model::CallId::try_from(Uuid::from_u128(1))?,
            1,
        )?;
        let mut wire = json!({"type":"function"});
        let error = lower_program_caller(&mut wire, &lineage).expect_err("local lineage lowered");
        assert!(error.to_string().contains("provider-hosted"));
        assert_eq!(wire, json!({"type":"function"}));
        Ok(())
    }

    fn id<T>(value: u128) -> Result<T, plakous_model::IdentityError>
    where
        T: TryFrom<Uuid, Error = plakous_model::IdentityError>,
    {
        T::try_from(Uuid::from_u128(value))
    }

    fn metadata() -> Result<RequestMetadata, plakous_model::IdentityError> {
        Ok(RequestMetadata::for_turn(
            id::<RunId>(1)?,
            id::<TurnId>(2)?,
            id::<AttemptId>(3)?,
            Timestamp::new(1_700_000_000, 0)?,
        ))
    }

    fn workflow_metadata() -> Result<RequestMetadata, plakous_model::IdentityError> {
        Ok(RequestMetadata::for_session_workflow(
            id::<RunId>(1)?,
            SessionWorkflowRootId::new([3; 32])?,
            id::<AttemptId>(3)?,
            Timestamp::new(1_700_000_000, 0)?,
        ))
    }

    #[test]
    fn openai_codex_session_workflow_emits_no_native_compaction_contract() -> TestResult {
        let (protocol, family, model) = target_values()?;
        let request = crate::test_support::model_request_builder(
            ConversationView::new(vec![user(21, "compact this")?]),
            workflow_metadata()?,
        )
        .build()?;
        let lowered = lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
                None,
            ),
        )?;
        let input = lowered.body()["input"].as_array().ok_or("input")?;
        assert!(
            input
                .iter()
                .all(|item| item["type"] != "compaction_trigger")
        );
        assert_eq!(lowered.body().get("client_metadata"), None);
        Ok(())
    }

    #[test]
    fn openai_codex_native_compaction_trigger_emits_compaction_trigger_item() -> TestResult {
        let (protocol, family, model) = target_values()?;
        let namespace = NamespaceId::new("plakous.compaction")?;
        let native = NativeOptions::new(
            [(
                namespace,
                Value::String("native_responses_trigger_v1".into()),
            )]
            .into_iter()
            .collect(),
        );
        let request = crate::test_support::model_request_builder(
            ConversationView::new(vec![user(22, "compact this")?]),
            workflow_metadata()?,
        )
        .native(native)
        .build()?;
        let lowered = lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
                None,
            ),
        )?;
        let input = lowered.body()["input"].as_array().ok_or("input")?;
        assert_eq!(
            input.last().ok_or("last")?["type"].as_str(),
            Some("compaction_trigger")
        );
        let without = crate::test_support::model_request_builder(
            ConversationView::new(vec![user(22, "compact this")?]),
            workflow_metadata()?,
        )
        .build()?;
        assert_ne!(request.identity(), without.identity());
        Ok(())
    }

    #[test]
    fn openai_codex_unknown_native_options_are_rejected() -> TestResult {
        let (protocol, family, model) = target_values()?;
        let namespace = NamespaceId::new("plakous.unknown")?;
        let native = NativeOptions::new(
            [(namespace, Value::String("x".into()))]
                .into_iter()
                .collect(),
        );
        let request = crate::test_support::model_request_builder(
            ConversationView::new(vec![user(23, "no")?]),
            workflow_metadata()?,
        )
        .native(native)
        .build()?;
        let error = lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
                None,
            ),
        )
        .expect_err("unknown native options");
        assert!(matches!(
            error,
            ResponsesRequestError::Option(ResponsesRequestOptionError::NativeOptions)
        ));
        Ok(())
    }

    #[test]
    fn codex_service_tier_is_exact_and_orthogonal_to_reasoning() -> TestResult {
        let (protocol, family, model) = target_values()?;
        let policy =
            ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content");
        let standard = crate::test_support::model_request_builder(
            ConversationView::new(vec![user(31, "standard")?]),
            metadata()?,
        )
        .reasoning(ReasoningPolicy::Enabled {
            effort: Some(ReasoningEffort::High),
            requirement: CapabilityRequirement::Required,
        })
        .build()?;
        let priority = crate::test_support::model_request_builder(
            ConversationView::new(vec![user(32, "priority")?]),
            metadata()?,
        )
        .processing_tier(ProcessingTierDisposition::ProviderNamed(
            ProviderServiceTierId::new("priority")?,
        ))
        .reasoning(ReasoningPolicy::Enabled {
            effort: Some(ReasoningEffort::Low),
            requirement: CapabilityRequirement::Required,
        })
        .build()?;
        let standard = lower_responses_request(
            &standard,
            ResponsesRequestConfig::new(&protocol, Some(&family), &model, policy, None),
        )?;
        let priority = lower_responses_request(
            &priority,
            ResponsesRequestConfig::new(&protocol, Some(&family), &model, policy, None),
        )?;
        assert_eq!(standard.body().get("service_tier"), None);
        assert_eq!(standard.body()["reasoning"]["effort"], "high");
        assert_eq!(priority.body()["service_tier"], "priority");
        assert_eq!(priority.body()["reasoning"]["effort"], "low");
        Ok(())
    }

    #[test]
    fn non_codex_responses_policy_rejects_named_service_tier() -> TestResult {
        let (protocol, family, model) = target_values()?;
        let request = crate::test_support::model_request_builder(
            ConversationView::new(vec![user(33, "unsupported")?]),
            metadata()?,
        )
        .processing_tier(ProcessingTierDisposition::ProviderNamed(
            ProviderServiceTierId::new("priority")?,
        ))
        .build()?;
        assert!(
            lower_responses_request(
                &request,
                ResponsesRequestConfig::new(
                    &protocol,
                    Some(&family),
                    &model,
                    ResponsesRequestPolicy::openai_platform(
                        ResponsesStoragePolicy::Stateless,
                        false,
                        false,
                    ),
                    None,
                ),
            )
            .is_err()
        );
        Ok(())
    }

    fn native_compaction_prior(
        summary: &str,
        native: &str,
    ) -> Result<ConversationItem, Box<dyn std::error::Error>> {
        Ok(ConversationItem::new(
            id::<EntryId>(22)?,
            Actor::Assistant,
            vec![ContentBlock::Compaction(
                CompactionBlock::new(summary)?.with_native_item(native.to_owned()),
            )],
            Provenance::ModelOutput,
        )?)
    }

    #[test]
    fn requested_calls_with_results_prepare_fresh_on_shared_responses_routes() -> TestResult {
        let (protocol, family, model) = target_values()?;
        for owner in [
            ToolExecutionOwner::PlakLocal,
            ToolExecutionOwner::PlakMcp {
                server: plakous_model::McpServerId::new("fixture")?,
            },
        ] {
            for (raw, expected) in [
                (r#"{"q":"portable"}"#, r#"{"q":"portable"}"#),
                (r#""cell source""#, "cell source"),
            ] {
                let state = ToolCallState::Requested;
                let call = ToolCallBlock::new(
                    id::<CallId>(25)?,
                    ToolName::new("lookup")?,
                    owner.clone(),
                    state,
                    ToolCorrelation::new(None, None, ToolCallerLineage::Direct)?,
                    raw,
                )?;
                let items = vec![
                    user(24, "look up")?,
                    ConversationItem::new(
                        id::<EntryId>(25)?,
                        Actor::Assistant,
                        vec![
                            ContentBlock::ToolCall(call.clone()),
                            native_output(&protocol, &family, &model, "call-native")?,
                        ],
                        Provenance::ModelOutput,
                    )?,
                    ConversationItem::new(
                        id::<EntryId>(26)?,
                        Actor::Tool,
                        vec![ContentBlock::ToolResult(ToolResultBlock::new(
                            call.call_id(),
                            owner.clone(),
                            ToolResultKind::Success,
                            vec![ToolResultPart::Text(TextBlock::new("found", None)?)],
                            ToolCorrelation::new(None, None, ToolCallerLineage::Direct)?,
                        )?)],
                        Provenance::ToolExecution,
                    )?,
                ];
                let request = crate::test_support::model_request_builder(
                    ConversationView::new(items),
                    workflow_metadata()?,
                )
                .build()?;
                for policy in [
                    ResponsesRequestPolicy::openai_codex(
                        false,
                        false,
                        "reasoning.encrypted_content",
                    ),
                    ResponsesRequestPolicy::supergrok_stateless(),
                    ResponsesRequestPolicy::openrouter_stateless(),
                ] {
                    let lowered = lower_responses_request(
                        &request,
                        ResponsesRequestConfig::new(&protocol, Some(&family), &model, policy, None),
                    )?;
                    assert_eq!(lowered.body()["input"][1]["type"], "function_call");
                    assert_eq!(lowered.body()["input"][1]["arguments"], expected);
                    assert_eq!(lowered.body()["input"][2]["output"], "found");
                }
            }
        }
        Ok(())
    }

    #[test]
    fn unbound_compaction_requires_exact_replay() -> TestResult {
        let (protocol, family, model) = target_values()?;
        let request = crate::test_support::model_request_builder(
            ConversationView::new(vec![native_compaction_prior(
                "opaque-ciphertext",
                r#"{"type":"compaction","encrypted_content":"opaque-ciphertext"}"#,
            )?]),
            workflow_metadata()?,
        )
        .build()?;
        assert!(matches!(
            lower_responses_request(
                &request,
                ResponsesRequestConfig::new(
                    &protocol,
                    Some(&family),
                    &model,
                    ResponsesRequestPolicy::openai_codex(
                        false,
                        false,
                        "reasoning.encrypted_content"
                    ),
                    None
                )
            ),
            Err(ResponsesRequestError::Content(
                ResponsesRequestContentError::CompactionWithoutReplay
            ))
        ));
        Ok(())
    }

    fn compaction_binding()
    -> Result<plakous_model::CompactionReplayBinding, Box<dyn std::error::Error>> {
        let (protocol, family, model) = target_values()?;
        Ok(plakous_model::CompactionReplayBinding::new(
            protocol,
            Some(family),
            model,
            Digest::sha256(DigestDomain::ModelBinding, 1, &[b"responses-test-route"])?,
        ))
    }

    fn bound_compaction_prior(
        summary: &str,
        native: &str,
    ) -> Result<ConversationItem, Box<dyn std::error::Error>> {
        let (protocol, family, model) = target_values()?;
        let replay = ReplayEnvelope::new(
            protocol,
            Some(family),
            ReplayScope::ExactModel(model),
            ReplayKindId::new("responses.output_item")?,
            1,
            DataClassification::Sensitive,
            RawValue::from_string(native.to_owned())?,
        )?;
        Ok(ConversationItem::new(
            id::<EntryId>(22)?,
            Actor::Assistant,
            vec![
                ContentBlock::Compaction(
                    CompactionBlock::new(summary)?
                        .with_native_item(native.to_owned())
                        .with_native_binding(compaction_binding()?),
                ),
                ContentBlock::Opaque(OpaqueBlock::new(replay, OpaqueFallback::CompatibleReplay)?),
            ],
            Provenance::ModelOutput,
        )?)
    }

    #[test]
    fn projector_shaped_history_reopens_with_exact_artifact_and_prepares_fresh() -> TestResult {
        let (protocol, family, model) = target_values()?;
        let account = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"responses-test-route"])?;
        let make_replay = |native: Value| -> Result<ReplayEnvelope, Box<dyn std::error::Error>> {
            Ok(ReplayEnvelope::new(
                protocol.clone(),
                Some(family.clone()),
                ReplayScope::ExactModel(model.clone()),
                ReplayKindId::new("responses.output_item")?,
                1,
                DataClassification::Sensitive,
                RawValue::from_string(native.to_string())?,
            )?)
        };
        let blank_native = json!({"id":"blank", "type":"message", "role":"assistant", "status":"completed", "content":[{"type":"output_text","text":"","annotations":[]}]});
        let blank_replay = make_replay(blank_native.clone())?;
        let compact = bound_compaction_prior(
            "opaque-ciphertext",
            r#"{"type":"compaction","encrypted_content":"opaque-ciphertext"}"#,
        )?;
        let compact_replay = compact
            .blocks()
            .iter()
            .find_map(|block| match block {
                ContentBlock::Opaque(opaque) => Some(opaque.replay().clone()),
                _ => None,
            })
            .ok_or("compact replay")?;
        let call_native = json!({"id":"call-item","type":"function_call","status":"completed","call_id":"call-1","name":"lookup","arguments":"{}"});
        let call_replay = make_replay(call_native.clone())?;
        let call = ToolCallBlock::new(
            id::<CallId>(28)?,
            ToolName::new("lookup")?,
            ToolExecutionOwner::PlakLocal,
            ToolCallState::Requested,
            ToolCorrelation::new(Some("call-1".to_owned()), None, ToolCallerLineage::Direct)?,
            "{}",
        )?;
        let producing = crate::test_support::model_request_builder(
            ConversationView::new(vec![user(20, "first")?]),
            metadata()?,
        )
        .tools(vec![tool("lookup", false)?])
        .resolved_harness_plan_identity(harness_plan_identity()?)
        .build()?;
        let items = vec![
            user(20, "first")?,
            ConversationItem::new(
                id::<EntryId>(21)?,
                Actor::Assistant,
                vec![ContentBlock::Opaque(OpaqueBlock::new(
                    blank_replay.clone(),
                    OpaqueFallback::Omit,
                )?)],
                Provenance::ModelOutput,
            )?,
            compact,
            ConversationItem::new(
                id::<EntryId>(23)?,
                Actor::Assistant,
                vec![
                    ContentBlock::ToolCall(call.clone()),
                    ContentBlock::Opaque(OpaqueBlock::new(
                        call_replay.clone(),
                        OpaqueFallback::CompatibleReplay,
                    )?),
                ],
                Provenance::ModelOutput,
            )?,
            ConversationItem::new(
                id::<EntryId>(24)?,
                Actor::Tool,
                vec![ContentBlock::ToolResult(ToolResultBlock::new(
                    call.call_id(),
                    call.owner().clone(),
                    ToolResultKind::Success,
                    vec![ToolResultPart::Text(TextBlock::new("found", None)?)],
                    call.correlation().clone(),
                )?)],
                Provenance::ToolExecution,
            )?,
            user(25, "follow up")?,
        ];
        let request =
            crate::test_support::model_request_builder(ConversationView::new(items), metadata()?)
                .tools(vec![tool("lookup", false)?])
                .resolved_harness_plan_identity(harness_plan_identity()?)
                .build()?;
        let now = Timestamp::new(1_700_000_000, 0)?;
        let artifact = output_artifact(
            &producing,
            &request,
            "responses-test-route",
            vec![blank_replay, compact_replay, call_replay],
            ResponsesStorageMode::Stateless,
            "fixture-response",
            None,
            now,
            b"history-reopen",
        )?;
        let reopened_items: Vec<ConversationItem> =
            serde_json::from_slice(&serde_json::to_vec(&request.input().items().to_vec())?)?;
        assert_eq!(reopened_items.as_slice(), request.input().items().to_vec());
        let artifact =
            ResponsesArtifactCodec::decode_inert(&ResponsesArtifactCodec::encode(&artifact)?)?;
        let active = active_artifact(&artifact)?;
        let bridge = ResponsesArtifactReplayBridge::from_active_artifact(
            &active,
            &request,
            operation_authority(
                &request,
                "responses-test-route",
                ResponsesStoragePolicy::Stateless,
            )?,
            ResponsesReplayKind::new("responses.output_item", 1),
            None,
            None,
            now,
        )?;
        for policy in [
            ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
            ResponsesRequestPolicy::supergrok_stateless(),
            ResponsesRequestPolicy::openrouter_stateless(),
        ] {
            let lowered = lower_responses_request(
                &request,
                ResponsesRequestConfig::new(
                    &protocol,
                    Some(&family),
                    &model,
                    policy,
                    Some(bridge.context()),
                ),
            )?;
            assert_eq!(lowered.body()["input"][1], blank_native);
            assert_eq!(
                lowered.body()["input"][2]["encrypted_content"],
                "opaque-ciphertext"
            );
            let mut expected_call = call_native.clone();
            omit_output_only_input_fields(policy, &mut expected_call);
            assert_eq!(lowered.body()["input"][3], expected_call);
            let fresh = lower_responses_request(
                &request,
                ResponsesRequestConfig::new(&protocol, Some(&family), &model, policy, None)
                    .with_account_binding(&account),
            );
            if policy.authorizes_native_compaction_item_replay() {
                let fresh = fresh?;
                assert_eq!(fresh.body()["input"][1]["type"], "compaction");
                assert_eq!(fresh.body()["input"][2]["type"], "function_call");
                assert!(!fresh.body().to_string().contains("output_text"));
            } else {
                assert!(matches!(
                    fresh,
                    Err(ResponsesRequestError::Content(
                        ResponsesRequestContentError::CompactionWithoutReplay
                    ))
                ));
            }
        }
        for config in [
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
                None,
            ),
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
                None,
            )
            .with_account_binding(&Digest::sha256(
                DigestDomain::ModelBinding,
                1,
                &[b"other-account"],
            )?),
        ] {
            let error = lower_responses_request(&request, config)
                .expect_err("no foreign ciphertext replay");
            let classified =
                crate::ResponsesAttemptBuildError::Request(error).preparation_error_kind();
            assert_eq!(
                classified,
                plakous_model::ModelPreparationErrorKind::ProtectedHistory
            );
        }
        let other_protocol = ProtocolId::new("foreign.responses")?;
        let other_family = ModelFamilyId::new("foreign-family")?;
        let other_model = ModelId::new("foreign-model")?;
        for (dialect, target_family, target_model) in [
            (&other_protocol, &family, &model),
            (&protocol, &other_family, &model),
            (&protocol, &family, &other_model),
        ] {
            assert!(matches!(
                lower_responses_request(
                    &request,
                    ResponsesRequestConfig::new(
                        dialect,
                        Some(target_family),
                        target_model,
                        ResponsesRequestPolicy::openai_codex(
                            false,
                            false,
                            "reasoning.encrypted_content"
                        ),
                        None,
                    )
                    .with_account_binding(&account)
                ),
                Err(ResponsesRequestError::Content(
                    ResponsesRequestContentError::CompactionWithoutReplay
                ))
            ));
        }
        Ok(())
    }

    #[test]
    fn openai_codex_replays_bound_native_compaction_item() -> TestResult {
        let (protocol, family, model) = target_values()?;
        let prior = bound_compaction_prior(
            "opaque-ciphertext",
            r#"{"id":"compact_1","type":"compaction","encrypted_content":"opaque-ciphertext"}"#,
        )?;
        let request = crate::test_support::model_request_builder(
            ConversationView::new(vec![user(21, "continue")?, prior]),
            workflow_metadata()?,
        )
        .build()?;
        let lowered = lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
                None,
            )
            .with_account_binding(&Digest::sha256(
                DigestDomain::ModelBinding,
                1,
                &[b"responses-test-route"],
            )?),
        )?;
        let input = lowered.body()["input"].as_array().ok_or("input")?;
        let compaction = input
            .iter()
            .find(|item| item["type"] == "compaction")
            .ok_or("missing compaction item")?;
        assert_eq!(compaction["encrypted_content"], "opaque-ciphertext");
        assert!(compaction.get("id").is_none());
        Ok(())
    }

    #[test]
    fn openai_codex_rejects_compaction_ciphertext_mismatch() -> TestResult {
        let (protocol, family, model) = target_values()?;
        let prior = bound_compaction_prior(
            "semantic summary",
            r#"{"type":"compaction","encrypted_content":"opaque"}"#,
        )?;
        let request = crate::test_support::model_request_builder(
            ConversationView::new(vec![user(21, "continue")?, prior]),
            workflow_metadata()?,
        )
        .build()?;
        let lowered = lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
                None,
            )
            .with_account_binding(&Digest::sha256(
                DigestDomain::ModelBinding,
                1,
                &[b"responses-test-route"],
            )?),
        );
        assert!(matches!(
            lowered,
            Err(ResponsesRequestError::Content(
                ResponsesRequestContentError::CompactionWithoutReplay
            ))
        ));
        Ok(())
    }

    #[test]
    fn non_codex_responses_policy_rejects_native_compaction_replay() -> TestResult {
        let (protocol, family, model) = target_values()?;
        let prior = bound_compaction_prior(
            "opaque-ciphertext",
            r#"{"type":"compaction","encrypted_content":"opaque-ciphertext"}"#,
        )?;
        let request = crate::test_support::model_request_builder(
            ConversationView::new(vec![user(21, "continue")?, prior]),
            workflow_metadata()?,
        )
        .build()?;
        let lowered = lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::supergrok_stateless(),
                None,
            )
            .with_account_binding(&Digest::sha256(
                DigestDomain::ModelBinding,
                1,
                &[b"responses-test-route"],
            )?),
        );
        assert!(matches!(
            lowered,
            Err(ResponsesRequestError::Content(
                ResponsesRequestContentError::CompactionWithoutReplay
            ))
        ));
        Ok(())
    }

    fn user(value: u128, text: &str) -> Result<ConversationItem, Box<dyn std::error::Error>> {
        Ok(ConversationItem::new(
            id::<EntryId>(value)?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new(text, None)?)],
            Provenance::UserInput,
        )?)
    }

    fn native_output(
        protocol: &ProtocolId,
        family: &ModelFamilyId,
        model: &ModelId,
        marker: &str,
    ) -> Result<ContentBlock, Box<dyn std::error::Error>> {
        native_output_with_fallback(
            protocol,
            family,
            model,
            marker,
            OpaqueFallback::CompatibleReplay,
        )
    }

    fn native_output_with_fallback(
        protocol: &ProtocolId,
        family: &ModelFamilyId,
        model: &ModelId,
        marker: &str,
        fallback: OpaqueFallback,
    ) -> Result<ContentBlock, Box<dyn std::error::Error>> {
        let replay = ReplayEnvelope::new(
            protocol.clone(),
            Some(family.clone()),
            ReplayScope::ExactModel(model.clone()),
            ReplayKindId::new("responses.output_item")?,
            1,
            DataClassification::Sensitive,
            RawValue::from_string(json!({"type":"message","marker":marker}).to_string())?,
        )?;
        Ok(ContentBlock::Opaque(OpaqueBlock::new(replay, fallback)?))
    }

    fn cross_provider_history(
        protocol: &ProtocolId,
        family: &ModelFamilyId,
        model: &ModelId,
        base: u128,
        include_reasoning: bool,
    ) -> Result<Vec<ConversationItem>, Box<dyn std::error::Error>> {
        let call = ToolCallBlock::new(
            id::<CallId>(base)?,
            ToolName::new("lookup")?,
            ToolExecutionOwner::PlakLocal,
            ToolCallState::Requested,
            ToolCorrelation::new(
                Some(format!("call-{base}")),
                None,
                ToolCallerLineage::Direct,
            )?,
            r#"{"q":"portable"}"#,
        )?;
        let mut items = vec![ConversationItem::new(
            id::<EntryId>(base + 1)?,
            Actor::Assistant,
            vec![
                ContentBlock::Text(TextBlock::new("portable answer", Some(TextPhase::Final))?),
                native_output(protocol, family, model, "private-text-marker")?,
            ],
            Provenance::ModelOutput,
        )?];
        if include_reasoning {
            items.push(ConversationItem::new(
                id::<EntryId>(base + 2)?,
                Actor::Assistant,
                vec![
                    ContentBlock::Reasoning(ReasoningBlock::new(
                        ReasoningVisibility::Summary,
                        Some("portable reasoning summary".to_owned()),
                        None,
                    )?),
                    native_output(protocol, family, model, "private-reasoning-marker")?,
                ],
                Provenance::ModelOutput,
            )?);
        }
        items.extend([
            ConversationItem::new(
                id::<EntryId>(base + 3)?,
                Actor::Assistant,
                vec![
                    ContentBlock::ToolCall(call.clone()),
                    native_output(protocol, family, model, "private-tool-marker")?,
                ],
                Provenance::ModelOutput,
            )?,
            ConversationItem::new(
                id::<EntryId>(base + 4)?,
                Actor::Tool,
                vec![ContentBlock::ToolResult(ToolResultBlock::new(
                    call.call_id(),
                    ToolExecutionOwner::PlakLocal,
                    ToolResultKind::Success,
                    vec![ToolResultPart::Text(TextBlock::new(
                        "portable result",
                        None,
                    )?)],
                    call.correlation().clone(),
                )?)],
                Provenance::ToolExecution,
            )?,
            user(base + 5, "continue on the rebound target")?,
        ]);
        Ok(items)
    }

    fn tool(name: &str, custom: bool) -> Result<ConfiguredTool, Box<dyn std::error::Error>> {
        tool_with_concurrency(name, custom, ToolConcurrency::Parallel)
    }

    fn tool_with_concurrency(
        name: &str,
        custom: bool,
        concurrency: ToolConcurrency,
    ) -> Result<ConfiguredTool, Box<dyn std::error::Error>> {
        Ok(ConfiguredTool::new(
            ToolDefinition::new(
                ToolName::new(name)?,
                "1.0.0",
                format!("{name} description"),
                if custom {
                    ToolInputContract::utf8(65_536)?
                } else {
                    ToolInputContract::structured(json!({
                        "$schema": "https://json-schema.org/draft/2020-12/schema",
                        "type": "object",
                        "properties": {"q": {"type": "string"}},
                        "required": ["q"],
                        "additionalProperties": false
                    }))?
                },
                ToolOutputContract::portable_model_content(),
                concurrency,
                ToolEffect::ReadOnly,
            )?,
            ToolExecutionDisposition::Local,
            ToolCallerDisposition::DirectOnly,
        ))
    }

    #[test]
    fn provider_parallel_permission_is_independent_of_local_tool_scheduling() -> TestResult {
        let (protocol, family, model) = target_values()?;
        let request = crate::test_support::model_request_builder(
            ConversationView::new(vec![user(10, "Use the appropriate tool.")?]),
            metadata()?,
        )
        .tools(vec![
            tool_with_concurrency("parallel_lookup", false, ToolConcurrency::Parallel)?,
            tool_with_concurrency("serial_write", false, ToolConcurrency::Serial)?,
        ])
        .build()?;

        for (admitted, expected) in [(true, true), (false, false)] {
            let lowered = lower_responses_request(
                &request,
                ResponsesRequestConfig::new(
                    &protocol,
                    Some(&family),
                    &model,
                    ResponsesRequestPolicy::openai_platform(
                        ResponsesStoragePolicy::ProviderStored,
                        false,
                        admitted,
                    ),
                    None,
                ),
            )?;
            assert_eq!(
                lowered.body().get("parallel_tool_calls"),
                Some(&Value::Bool(expected))
            );
        }
        Ok(())
    }

    #[test]
    fn openrouter_tools_emit_documented_null_strict_and_serial_authority() -> TestResult {
        let (protocol, family, model) = target_values()?;
        let request = crate::test_support::model_request_builder(
            ConversationView::new(vec![user(11, "Use one tool.")?]),
            metadata()?,
        )
        .tools(vec![tool_with_concurrency(
            "lookup",
            false,
            ToolConcurrency::Serial,
        )?])
        .limits(GenerationLimits::new(
            Some(NonZeroU32::new(64).expect("nonzero")),
            Vec::new(),
        )?)
        .build()?;
        let policy = ResponsesRequestPolicy::openrouter_stateless();
        validate_tools(
            request.tools(),
            request.tool_choice(),
            ParallelToolDisposition::Disabled,
            policy,
        )?;
        let lowered = lower_responses_request(
            &request,
            ResponsesRequestConfig::new(&protocol, Some(&family), &model, policy, None),
        )?;
        assert_eq!(lowered.body()["tools"][0]["strict"], Value::Null);
        assert_eq!(lowered.body()["tool_choice"], "auto");
        assert!(lowered.body().get("parallel_tool_calls").is_none());
        assert!(lowered.body().get("include").is_none());
        Ok(())
    }

    fn target_values() -> Result<(ProtocolId, ModelFamilyId, ModelId), Box<dyn std::error::Error>> {
        Ok((
            ProtocolId::new("openai.responses")?,
            ModelFamilyId::new("gpt-family")?,
            ModelId::new("gpt-test")?,
        ))
    }

    #[test]
    fn mixed_v2_projection_lowers_explicit_function_and_custom_presentations() -> TestResult {
        let base = crate::test_support::model_request_builder(
            ConversationView::new(vec![user(90, "run it")?]),
            metadata()?,
        )
        .tools(vec![
            tool("run_process", true)?,
            tool("poll_process", true)?,
        ])
        .tool_choice(ToolChoice::Specific(ToolName::new("run_process")?))
        .resolved_harness_plan_identity(harness_plan_identity()?)
        .build()?;

        let codex_protocol = ProtocolId::new("openai.codex.responses")?;
        let codex_family = ModelFamilyId::new("responses-family")?;
        let codex_model = ModelId::new("gpt-5.3-codex-spark")?;
        let schema = plakous_model::PortableToolSchemaV2::new(json!({
            "$schema":"https://json-schema.org/draft/2020-12/schema", "type":"object",
            "properties":{"cmd":{"type":"string"},"yield_time_ms":{"type":"integer","minimum":0}},
            "required":["cmd"], "additionalProperties":false
        }))?;
        let output = plakous_model::PortableToolSchemaV2::new(json!({
            "$schema":"https://json-schema.org/draft/2020-12/schema", "type":"object",
            "properties":{"output":{"type":"string"}}, "required":["output"], "additionalProperties":false
        }))?;
        let source = Digest::sha256(DigestDomain::ToolDefinition, 2, &[b"test-projection"])?;
        let target = plakous_model::ToolProjectionTargetV2::new(
            plakous_model::IntegrationClass::Standard,
            codex_protocol.as_str(),
            "openai.codex_subscription",
            "openai.codex.compat.2026-07-31.r1",
            codex_family.as_str(),
            codex_model.as_str(),
            codex_model.as_str(),
            base.effective_provider_configuration_id(),
            1,
        )?;
        let projection = CompiledToolProjectionV2::new(
            target,
            source,
            source,
            vec![
                ProjectedToolDefinitionV2::new_custom(
                    ToolName::new("run_process")?,
                    vec![ToolName::new("run_process")?],
                    ToolName::new("exec")?,
                    "execute bounded JavaScript",
                    plakous_model::Utf8TextContractV1::new(65_536)?,
                    plakous_model::ProjectedCustomToolFormatV2::lark(
                        "start: source\nsource: /(?s).+/",
                    )?,
                    output.clone(),
                    source,
                )?,
                ProjectedToolDefinitionV2::new(
                    ToolName::new("poll_process")?,
                    vec![ToolName::new("poll_process")?],
                    ToolName::new("wait")?,
                    "wait for an evaluation",
                    schema,
                    output,
                    source,
                )?,
            ],
        )?;
        let prefix = base.stable_prefix().with_tool_projection_v2(projection)?;
        let request = ModelRequest::from_parts(prefix, base.input().clone(), base.metadata())?;
        let codex = lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &codex_protocol,
                Some(&codex_family),
                &codex_model,
                ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
                None,
            ),
        )?;
        let codex_tools = codex.body()["tools"].as_array().ok_or("codex tools")?;
        assert_eq!(codex_tools.len(), 2);
        assert_eq!(codex_tools[0]["name"], "exec");
        assert_eq!(codex_tools[0]["type"], "custom");
        assert_eq!(codex_tools[0]["format"]["type"], "grammar");
        assert_eq!(codex_tools[0]["format"]["syntax"], "lark");
        assert_eq!(
            codex_tools[0]["format"]["definition"],
            "start: source\nsource: /(?s).+/"
        );
        assert!(codex_tools[0].get("parameters").is_none());
        assert_eq!(codex_tools[1]["name"], "wait");
        assert_eq!(codex_tools[1]["type"], "function");
        assert_eq!(
            codex.custom_tools(),
            &BTreeSet::from([ToolName::new("exec")?])
        );
        assert_eq!(
            codex.body()["tool_choice"],
            json!({"type":"custom", "name":"exec"})
        );
        assert_eq!(
            codex_tools[1]["parameters"]["properties"]["yield_time_ms"]["type"],
            "integer"
        );
        assert_eq!(
            canonical_local_tools(&request),
            vec![
                ResponsesCanonicalTool::local_custom(ToolName::new("exec")?),
                ResponsesCanonicalTool::local_function(ToolName::new("wait")?),
            ]
        );
        assert_eq!(codex.body()["parallel_tool_calls"], false);
        let drifted_model = ModelId::new("gpt-5.3-codex-neighbor")?;
        assert!(matches!(
            lower_responses_request(
                &request,
                ResponsesRequestConfig::new(
                    &codex_protocol,
                    Some(&codex_family),
                    &drifted_model,
                    ResponsesRequestPolicy::openai_codex(
                        false,
                        false,
                        "reasoning.encrypted_content",
                    ),
                    None,
                ),
            ),
            Err(ResponsesRequestError::Option(
                ResponsesRequestOptionError::ProcessProjectionUnavailable
            ))
        ));
        Ok(())
    }

    #[test]
    fn ordinary_tool_is_not_projected_by_provider_or_model_name() -> TestResult {
        let request = crate::test_support::model_request_builder(
            ConversationView::new(vec![user(91, "run it")?]),
            metadata()?,
        )
        .tools(vec![tool("run_process", false)?])
        .resolved_harness_plan_identity(harness_plan_identity()?)
        .build()?;
        let protocol = ProtocolId::new("openai.codex.responses")?;
        let family = ModelFamilyId::new("responses-family")?;
        let model = ModelId::new("gpt-codex-neighbor")?;
        let lowered = lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
                None,
            ),
        )?;
        assert_eq!(lowered.body()["tools"][0]["name"], "run_process");
        Ok(())
    }

    fn harness_plan_identity() -> Result<Digest, plakous_model::DigestError> {
        Digest::sha256(
            DigestDomain::ResolvedHarnessPlan,
            1,
            &[b"responses-test-tool-plan"],
        )
    }

    fn descriptor(route: &str) -> Result<ResolvedModelDescriptor, Box<dyn std::error::Error>> {
        let (protocol, family, default_model) = target_values()?;
        let model = if route == "responses-ptc-test-route" {
            ModelId::new("gpt-5.6-sol")?
        } else {
            default_model
        };
        let provenance = FactProvenance::new(
            FactSource::TestFixture,
            Timestamp::new(1_700_000_000, 0)?,
            None,
        );
        let mut capabilities = BTreeMap::new();
        for capability in [
            Capability::Streaming,
            Capability::Tools,
            Capability::Continuation,
            Capability::Caching,
        ] {
            capabilities.insert(
                capability,
                FeatureSupport::new(
                    Support::Native,
                    CapabilityConstraints::default(),
                    provenance,
                ),
            );
        }
        if route == "responses-ptc-test-route" {
            capabilities.insert(
                Capability::HostedProgrammaticToolCalling,
                FeatureSupport::new(
                    Support::Native,
                    CapabilityConstraints::default(),
                    provenance,
                ),
            );
        }
        Ok(ResolvedModelDescriptor::new(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            protocol,
            RouteId::new(route)?,
            model,
            None,
            Some(family),
            ModelAdapterProfileId::new("responses-test-adapter")?,
            CapabilitySet::new(capabilities, BTreeMap::new()),
        ))
    }

    fn selected_responses_configuration()
    -> Result<SelectedProviderConfiguration, Box<dyn std::error::Error>> {
        let integration_profile = ModelAdapterProfileId::new("responses-test-adapter")?;
        let integration_revision = ProviderProfileRevisionId::new("responses-test-v1")?;
        Ok(SelectedProviderConfiguration::new(
            ProviderRouteSelection::new(
                ProtocolId::new("openai.responses")?,
                RouteId::new("responses-test-route")?,
                integration_profile.clone(),
                integration_revision.clone(),
                ModelAdapterProfileId::new("responses-test-auth")?,
                ProviderProfileRevisionId::new("responses-test-auth-v1")?,
                ProviderAuthorityClassId::new("test-subscription")?,
                CredentialSelectionId::new("test-account")?,
                ModelId::new("gpt-test")?,
            ),
            ProviderBehaviorConfiguration {
                processing_tier: ProcessingTierDisposition::Standard,
                tool_mode: plakous_model::ToolPresentationMode::Direct,
                reasoning_effort: ReasoningEffortDisposition::Disabled,
                reasoning_summary: ReasoningSummaryDisposition::Omit,
                reasoning_include: ReasoningIncludeDisposition::EncryptedContent,
                replay: ReplayDisposition::StrictEncrypted,
                cache: CachePolicy::Disabled,
                tools: Vec::new(),
                hosted_tools: Vec::new(),
                tool_choice: ToolChoice::Auto,
                parallel_tools: ParallelToolDisposition::Disabled,
                output: OutputPolicy::default(),
                output_bound: OutputBoundDisposition::Enforced(NonZeroU32::MIN),
                storage: ProviderStorageDisposition::DisabledOnWire,
                retention: RetentionDisposition::RouteContract {
                    profile: integration_profile,
                    revision: integration_revision,
                },
                response_state: ResponseStateDisposition::Stateless,
                context_evidence_requirement: ContextAdmissionRequirement::AnyBounded,
                local_durability: LocalDurabilityDisposition::Ephemeral,
            },
        )?)
    }

    fn configuration_authority(
        descriptor: &ResolvedModelDescriptor,
    ) -> Result<ResolvedProviderAuthorityBinding, Box<dyn std::error::Error>> {
        let runtime = RuntimeModelAuthority::new(
            descriptor,
            Digest::sha256(DigestDomain::ModelBinding, 1, &[b"responses-test-account"])?,
            Digest::sha256(
                DigestDomain::ModelBinding,
                1,
                &[b"responses-test-credentials"],
            )?,
            1,
            Timestamp::new(1_700_000_000, 0)?,
            Some(Timestamp::new(1_700_000_100, 0)?),
        )?;
        Ok(ResolvedProviderAuthorityBinding::new(
            descriptor,
            &runtime,
            ProviderSemanticEvidence::new(1, b"responses-test-semantic-v1".to_vec())?,
        )?)
    }

    fn custom_descriptor(
        route: &str,
    ) -> Result<ResolvedModelDescriptor, Box<dyn std::error::Error>> {
        let standard = descriptor(route)?;
        Ok(ResolvedModelDescriptor::new_custom(
            standard.protocol().clone(),
            standard.route().clone(),
            standard.requested_model().clone(),
            standard.reported_model().cloned(),
            standard.model_family().cloned(),
            standard.profile().clone(),
            standard.capabilities().clone(),
        ))
    }

    #[test]
    fn compilation_retains_exact_standard_and_custom_implementation_identity() -> TestResult {
        let selected = selected_responses_configuration()?;
        let standard = descriptor("responses-test-route")?;
        let expected_standard = ModelImplementationIdentity::release_owned(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            &standard,
            1,
        )?;
        let compiled_standard = compile_provider_configuration(
            &selected,
            standard.clone(),
            configuration_authority(&standard)?,
            ResponsesRequestPolicy::supergrok_stateless(),
        )?;
        assert_eq!(
            compiled_standard.effective().model_implementation(),
            &expected_standard
        );

        let custom = custom_descriptor("responses-test-route")?;
        let custom_implementation = ModelImplementationIdentity::custom(
            NamespaceId::new("example.responses.dynamic")?,
            7,
            NamespaceId::new("example.integration")?,
        )?;
        let compiled_custom = compile_provider_configuration_with_implementation(
            &selected,
            custom.clone(),
            custom_implementation.clone(),
            configuration_authority(&custom)?,
            ResponsesRequestPolicy::supergrok_stateless(),
        )?;
        assert_eq!(
            compiled_custom.effective().model_implementation(),
            &custom_implementation
        );
        let gateway = ResponsesRouterPolicy::gateway_managed(None, None)?;
        let routed_custom = compile_provider_configuration_with_implementation_and_router_policy(
            &selected,
            custom.clone(),
            custom_implementation.clone(),
            configuration_authority(&custom)?,
            ResponsesRequestPolicy::supergrok_stateless(),
            Some(&gateway),
        )?;
        assert_ne!(
            compiled_custom.effective_identity(),
            routed_custom.effective_identity()
        );
        let direct_policy: Value = serde_json::from_slice(
            compiled_custom
                .effective()
                .family_policy()
                .canonical_bytes(),
        )?;
        let routed_policy: Value =
            serde_json::from_slice(routed_custom.effective().family_policy().canonical_bytes())?;
        assert!(direct_policy.get("router_policy").is_none());
        assert_eq!(
            routed_policy["router_policy"],
            serde_json::to_value(gateway.execution_identity()?)?
        );

        let error = compile_provider_configuration_with_implementation(
            &selected,
            custom.clone(),
            expected_standard,
            configuration_authority(&custom)?,
            ResponsesRequestPolicy::supergrok_stateless(),
        )
        .expect_err("foreign descriptor/implementation class must fail");
        assert_eq!(
            error,
            ResponsesProviderConfigurationError::ProviderConfiguration(
                ProviderConfigurationError::ForeignIntegrationClass
            )
        );
        Ok(())
    }

    #[test]
    fn selected_policy_mismatch_fails_on_one_field() -> TestResult {
        let mut selected = selected_responses_configuration()?;
        let mut behavior = selected.behavior().clone();
        behavior.replay = ReplayDisposition::Disabled;
        selected = SelectedProviderConfiguration::new(selected.route().clone(), behavior)?;
        let descriptor = descriptor("responses-test-route")?;
        let error = compile_provider_configuration(
            &selected,
            descriptor.clone(),
            configuration_authority(&descriptor)?,
            ResponsesRequestPolicy::supergrok_stateless(),
        )
        .expect_err("one changed replay field must fail");
        assert_eq!(
            error,
            ResponsesProviderConfigurationError::PolicyMismatch("replay")
        );
        Ok(())
    }

    #[test]
    fn canonical_custom_tool_is_compilable_but_requires_request_projection() -> TestResult {
        let mut selected = selected_responses_configuration()?;
        let mut behavior = selected.behavior().clone();
        behavior.tools = vec![tool("canonical_process_start", true)?];
        selected = SelectedProviderConfiguration::new(selected.route().clone(), behavior)?;
        let descriptor = descriptor("responses-test-route")?;
        compile_provider_configuration(
            &selected,
            descriptor.clone(),
            configuration_authority(&descriptor)?,
            ResponsesRequestPolicy::supergrok_stateless(),
        )?;

        let request = crate::test_support::model_request_builder(
            ConversationView::new(vec![user(92, "run it")?]),
            metadata()?,
        )
        .tools(vec![tool("canonical_process_start", true)?])
        .build()?;
        let (protocol, family, model) = target_values()?;
        assert!(matches!(
            lower_responses_request(
                &request,
                ResponsesRequestConfig::new(
                    &protocol,
                    Some(&family),
                    &model,
                    ResponsesRequestPolicy::supergrok_stateless(),
                    None,
                ),
            ),
            Err(ResponsesRequestError::Option(
                ResponsesRequestOptionError::CustomTools
            ))
        ));
        Ok(())
    }

    #[test]
    fn family_policy_envelope_is_deterministic_canonical_and_roundtrips() -> TestResult {
        let selected = selected_responses_configuration()?;
        let descriptor = descriptor("responses-test-route")?;
        let policy = ResponsesRequestPolicy::supergrok_stateless();
        let first = compile_provider_configuration(
            &selected,
            descriptor.clone(),
            configuration_authority(&descriptor)?,
            policy,
        )?;
        let second = compile_provider_configuration(
            &selected,
            descriptor.clone(),
            configuration_authority(&descriptor)?,
            policy,
        )?;
        assert_eq!(first, second);
        let canonical: Value =
            serde_json::from_slice(first.effective().family_policy().canonical_bytes())?;
        assert_eq!(canonical["schema_revision"], 1);
        assert_eq!(canonical["compiler_revision"], 1);
        assert_eq!(
            canonical["execution_revision"],
            RESPONSES_REQUEST_POLICY_EXECUTION_REVISION
        );
        assert_eq!(canonical["store_field"], "emit");
        assert_eq!(canonical["reasoning_summary"], "concise");
        assert_eq!(canonical["reasoning_include"], "always");
        assert_eq!(canonical["replay"], "strict");
        let encoded = serde_json::to_vec(&first)?;
        let decoded: CompiledProviderConfiguration = serde_json::from_slice(&encoded)?;
        assert_eq!(decoded, first);
        validate_supported_family_policy(
            descriptor
                .model_family()
                .ok_or("descriptor omitted family")?,
            decoded.effective().family_policy(),
        )?;
        assert_eq!(
            validate_supported_family_policy(
                &ModelFamilyId::new("foreign-responses-family")?,
                decoded.effective().family_policy(),
            ),
            Err(ResponsesProviderConfigurationError::ProviderConfiguration(
                ProviderConfigurationError::ForeignTarget,
            ))
        );
        Ok(())
    }

    #[test]
    fn family_policy_support_rejects_each_unknown_revision() -> TestResult {
        let selected = selected_responses_configuration()?;
        let descriptor = descriptor("responses-test-route")?;
        let compiled = compile_provider_configuration(
            &selected,
            descriptor.clone(),
            configuration_authority(&descriptor)?,
            ResponsesRequestPolicy::supergrok_stateless(),
        )?;
        let current = compiled.effective().family_policy();
        let unsupported = |schema, compiler, dialect, bytes| {
            FamilyPolicyEnvelope::new(current.family().clone(), schema, compiler, dialect, bytes)
        };
        for envelope in [
            unsupported(
                RESPONSES_FAMILY_POLICY_SCHEMA_REVISION + 1,
                RESPONSES_FAMILY_POLICY_COMPILER_REVISION,
                RESPONSES_REQUEST_POLICY_EXECUTION_REVISION,
                current.canonical_bytes().to_vec(),
            )?,
            unsupported(
                RESPONSES_FAMILY_POLICY_SCHEMA_REVISION,
                RESPONSES_FAMILY_POLICY_COMPILER_REVISION + 1,
                RESPONSES_REQUEST_POLICY_EXECUTION_REVISION,
                current.canonical_bytes().to_vec(),
            )?,
            unsupported(
                RESPONSES_FAMILY_POLICY_SCHEMA_REVISION,
                RESPONSES_FAMILY_POLICY_COMPILER_REVISION,
                RESPONSES_REQUEST_POLICY_EXECUTION_REVISION + 1,
                current.canonical_bytes().to_vec(),
            )?,
        ] {
            assert_eq!(
                validate_supported_family_policy(current.family(), &envelope),
                Err(ResponsesProviderConfigurationError::ProviderConfiguration(
                    ProviderConfigurationError::UnsupportedRevision,
                ))
            );
        }

        for field in [
            "schema_revision",
            "compiler_revision",
            "execution_revision",
            "artifact_schema_revision",
            "artifact_codec_revision",
        ] {
            let mut canonical: Value = serde_json::from_slice(current.canonical_bytes())?;
            canonical[field] = Value::from(99);
            let envelope = unsupported(
                RESPONSES_FAMILY_POLICY_SCHEMA_REVISION,
                RESPONSES_FAMILY_POLICY_COMPILER_REVISION,
                RESPONSES_REQUEST_POLICY_EXECUTION_REVISION,
                serde_json::to_vec(&canonical)?,
            )?;
            assert_eq!(
                validate_supported_family_policy(current.family(), &envelope),
                Err(ResponsesProviderConfigurationError::ProviderConfiguration(
                    ProviderConfigurationError::UnsupportedRevision,
                ))
            );

            let mut defaulted: Value = serde_json::from_slice(current.canonical_bytes())?;
            defaulted[field] = Value::from(0);
            let envelope = unsupported(
                RESPONSES_FAMILY_POLICY_SCHEMA_REVISION,
                RESPONSES_FAMILY_POLICY_COMPILER_REVISION,
                RESPONSES_REQUEST_POLICY_EXECUTION_REVISION,
                serde_json::to_vec(&defaulted)?,
            )?;
            assert!(matches!(
                validate_supported_family_policy(current.family(), &envelope),
                Err(ResponsesProviderConfigurationError::ProviderConfiguration(
                    ProviderConfigurationError::UnsupportedRevision
                ))
            ));

            let mut missing: Value = serde_json::from_slice(current.canonical_bytes())?;
            missing
                .as_object_mut()
                .ok_or("canonical policy fixture must be an object")?
                .remove(field);
            let envelope = unsupported(
                RESPONSES_FAMILY_POLICY_SCHEMA_REVISION,
                RESPONSES_FAMILY_POLICY_COMPILER_REVISION,
                RESPONSES_REQUEST_POLICY_EXECUTION_REVISION,
                serde_json::to_vec(&missing)?,
            )?;
            assert!(matches!(
                validate_supported_family_policy(current.family(), &envelope),
                Err(ResponsesProviderConfigurationError::ProviderConfiguration(
                    ProviderConfigurationError::UnsupportedRevision
                ))
            ));
        }
        Ok(())
    }

    #[test]
    fn platform_function_strictness_changes_execution_authority() -> TestResult {
        for policy in [
            ResponsesRequestPolicy::openai_platform(
                ResponsesStoragePolicy::ProviderStored,
                false,
                false,
            ),
            ResponsesRequestPolicy::openai_platform_stateless(),
        ] {
            assert_eq!(policy.function_strict, FunctionStrictPolicy::IncludeFalse);
            assert_eq!(
                policy.structured_output,
                StructuredOutputPolicy::StrictJsonSchema
            );
            let mut strict = policy;
            strict.function_strict = FunctionStrictPolicy::IncludeTrue;
            assert_ne!(policy.execution_identity()?, strict.execution_identity()?);
        }
        Ok(())
    }

    #[test]
    fn policy_and_envelope_execution_identity_are_equal() -> TestResult {
        let selected = selected_responses_configuration()?;
        let descriptor = descriptor("responses-test-route")?;
        let policy = ResponsesRequestPolicy::supergrok_stateless();
        let compiled = compile_provider_configuration(
            &selected,
            descriptor.clone(),
            configuration_authority(&descriptor)?,
            policy,
        )?;
        let canonical: Value =
            serde_json::from_slice(compiled.effective().family_policy().canonical_bytes())?;
        assert_eq!(
            canonical["execution_identity"],
            serde_json::to_value(policy.execution_identity()?)?
        );
        Ok(())
    }

    fn operation_binding(
        request: &ModelRequest,
        route: &str,
    ) -> Result<ResponsesArtifactBinding, Box<dyn std::error::Error>> {
        let descriptor = descriptor(route)?;
        let plan = descriptor.validate_request(request.clone(), ValidationMode::Strict)?;
        Ok(ResponsesArtifactBinding::from_admitted_operation(
            &descriptor,
            &plan,
            request,
            Digest::sha256(DigestDomain::ModelBinding, 1, &[route.as_bytes()])?,
            ResponsesSemanticRevisions::new(1, 1, 1)?,
            None,
        )?)
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the fixture seals every independent artifact authority fact"
    )]
    fn output_artifact(
        producing_request: &ModelRequest,
        covered_request: &ModelRequest,
        route: &str,
        replay: Vec<ReplayEnvelope>,
        storage: ResponsesStorageMode,
        response_id: &str,
        expires_at: Option<Timestamp>,
        now: Timestamp,
        tag: &[u8],
    ) -> Result<ResponsesArtifact, Box<dyn std::error::Error>> {
        let descriptor = descriptor(route)?;
        let plan =
            descriptor.validate_request(producing_request.clone(), ValidationMode::Strict)?;
        let output_count = u32::try_from(replay.len())?;
        let producing_count = producing_request.input().items().len();
        let covered_count = producing_count
            .checked_add(replay.len())
            .ok_or_else(|| std::io::Error::other("artifact coverage overflow"))?;
        let covered_projection = covered_request
            .input()
            .canonical_item_prefix_identity(covered_count)?
            .identity();
        let physical_request = Digest::sha256(
            DigestDomain::ProviderOperation,
            1,
            &[b"request-host-fixture-physical", tag],
        )?;
        let terminal_response = Digest::sha256(
            DigestDomain::ProviderOperation,
            1,
            &[b"request-host-fixture-terminal", tag],
        )?;
        let production = ResponsesArtifactProduction::from_admitted_response(
            producing_request,
            &plan,
            physical_request,
            terminal_response,
            output_count,
            covered_projection,
        )?;
        let first = replay
            .first()
            .ok_or_else(|| std::io::Error::other("fixture requires output replay"))?;
        let replay_contract = ResponsesReplayContract::new(ResponsesArtifactReplayKind::new(
            first.kind().clone(),
            first.version(),
        )?);
        let replay = replay
            .into_iter()
            .enumerate()
            .map(|(index, envelope)| {
                Ok(ResponsesReplayItem::new(
                    ResponsesReplayPurpose::OutputItem,
                    u32::try_from(index)?,
                    envelope,
                ))
            })
            .collect::<Result<Vec<_>, std::num::TryFromIntError>>()?;
        let continuation = if storage == ResponsesStorageMode::ProviderStored {
            Some(ResponsesContinuation::new(
                response_id,
                production.covered_projection_items(),
                production.covered_projection(),
                production.terminal_response(),
            )?)
        } else {
            None
        };
        Ok(ResponsesArtifact::new(
            operation_binding(producing_request, route)?,
            production,
            replay_contract,
            replay,
            continuation,
            None,
            storage,
            expires_at,
            now,
        )?)
    }

    fn operation_authority(
        request: &ModelRequest,
        route: &str,
        storage: ResponsesStoragePolicy,
    ) -> Result<ResponsesRequestAuthority, Box<dyn std::error::Error>> {
        let descriptor = descriptor(route)?;
        let plan = descriptor.validate_request(request.clone(), ValidationMode::Strict)?;
        Ok(ResponsesRequestAuthority::from_admitted_operation(
            &descriptor,
            &plan,
            request,
            Digest::sha256(DigestDomain::ModelBinding, 1, &[route.as_bytes()])?,
            ResponsesSemanticRevisions::new(1, 1, 1)?,
            None,
            storage,
        )?)
    }

    fn active_artifact(
        artifact: &ResponsesArtifact,
    ) -> Result<ActiveFamilyArtifact, Box<dyn std::error::Error>> {
        Ok(ActiveFamilyArtifact::new(
            "responses",
            RESPONSES_ARTIFACT_SCHEMA_REVISION,
            RESPONSES_ARTIFACT_CODEC_REVISION,
            artifact.binding().identity(),
            ResponsesArtifactCodec::encode(artifact)?,
        )?)
    }

    fn active_replay_fixture(
        storage: ResponsesStorageMode,
        expires_at: Option<Timestamp>,
    ) -> Result<(ModelRequest, ActiveFamilyArtifact), Box<dyn std::error::Error>> {
        active_replay_fixture_with_encrypted(
            storage,
            expires_at,
            Some("completed"),
            Some("fixture-encrypted"),
        )
    }

    fn active_replay_fixture_with_encrypted(
        storage: ResponsesStorageMode,
        expires_at: Option<Timestamp>,
        status: Option<&str>,
        encrypted_content: Option<&str>,
    ) -> Result<(ModelRequest, ActiveFamilyArtifact), Box<dyn std::error::Error>> {
        let (protocol, family, model) = target_values()?;
        let mut native = json!({
            "id": "fixture-reasoning",
            "type": "reasoning",
            "summary": []
        });
        if let Some(status) = status {
            native["status"] = Value::String(status.to_owned());
        }
        if let Some(encrypted_content) = encrypted_content {
            native["encrypted_content"] = Value::String(encrypted_content.to_owned());
        }
        let replay = ReplayEnvelope::new(
            protocol,
            Some(family),
            ReplayScope::ExactModel(model),
            ReplayKindId::new("openai.output_item")?,
            2,
            DataClassification::Sensitive,
            RawValue::from_string(native.to_string())?,
        )?;
        let prefix = HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![user(500, "fixture first")?],
        )?;
        let producing_request = crate::test_support::model_request_builder(
            ConversationView::from_shared(
                CanonicalHistory::new(vec![prefix.clone()])?,
                WorkingProjection::default(),
            ),
            metadata()?,
        )
        .resolved_harness_plan_identity(harness_plan_identity()?)
        .build()?;
        let output = HistorySegment::new(
            HistorySegmentKind::ReasoningReplay,
            vec![ConversationItem::new(
                id::<EntryId>(501)?,
                Actor::Assistant,
                vec![ContentBlock::Opaque(OpaqueBlock::new(
                    replay.clone(),
                    OpaqueFallback::CompatibleReplay,
                )?)],
                Provenance::ModelOutput,
            )?],
        )?;
        let request = crate::test_support::model_request_builder(
            ConversationView::from_shared(
                CanonicalHistory::new(vec![
                    prefix,
                    output,
                    HistorySegment::new(
                        HistorySegmentKind::RecentIntent,
                        vec![user(502, "fixture next")?],
                    )?,
                ])?,
                WorkingProjection::default(),
            ),
            metadata()?,
        )
        .resolved_harness_plan_identity(harness_plan_identity()?)
        .build()?;
        let artifact = output_artifact(
            &producing_request,
            &request,
            "responses-test-route",
            vec![replay],
            storage,
            "fixture-response",
            expires_at,
            Timestamp::new(1_700_000_000, 0)?,
            b"active-replay-fixture",
        )?;
        Ok((request, active_artifact(&artifact)?))
    }

    #[test]
    fn strict_lowering_rejects_active_reasoning_without_encrypted_content() -> TestResult {
        let (request, active) = active_replay_fixture_with_encrypted(
            ResponsesStorageMode::Stateless,
            None,
            Some("completed"),
            None,
        )?;
        let (protocol, family, model) = target_values()?;
        let bridge = ResponsesArtifactReplayBridge::from_active_artifact(
            &active,
            &request,
            operation_authority(
                &request,
                "responses-test-route",
                ResponsesStoragePolicy::Stateless,
            )?,
            ResponsesReplayKind::new("openai.output_item", 2),
            None,
            None,
            Timestamp::new(1_700_000_000, 0)?,
        )?;
        assert!(matches!(
            lower_responses_request(
                &request,
                ResponsesRequestConfig::new(
                    &protocol,
                    Some(&family),
                    &model,
                    ResponsesRequestPolicy::supergrok_stateless(),
                    Some(bridge.context()),
                ),
            ),
            Err(ResponsesRequestError::Replay(
                ResponsesReplayError::StrictEncryptedReasoning
            ))
        ));
        Ok(())
    }

    #[test]
    fn strict_lowering_accepts_codex_reasoning_without_status() -> TestResult {
        let (request, active) = active_replay_fixture_with_encrypted(
            ResponsesStorageMode::Stateless,
            None,
            None,
            Some("fixture-encrypted"),
        )?;
        let (protocol, family, model) = target_values()?;
        let bridge = ResponsesArtifactReplayBridge::from_active_artifact(
            &active,
            &request,
            operation_authority(
                &request,
                "responses-test-route",
                ResponsesStoragePolicy::Stateless,
            )?,
            ResponsesReplayKind::new("openai.output_item", 2),
            None,
            None,
            Timestamp::new(1_700_000_000, 0)?,
        )?;
        lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
                Some(bridge.context()),
            ),
        )?;
        Ok(())
    }

    #[test]
    fn active_context_replays_bound_output_and_portably_lowers_compatible_unbound_output()
    -> TestResult {
        let (base, active) = active_replay_fixture(ResponsesStorageMode::Stateless, None)?;
        let (protocol, family, model) = target_values()?;
        let mut items = base.input().items().to_vec();
        items.insert(
            2,
            ConversationItem::new(
                id::<EntryId>(503)?,
                Actor::Assistant,
                vec![
                    ContentBlock::Text(TextBlock::new(
                        "portable compatible-but-unbound answer",
                        Some(TextPhase::Final),
                    )?),
                    native_output(
                        &protocol,
                        &family,
                        &model,
                        "compatible-unbound-native-marker",
                    )?,
                ],
                Provenance::ModelOutput,
            )?,
        );
        let request =
            crate::test_support::model_request_builder(ConversationView::new(items), metadata()?)
                .resolved_harness_plan_identity(harness_plan_identity()?)
                .build()?;
        let authority = operation_authority(
            &request,
            "responses-test-route",
            ResponsesStoragePolicy::Stateless,
        )?;
        let bridge = ResponsesArtifactReplayBridge::from_active_artifact(
            &active,
            &request,
            authority,
            ResponsesReplayKind::new("openai.output_item", 2),
            None,
            None,
            Timestamp::new(1_700_000_000, 0)?,
        )?;
        let lowered = lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
                Some(bridge.context()),
            ),
        )?;
        let wire = lowered.body().to_string();
        assert!(wire.contains("fixture-reasoning"));
        assert!(wire.contains("portable compatible-but-unbound answer"));
        assert!(!wire.contains("compatible-unbound-native-marker"));
        Ok(())
    }

    #[test]
    fn nested_digest_versions_are_part_of_authority_and_prefix_identity() -> TestResult {
        let request = crate::test_support::model_request_builder(
            ConversationView::new(vec![user(9, "exact identity")?]),
            metadata()?,
        )
        .resolved_harness_plan_identity(harness_plan_identity()?)
        .build()?;
        let binding = operation_binding(&request, "test.responses")?;
        let raw = [0x5a; 32];
        let request_v1 = Digest::from_sha256_bytes(DigestDomain::HistoryProjection, 1, raw)?;
        let request_v2 = Digest::from_sha256_bytes(DigestDomain::HistoryProjection, 2, raw)?;
        let plan = harness_plan_identity()?;
        assert_ne!(
            operation_authority_digest(
                &binding,
                ResponsesStoragePolicy::Stateless,
                request_v1,
                plan,
            )?,
            operation_authority_digest(
                &binding,
                ResponsesStoragePolicy::Stateless,
                request_v2,
                plan,
            )?
        );

        let authority_v1 = Digest::from_sha256_bytes(DigestDomain::ModelBinding, 1, raw)?;
        let authority_v2 = Digest::from_sha256_bytes(DigestDomain::ModelBinding, 2, raw)?;
        assert_ne!(
            prefix_binding_digest(authority_v1, request.input().identity())?,
            prefix_binding_digest(authority_v2, request.input().identity())?
        );
        Ok(())
    }

    #[test]
    fn platform_golden_preserves_ordered_request_shape() -> TestResult {
        let (protocol, family, model) = target_values()?;
        let request = crate::test_support::model_request_builder(
            ConversationView::new(vec![user(10, "Find the answer.")?]),
            metadata()?,
        )
        .instructions(vec![
            InstructionBlock::new(InstructionSource::Developer, "Be precise.")?,
            InstructionBlock::new(InstructionSource::Harness, "Use tools when useful.")?,
        ])
        .tools(vec![tool("lookup", false)?])
        .limits(GenerationLimits::new(
            Some(NonZeroU32::new(512).expect("nonzero")),
            Vec::new(),
        )?)
        .build()?;
        let lowered = lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_platform(
                    ResponsesStoragePolicy::ProviderStored,
                    false,
                    true,
                ),
                None,
            ),
        )?;
        assert_eq!(
            lowered.body(),
            &json!({
                "model": "gpt-test",
                "input": [{
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "Find the answer."}]
                }],
                "stream": true,
                "store": true,
                "instructions": "Be precise.\n\nUse tools when useful.",
                "tools": [{
                    "type": "function",
                    "name": "lookup",
                    "description": "lookup description",
                    // The durable V2 document carries the `$schema` dialect URI;
                    // the wire projection strips it (`wire_document`), so the
                    // lowered tool object never spends those bytes.
                    "parameters": {
                        "type": "object",
                        "properties": {"q": {"type": "string"}},
                        "required": ["q"],
                        "additionalProperties": false
                    },
                    "strict": false
                }],
                "parallel_tool_calls": true,
                "tool_choice": "auto",
                "max_output_tokens": 512
            })
        );
        assert_eq!(lowered.replay_mode(), &ResponsesReplayMode::Full);
        assert!(!format!("{lowered:?}").contains("Find the answer"));
        Ok(())
    }

    #[test]
    fn assistant_history_uses_output_text_and_projects_commentary_without_replay() -> TestResult {
        let (protocol, family, model) = target_values()?;
        let assistant = |phase| {
            ConversationItem::new(
                id::<EntryId>(15).expect("entry"),
                Actor::Assistant,
                vec![ContentBlock::Text(
                    TextBlock::new("prior answer", phase).expect("text"),
                )],
                Provenance::ModelOutput,
            )
            .expect("assistant item")
        };
        let request = crate::test_support::model_request_builder(
            ConversationView::new(vec![assistant(Some(TextPhase::Final))]),
            metadata()?,
        )
        .build()?;
        let lowered = lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
                None,
            ),
        )?;
        assert_eq!(lowered.body()["input"][0]["role"], "assistant");
        assert_eq!(
            lowered.body()["input"][0]["content"][0]["type"],
            "output_text"
        );
        assert_eq!(
            lowered.body()["input"][0]["content"][0]["text"],
            "prior answer"
        );

        let commentary = crate::test_support::model_request_builder(
            ConversationView::new(vec![assistant(Some(TextPhase::Commentary))]),
            metadata()?,
        )
        .build()?;
        let lowered = lower_responses_request(
            &commentary,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
                None,
            ),
        )?;
        assert_eq!(lowered.body()["input"][0]["role"], "assistant");
        assert_eq!(
            lowered.body()["input"][0]["content"][0]["type"],
            "output_text"
        );
        assert_eq!(
            lowered.body()["input"][0]["content"][0]["text"],
            "prior answer"
        );

        let source_protocol = ProtocolId::new("openai.codex.responses")?;
        let source_family = ModelFamilyId::new("gpt")?;
        let source_model = ModelId::new("gpt-5.6-sol")?;
        let commentary_with_native = crate::test_support::model_request_builder(
            ConversationView::new(vec![ConversationItem::new(
                id::<EntryId>(16)?,
                Actor::Assistant,
                vec![
                    ContentBlock::Text(TextBlock::new("working", Some(TextPhase::Commentary))?),
                    native_output(
                        &source_protocol,
                        &source_family,
                        &source_model,
                        "private-commentary-marker",
                    )?,
                ],
                Provenance::ModelOutput,
            )?]),
            metadata()?,
        )
        .build()?;
        let lowered = lower_responses_request(
            &commentary_with_native,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
                None,
            ),
        )?;
        let wire = lowered.body().to_string();
        assert!(wire.contains("working"));
        assert!(!wire.contains("private-commentary-marker"));
        Ok(())
    }

    #[test]
    fn codex_history_projects_portably_to_supergrok_without_native_bytes() -> TestResult {
        let source_protocol = ProtocolId::new("openai.codex.responses")?;
        let source_family = ModelFamilyId::new("gpt")?;
        let source_model = ModelId::new("gpt-5.6")?;
        let target_protocol = ProtocolId::new("xai.supergrok.responses")?;
        let target_family = ModelFamilyId::new("grok")?;
        let target_model = ModelId::new("grok-4.5")?;
        let history =
            cross_provider_history(&source_protocol, &source_family, &source_model, 600, true)?;
        let request =
            crate::test_support::model_request_builder(ConversationView::new(history), metadata()?)
                .tools(vec![tool("lookup", false)?])
                .build()?;
        let canonical_before = serde_json::to_vec(request.input().history())?;
        let lowered = lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &target_protocol,
                Some(&target_family),
                &target_model,
                ResponsesRequestPolicy::supergrok_stateless(),
                None,
            ),
        )?;
        let wire = lowered.body().to_string();
        assert!(wire.contains("portable answer"));
        assert!(wire.contains("portable result"));
        assert!(wire.contains("call-600"));
        assert!(!wire.contains("portable reasoning summary"));
        assert!(!wire.contains("private-"));
        assert_eq!(
            canonical_before,
            serde_json::to_vec(request.input().history())?
        );
        Ok(())
    }

    #[test]
    fn grok_history_projects_portably_to_codex_without_foreign_reasoning() -> TestResult {
        let source_protocol = ProtocolId::new("xai.supergrok.responses")?;
        let source_family = ModelFamilyId::new("grok")?;
        let source_model = ModelId::new("grok-4.5")?;
        let target_protocol = ProtocolId::new("openai.codex.responses")?;
        let target_family = ModelFamilyId::new("gpt")?;
        let target_model = ModelId::new("gpt-5.6")?;
        let build = |include_reasoning| -> Result<ModelRequest, Box<dyn std::error::Error>> {
            Ok(crate::test_support::model_request_builder(
                ConversationView::new(cross_provider_history(
                    &source_protocol,
                    &source_family,
                    &source_model,
                    700,
                    include_reasoning,
                )?),
                metadata()?,
            )
            .tools(vec![tool("lookup", false)?])
            .build()?)
        };
        let config = || {
            ResponsesRequestConfig::new(
                &target_protocol,
                Some(&target_family),
                &target_model,
                ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
                None,
            )
        };
        let portable = build(true)?;
        let canonical_before = serde_json::to_vec(portable.input().history())?;
        let wire = lower_responses_request(&portable, config())?
            .body()
            .to_string();
        assert!(wire.contains("portable answer"));
        assert!(wire.contains("portable result"));
        assert!(wire.contains("call-700"));
        assert!(!wire.contains("portable reasoning summary"));
        assert!(!wire.contains("private-"));
        assert_eq!(
            canonical_before,
            serde_json::to_vec(portable.input().history())?
        );
        Ok(())
    }

    #[test]
    fn target_compatible_native_output_without_artifact_uses_portable_semantics() -> TestResult {
        let protocol = ProtocolId::new("openai.codex.responses")?;
        let family = ModelFamilyId::new("gpt")?;
        let model = ModelId::new("gpt-5.6")?;
        let request = crate::test_support::model_request_builder(
            ConversationView::new(cross_provider_history(
                &protocol, &family, &model, 750, false,
            )?),
            metadata()?,
        )
        .tools(vec![tool("lookup", false)?])
        .build()?;
        let config = || {
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
                None,
            )
        };
        let wire = lower_responses_request(&request, config())?
            .body()
            .to_string();
        assert!(wire.contains("portable answer"));
        assert!(wire.contains("portable result"));
        assert!(!wire.contains("private-"));

        let opaque_only = crate::test_support::model_request_builder(
            ConversationView::new(vec![ConversationItem::new(
                id::<EntryId>(759)?,
                Actor::Assistant,
                vec![native_output(
                    &protocol,
                    &family,
                    &model,
                    "target-compatible-opaque-only",
                )?],
                Provenance::ModelOutput,
            )?]),
            metadata()?,
        )
        .build()?;
        assert!(matches!(
            lower_responses_request(&opaque_only, config()),
            Err(ResponsesRequestError::Content(
                ResponsesRequestContentError::OpaqueOnlyWithoutCompatibleReplay
            ))
        ));
        Ok(())
    }

    #[test]
    fn incompatible_opaque_fallback_matrix_is_exact_in_both_directions() -> TestResult {
        let directions = [
            (
                ProtocolId::new("openai.codex.responses")?,
                ModelFamilyId::new("gpt")?,
                ModelId::new("gpt-5.6")?,
                ProtocolId::new("xai.supergrok.responses")?,
                ModelFamilyId::new("grok")?,
                ModelId::new("grok-4.5")?,
                ResponsesRequestPolicy::supergrok_stateless(),
            ),
            (
                ProtocolId::new("xai.supergrok.responses")?,
                ModelFamilyId::new("grok")?,
                ModelId::new("grok-4.5")?,
                ProtocolId::new("openai.codex.responses")?,
                ModelFamilyId::new("gpt")?,
                ModelId::new("gpt-5.6")?,
                ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
            ),
        ];
        for (
            direction,
            (
                source_protocol,
                source_family,
                source_model,
                target_protocol,
                target_family,
                target_model,
                policy,
            ),
        ) in directions.into_iter().enumerate()
        {
            for (fallback_index, fallback) in [
                OpaqueFallback::Omit,
                OpaqueFallback::SemanticSummary("portable opaque summary".to_owned()),
                OpaqueFallback::CompatibleReplay,
            ]
            .into_iter()
            .enumerate()
            {
                let base = 900 + u128::try_from(direction * 10 + fallback_index)?;
                let opaque_only = crate::test_support::model_request_builder(
                    ConversationView::new(vec![ConversationItem::new(
                        id::<EntryId>(base)?,
                        Actor::Assistant,
                        vec![native_output_with_fallback(
                            &source_protocol,
                            &source_family,
                            &source_model,
                            "private-fallback-marker",
                            fallback.clone(),
                        )?],
                        Provenance::ModelOutput,
                    )?]),
                    metadata()?,
                )
                .build()?;
                let config = || {
                    ResponsesRequestConfig::new(
                        &target_protocol,
                        Some(&target_family),
                        &target_model,
                        policy,
                        None,
                    )
                };
                match &fallback {
                    OpaqueFallback::CompatibleReplay => assert!(matches!(
                        lower_responses_request(&opaque_only, config()),
                        Err(ResponsesRequestError::Content(
                            ResponsesRequestContentError::OpaqueOnlyWithoutCompatibleReplay
                        ))
                    )),
                    OpaqueFallback::Omit => assert_eq!(
                        lower_responses_request(&opaque_only, config())?.body()["input"],
                        json!([])
                    ),
                    OpaqueFallback::SemanticSummary(summary) => {
                        let lowered = lower_responses_request(&opaque_only, config())?;
                        assert_eq!(lowered.body()["input"][0]["role"], "assistant");
                        assert_eq!(
                            lowered.body()["input"][0]["content"][0],
                            json!({"type":"output_text","text":summary})
                        );
                    }
                    _ => unreachable!("closed test fallback"),
                }

                let mixed = crate::test_support::model_request_builder(
                    ConversationView::new(vec![ConversationItem::new(
                        id::<EntryId>(base + 100)?,
                        Actor::Assistant,
                        vec![
                            ContentBlock::Text(TextBlock::new(
                                "portable sibling",
                                Some(TextPhase::Final),
                            )?),
                            native_output_with_fallback(
                                &source_protocol,
                                &source_family,
                                &source_model,
                                "private-mixed-marker",
                                fallback.clone(),
                            )?,
                        ],
                        Provenance::ModelOutput,
                    )?]),
                    metadata()?,
                )
                .build()?;
                let mixed = lower_responses_request(&mixed, config())?;
                assert_eq!(mixed.body()["input"][0]["role"], "assistant");
                assert_eq!(
                    mixed.body()["input"][0]["content"][0]["text"],
                    "portable sibling"
                );
                let expected_parts =
                    usize::from(matches!(fallback, OpaqueFallback::SemanticSummary(_))) + 1;
                assert_eq!(
                    mixed.body()["input"][0]["content"]
                        .as_array()
                        .expect("message parts")
                        .len(),
                    expected_parts
                );
                assert!(!mixed.body().to_string().contains("private-"));
            }

            let user_summary = crate::test_support::model_request_builder(
                ConversationView::new(vec![ConversationItem::new(
                    id::<EntryId>(990 + u128::try_from(direction)?)?,
                    Actor::User,
                    vec![native_output_with_fallback(
                        &source_protocol,
                        &source_family,
                        &source_model,
                        "private-user-marker",
                        OpaqueFallback::SemanticSummary("portable user summary".to_owned()),
                    )?],
                    Provenance::UserInput,
                )?]),
                metadata()?,
            )
            .build()?;
            let lowered = lower_responses_request(
                &user_summary,
                ResponsesRequestConfig::new(
                    &target_protocol,
                    Some(&target_family),
                    &target_model,
                    policy,
                    None,
                ),
            )?;
            assert_eq!(lowered.body()["input"][0]["role"], "user");
            assert_eq!(
                lowered.body()["input"][0]["content"][0],
                json!({"type":"input_text","text":"portable user summary"})
            );
        }
        Ok(())
    }

    #[test]
    fn opaque_only_cross_target_semantics_fail_closed_without_payload_disclosure() -> TestResult {
        let cases = [
            (
                ProtocolId::new("openai.codex.responses")?,
                ModelFamilyId::new("gpt")?,
                ModelId::new("gpt-5.6")?,
                ProtocolId::new("xai.supergrok.responses")?,
                ModelFamilyId::new("grok")?,
                ModelId::new("grok-4.5")?,
                ResponsesRequestPolicy::supergrok_stateless(),
                800,
            ),
            (
                ProtocolId::new("xai.supergrok.responses")?,
                ModelFamilyId::new("grok")?,
                ModelId::new("grok-4.5")?,
                ProtocolId::new("openai.codex.responses")?,
                ModelFamilyId::new("gpt")?,
                ModelId::new("gpt-5.6")?,
                ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
                810,
            ),
        ];
        for (
            source_protocol,
            source_family,
            source_model,
            target_protocol,
            target_family,
            target_model,
            policy,
            entry,
        ) in cases
        {
            let request = crate::test_support::model_request_builder(
                ConversationView::new(vec![ConversationItem::new(
                    id::<EntryId>(entry)?,
                    Actor::Assistant,
                    vec![native_output(
                        &source_protocol,
                        &source_family,
                        &source_model,
                        "secret-opaque-only-marker",
                    )?],
                    Provenance::ModelOutput,
                )?]),
                metadata()?,
            )
            .build()?;
            let error = lower_responses_request(
                &request,
                ResponsesRequestConfig::new(
                    &target_protocol,
                    Some(&target_family),
                    &target_model,
                    policy,
                    None,
                ),
            )
            .expect_err("opaque-only foreign semantics must fail closed");
            assert_eq!(
                error,
                ResponsesRequestError::Content(
                    ResponsesRequestContentError::OpaqueOnlyWithoutCompatibleReplay
                )
            );
            assert!(!error.to_string().contains("secret-opaque-only-marker"));
            assert!(!format!("{error:?}").contains("secret-opaque-only-marker"));
        }
        Ok(())
    }

    #[test]
    fn codex_deltas_are_closed_policy_data() -> TestResult {
        let (protocol, family, model) = target_values()?;
        let optional_schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type":"object", "properties":{
                "source":{"type":"string"},
                "yield_time_ms":{"type":"integer","minimum":0,"maximum":10000}
            }, "required":["source"], "additionalProperties":false
        });
        let optional_tool = ConfiguredTool::new(
            ToolDefinition::new(
                ToolName::new("exec_js")?,
                "1.0.0",
                "Run local JavaScript",
                ToolInputContract::structured(optional_schema.clone())?,
                ToolOutputContract::portable_model_content(),
                ToolConcurrency::Parallel,
                ToolEffect::ReadOnly,
            )?,
            ToolExecutionDisposition::Local,
            ToolCallerDisposition::DirectOnly,
        );
        let request =
            crate::test_support::model_request_builder(ConversationView::default(), metadata()?)
                .tools(vec![optional_tool, tool("write_code", true)?])
                .limits(GenerationLimits::new(
                    Some(NonZeroU32::new(512).expect("nonzero")),
                    Vec::new(),
                )?)
                .tool_choice(ToolChoice::Specific(ToolName::new("write_code")?))
                .build()?;
        let lowered = lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_codex(false, true, "reasoning.encrypted_content"),
                None,
            ),
        )?;
        assert_eq!(lowered.body()["store"], false);
        assert!(lowered.body().get("max_output_tokens").is_none());
        assert_eq!(lowered.body()["tools"][0]["strict"], false);
        assert_eq!(
            lowered.body()["tools"][0]["parameters"]["required"],
            json!(["source"])
        );
        assert_eq!(
            lowered.body()["tools"][0]["parameters"]["properties"],
            optional_schema["properties"]
        );
        assert_eq!(lowered.body()["tools"][1]["type"], "custom");
        assert_eq!(
            lowered.body()["tool_choice"],
            json!({"type": "custom", "name": "write_code"})
        );
        assert!(
            lowered
                .custom_tools()
                .contains(&ToolName::new("write_code")?)
        );
        Ok(())
    }

    #[test]
    fn grok_deltas_emit_exact_stateless_reasoning_wire() -> TestResult {
        let (protocol, family, model) = target_values()?;
        let request = crate::test_support::model_request_builder(
            ConversationView::new(vec![user(41, "Inspect the repository.")?]),
            metadata()?,
        )
        .limits(GenerationLimits::new(
            Some(NonZeroU32::new(256).expect("nonzero")),
            Vec::new(),
        )?)
        .reasoning(ReasoningPolicy::Enabled {
            effort: Some(ReasoningEffort::Medium),
            requirement: CapabilityRequirement::Required,
        })
        .build()?;
        let policy = ResponsesRequestPolicy::supergrok_stateless();
        let lowered = lower_responses_request(
            &request,
            ResponsesRequestConfig::new(&protocol, Some(&family), &model, policy, None),
        )?;
        assert_eq!(
            lowered.body(),
            &json!({
                "model": "gpt-test",
                "input": [{
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "Inspect the repository."}]
                }],
                "stream": true,
                "store": false,
                "include": ["reasoning.encrypted_content"],
                "max_output_tokens": 256,
                "reasoning": {"effort": "medium", "summary": "concise"}
            })
        );
        assert!(lowered.body().get("tool_choice").is_none());
        assert!(lowered.body().get("parallel_tool_calls").is_none());
        Ok(())
    }

    #[test]
    fn supergrok_lowers_instructions_as_first_input_system_message() -> TestResult {
        let (protocol, family, model) = target_values()?;
        let request = crate::test_support::model_request_builder(
            ConversationView::new(vec![user(10, "hello")?]),
            metadata()?,
        )
        .instructions(vec![InstructionBlock::new(
            InstructionSource::Profile,
            "You are a precise coding agent.",
        )?])
        .build()?;
        let lowered = lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::supergrok_stateless(),
                None,
            ),
        )?;
        assert!(lowered.body().get("instructions").is_none());
        let input = lowered.body()["input"].as_array().ok_or("input")?;
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["role"], "system");
        assert!(
            input[0]["content"][0]["text"]
                .as_str()
                .is_some_and(|text| text.contains("precise coding agent"))
        );
        Ok(())
    }

    #[test]
    fn supergrok_input_omits_output_only_status_on_reasoning_replay() -> TestResult {
        let (request, active) = active_replay_fixture(ResponsesStorageMode::Stateless, None)?;
        let (protocol, family, model) = target_values()?;
        let bridge = ResponsesArtifactReplayBridge::from_active_artifact(
            &active,
            &request,
            operation_authority(
                &request,
                "responses-test-route",
                ResponsesStoragePolicy::Stateless,
            )?,
            ResponsesReplayKind::new("openai.output_item", 2),
            None,
            None,
            Timestamp::new(1_700_000_000, 0)?,
        )?;
        let lowered = lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::supergrok_stateless(),
                Some(bridge.context()),
            ),
        )?;
        let reasoning = lowered.body()["input"]
            .as_array()
            .ok_or("input")?
            .iter()
            .find(|item| item["type"] == "reasoning")
            .ok_or("missing reasoning replay")?;
        assert_eq!(reasoning["encrypted_content"], "fixture-encrypted");
        assert_eq!(reasoning["id"], "fixture-reasoning");
        assert!(
            reasoning.get("status").is_none(),
            "Grok Build rejects reasoning status on input, got {reasoning}"
        );
        Ok(())
    }

    /// The official client sends `summary: "auto"` for every catalog model
    /// that admits the parameter; the summaries it returns are the only wire
    /// a Codex thought block can arrive on.
    #[test]
    fn codex_reasoning_requests_auto_summaries_and_always_includes_encrypted_replay() -> TestResult
    {
        let (protocol, family, model) = target_values()?;
        let request =
            crate::test_support::model_request_builder(ConversationView::default(), metadata()?)
                .reasoning(ReasoningPolicy::Enabled {
                    effort: Some(ReasoningEffort::High),
                    requirement: CapabilityRequirement::Required,
                })
                .build()?;
        let lowered = lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
                None,
            ),
        )?;
        assert_eq!(
            lowered.body()["reasoning"],
            json!({"effort": "high", "summary": "auto"})
        );
        assert_eq!(
            lowered.body()["include"],
            json!(["reasoning.encrypted_content"])
        );
        Ok(())
    }

    #[test]
    fn grok_emits_stateless_store_and_encrypted_include_like_the_pinned_reference() -> TestResult {
        let (protocol, family, model) = target_values()?;
        let request =
            crate::test_support::model_request_builder(ConversationView::default(), metadata()?)
                .build()?;
        let lowered = lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::supergrok_stateless(),
                None,
            ),
        )?;
        assert!(lowered.body().get("reasoning").is_none());
        assert_eq!(
            lowered.body()["include"],
            json!(["reasoning.encrypted_content"])
        );
        assert_eq!(lowered.body()["store"], false);
        Ok(())
    }

    #[test]
    fn grok_reasoning_policy_admits_xhigh_for_model_specific_upstream_gating() -> TestResult {
        let (protocol, family, model) = target_values()?;
        let policy = ResponsesRequestPolicy::supergrok_stateless();
        let xhigh =
            crate::test_support::model_request_builder(ConversationView::default(), metadata()?)
                .reasoning(ReasoningPolicy::Enabled {
                    effort: Some(ReasoningEffort::XHigh),
                    requirement: CapabilityRequirement::Required,
                })
                .build()?;
        assert!(
            lower_responses_request(
                &xhigh,
                ResponsesRequestConfig::new(&protocol, Some(&family), &model, policy, None,),
            )
            .is_ok()
        );
        Ok(())
    }

    #[test]
    fn xai_platform_policy_lowers_none_through_xhigh_and_refuses_the_rest() -> TestResult {
        type Lowered = Result<Result<Value, ResponsesRequestError>, Box<dyn std::error::Error>>;
        let (protocol, family, model) = target_values()?;
        let policy = ResponsesRequestPolicy::xai_platform_stateless("reasoning.encrypted_content");
        let lower = |effort| -> Lowered {
            let request = crate::test_support::model_request_builder(
                ConversationView::default(),
                metadata()?,
            )
            .reasoning(ReasoningPolicy::Enabled {
                effort: Some(effort),
                requirement: CapabilityRequirement::Required,
            })
            .build()?;
            Ok(lower_responses_request(
                &request,
                ResponsesRequestConfig::new(&protocol, Some(&family), &model, policy, None),
            )
            .map(|lowered| lowered.body()["reasoning"].clone()))
        };
        for (effort, wire) in [
            (ReasoningEffort::None, "none"),
            (ReasoningEffort::Low, "low"),
            (ReasoningEffort::Medium, "medium"),
            (ReasoningEffort::High, "high"),
            (ReasoningEffort::XHigh, "xhigh"),
        ] {
            assert_eq!(lower(effort)??, json!({ "effort": wire }), "{effort:?}");
        }
        for effort in [ReasoningEffort::Max, ReasoningEffort::Ultra] {
            assert!(matches!(
                lower(effort)?,
                Err(ResponsesRequestError::Option(
                    ResponsesRequestOptionError::UnknownReasoningEffort
                ))
            ));
        }
        Ok(())
    }

    #[test]
    fn cache_intent_fails_closed_when_policy_has_no_exact_wire_semantics() -> TestResult {
        let (protocol, family, model) = target_values()?;
        let request =
            crate::test_support::model_request_builder(ConversationView::default(), metadata()?)
                .cache(CachePolicy::Enabled {
                    requirement: CapabilityRequirement::Required,
                })
                .build()?;
        assert!(matches!(
            lower_responses_request(
                &request,
                ResponsesRequestConfig::new(
                    &protocol,
                    Some(&family),
                    &model,
                    ResponsesRequestPolicy::supergrok_stateless(),
                    None,
                ),
            ),
            Err(ResponsesRequestError::Option(
                ResponsesRequestOptionError::PromptCache
            ))
        ));

        let disabled =
            crate::test_support::model_request_builder(ConversationView::default(), metadata()?)
                .cache(CachePolicy::Disabled)
                .build()?;
        for policy in [
            ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
            ResponsesRequestPolicy::supergrok_stateless(),
        ] {
            let lowered = lower_responses_request(
                &disabled,
                ResponsesRequestConfig::new(&protocol, Some(&family), &model, policy, None),
            )?;
            assert!(lowered.body().get("prompt_cache_key").is_none());
        }
        Ok(())
    }

    #[test]
    fn affinity_key_and_gateway_session_emit_from_branch_identity() -> TestResult {
        let (protocol, family, model) = target_values()?;
        let affinity = Digest::sha256(DigestDomain::CacheAffinity, 1, &[b"test-affinity"])?;
        let request =
            crate::test_support::model_request_builder(ConversationView::default(), metadata()?)
                .cache(CachePolicy::Disabled)
                .build()?;
        let codex = lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
                None,
            )
            .with_cache_affinity(Some(&affinity)),
        )?;
        assert_eq!(
            codex.body()["prompt_cache_key"],
            json!(affinity.to_string())
        );
        assert!(codex.body().get("session_id").is_none());
        assert!(codex.body().get("cache_control").is_none());

        let openrouter = lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openrouter_stateless(),
                None,
            )
            .with_cache_affinity(Some(&affinity)),
        )?;
        assert_eq!(openrouter.body()["session_id"], json!(affinity.to_string()));
        assert!(openrouter.body().get("prompt_cache_key").is_none());
        assert!(openrouter.body().get("cache_control").is_none());
        Ok(())
    }

    #[test]
    fn forked_branch_affinity_does_not_reuse_source_wire_keys() -> TestResult {
        let (protocol, family, model) = target_values()?;
        let session = id::<SessionId>(90_001)?;
        let source_branch = id::<BranchId>(90_002)?;
        let fork_branch = id::<BranchId>(90_003)?;
        let source =
            ModelCacheAffinity::from_runtime_entropy(session, source_branch, 0, [0x41; 32])?;
        let fork = ModelCacheAffinity::from_runtime_entropy(session, fork_branch, 0, [0x42; 32])?;
        let recovered =
            ModelCacheAffinity::from_runtime_entropy(session, fork_branch, 0, [0x42; 32])?;
        assert_ne!(source.identity(), fork.identity());
        assert_eq!(fork.identity(), recovered.identity());
        let request =
            crate::test_support::model_request_builder(ConversationView::default(), metadata()?)
                .cache(CachePolicy::Disabled)
                .build()?;
        let source_id = source.identity();
        let fork_id = fork.identity();

        let source_codex = lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
                None,
            )
            .with_cache_affinity(Some(&source_id)),
        )?;
        let fork_codex = lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
                None,
            )
            .with_cache_affinity(Some(&fork_id)),
        )?;
        assert_eq!(
            source_codex.body()["prompt_cache_key"],
            json!(source_id.to_string())
        );
        assert_eq!(
            fork_codex.body()["prompt_cache_key"],
            json!(fork_id.to_string())
        );
        assert_ne!(
            source_codex.body()["prompt_cache_key"],
            fork_codex.body()["prompt_cache_key"]
        );
        assert!(source_codex.body().get("session_id").is_none());
        assert!(fork_codex.body().get("session_id").is_none());

        let source_or = lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openrouter_stateless(),
                None,
            )
            .with_cache_affinity(Some(&source_id)),
        )?;
        let fork_or = lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openrouter_stateless(),
                None,
            )
            .with_cache_affinity(Some(&fork_id)),
        )?;
        assert_eq!(source_or.body()["session_id"], json!(source_id.to_string()));
        assert_eq!(fork_or.body()["session_id"], json!(fork_id.to_string()));
        assert_ne!(source_or.body()["session_id"], fork_or.body()["session_id"]);
        assert!(source_or.body().get("prompt_cache_key").is_none());
        assert!(fork_or.body().get("prompt_cache_key").is_none());
        assert!(source_or.body().get("cache_control").is_none());
        assert!(fork_or.body().get("cache_control").is_none());
        Ok(())
    }

    #[test]
    fn multimodal_tool_result_lowers_exact_data_urls_and_detail() -> TestResult {
        let image_url = "data:image/png;base64,AAAA";
        let audio_url = "data:audio/wav;base64,BBBB";
        let lowered = lower_tool_result(&[
            ToolResultPart::Image(plakous_model::ImageBlock::with_detail(
                "image/png",
                MediaSource::data_url(image_url)?,
                Some(ImageDetailHint::Original),
            )?),
            ToolResultPart::Audio(plakous_model::AudioBlock::new(
                "audio/wav",
                MediaSource::data_url(audio_url)?,
            )?),
        ])?;
        assert_eq!(
            lowered,
            json!([
                {"type":"input_image", "image_url":image_url, "detail":"original"},
                {"type":"input_audio", "audio_url":audio_url}
            ])
        );
        Ok(())
    }

    #[test]
    fn ordered_function_and_custom_tool_groups_lower_exactly() -> TestResult {
        let (protocol, family, model) = target_values()?;
        let long_result = "x".repeat(4_096);
        let function_call = ToolCallBlock::new(
            id::<CallId>(20)?,
            ToolName::new("lookup")?,
            ToolExecutionOwner::PlakLocal,
            ToolCallState::Requested,
            ToolCorrelation::new(
                Some("remote-function".to_owned()),
                None,
                ToolCallerLineage::Direct,
            )?,
            r#"{"q":"rust"}"#,
        )?;
        let custom_call = ToolCallBlock::new(
            id::<CallId>(21)?,
            ToolName::new("write_code")?,
            ToolExecutionOwner::PlakLocal,
            ToolCallState::Requested,
            ToolCorrelation::new(
                Some("remote-custom".to_owned()),
                None,
                ToolCallerLineage::Direct,
            )?,
            serde_json::to_string("println!(\"héllo\")")?,
        )?;
        let call_item = ConversationItem::new(
            id::<EntryId>(22)?,
            Actor::Assistant,
            vec![
                ContentBlock::ToolCall(function_call.clone()),
                ContentBlock::ToolCall(custom_call.clone()),
            ],
            Provenance::ModelOutput,
        )?;
        let result_item = ConversationItem::new(
            id::<EntryId>(23)?,
            Actor::Tool,
            vec![
                ContentBlock::ToolResult(ToolResultBlock::new(
                    function_call.call_id(),
                    ToolExecutionOwner::PlakLocal,
                    ToolResultKind::Success,
                    vec![ToolResultPart::Text(TextBlock::new(
                        long_result.clone(),
                        None,
                    )?)],
                    function_call.correlation().clone(),
                )?),
                ContentBlock::ToolResultUpdate(ToolResultUpdateBlock::new(
                    custom_call.call_id(),
                    ToolExecutionOwner::PlakLocal,
                    TextBlock::new("halfway", None)?,
                    custom_call.correlation().clone(),
                )?),
                ContentBlock::ToolResult(ToolResultBlock::new(
                    custom_call.call_id(),
                    ToolExecutionOwner::PlakLocal,
                    ToolResultKind::Success,
                    vec![ToolResultPart::Text(TextBlock::new("written", None)?)],
                    custom_call.correlation().clone(),
                )?),
            ],
            Provenance::ToolExecution,
        )?;
        let request = crate::test_support::model_request_builder(
            ConversationView::new(vec![call_item, result_item]),
            workflow_metadata()?,
        )
        .tools(vec![tool("lookup", false)?, tool("write_code", true)?])
        .build()?;
        let lowered = lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_codex(false, true, "reasoning.encrypted_content"),
                None,
            ),
        )?;
        let input = lowered.body()["input"].as_array().expect("input");
        assert_eq!(input[0]["type"], "function_call");
        assert_eq!(input[1]["type"], "custom_tool_call");
        assert_eq!(input[1]["input"], "println!(\"héllo\")");
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[2]["output"], long_result);
        assert_eq!(input[3]["type"], "custom_tool_call_output");
        assert_eq!(input[3]["name"], "write_code");
        assert_eq!(input[3]["output"], "halfway");
        assert_eq!(input[4]["type"], "custom_tool_call_output");

        let invalid_custom = ToolCallBlock::new(
            id::<CallId>(27)?,
            ToolName::new("write_code")?,
            ToolExecutionOwner::PlakLocal,
            ToolCallState::Requested,
            ToolCorrelation::new(
                Some("invalid-custom".to_owned()),
                None,
                ToolCallerLineage::Direct,
            )?,
            r#"{"code":"print(1)"}"#,
        )?;
        let invalid_request = crate::test_support::model_request_builder(
            ConversationView::new(vec![
                ConversationItem::new(
                    id::<EntryId>(28)?,
                    Actor::Assistant,
                    vec![ContentBlock::ToolCall(invalid_custom.clone())],
                    Provenance::ModelOutput,
                )?,
                ConversationItem::new(
                    id::<EntryId>(29)?,
                    Actor::Tool,
                    vec![ContentBlock::ToolResult(ToolResultBlock::new(
                        invalid_custom.call_id(),
                        ToolExecutionOwner::PlakLocal,
                        ToolResultKind::Success,
                        vec![ToolResultPart::Text(TextBlock::new("written", None)?)],
                        invalid_custom.correlation().clone(),
                    )?)],
                    Provenance::ToolExecution,
                )?,
            ]),
            metadata()?,
        )
        .tools(vec![tool("write_code", true)?])
        .build()?;
        assert!(matches!(
            lower_responses_request(
                &invalid_request,
                ResponsesRequestConfig::new(
                    &protocol,
                    Some(&family),
                    &model,
                    ResponsesRequestPolicy::openai_codex(
                        false,
                        true,
                        "reasoning.encrypted_content"
                    ),
                    None,
                ),
            ),
            Err(ResponsesRequestError::Content(
                ResponsesRequestContentError::InvalidCustomToolArguments
            ))
        ));
        Ok(())
    }

    /// Rebind re-projection: a Codex Code Mode `exec` cell (a
    /// UTF-8 custom call, canonical as a JSON string literal) replayed onto a
    /// Responses target that exposes no custom tool — Codex Direct, Grok —
    /// lowers as a `function_call` carrying the original program text, and its
    /// yields fold into the one `function_call_output` in source order.
    #[test]
    fn code_mode_exec_cell_replays_as_a_function_call_on_a_direct_target() -> TestResult {
        let (protocol, family, model) = target_values()?;
        let program = "const out = await tools.exec_command({cmd: 'pwd'});\nyield_(out);";
        let exec_call = ToolCallBlock::new(
            id::<CallId>(40)?,
            ToolName::new("exec")?,
            ToolExecutionOwner::PlakLocal,
            ToolCallState::Requested,
            ToolCorrelation::new(
                Some("call_exec_1".to_owned()),
                None,
                ToolCallerLineage::Direct,
            )?,
            serde_json::to_string(program)?,
        )?;
        assert!(matches!(exec_call.arguments(), Some(Value::String(_))));
        let wait_call = ToolCallBlock::new(
            id::<CallId>(41)?,
            ToolName::new("wait")?,
            ToolExecutionOwner::PlakLocal,
            ToolCallState::Requested,
            ToolCorrelation::new(
                Some("call_wait_1".to_owned()),
                None,
                ToolCallerLineage::Direct,
            )?,
            r#"{"cell":"c1"}"#,
        )?;
        let call_item = ConversationItem::new(
            id::<EntryId>(42)?,
            Actor::Assistant,
            vec![
                ContentBlock::ToolCall(exec_call.clone()),
                ContentBlock::ToolCall(wait_call.clone()),
            ],
            Provenance::ModelOutput,
        )?;
        let result_item = ConversationItem::new(
            id::<EntryId>(43)?,
            Actor::Tool,
            vec![
                ContentBlock::ToolResultUpdate(ToolResultUpdateBlock::new(
                    exec_call.call_id(),
                    ToolExecutionOwner::PlakLocal,
                    TextBlock::new("/home/op", None)?,
                    exec_call.correlation().clone(),
                )?),
                ContentBlock::ToolResult(ToolResultBlock::new(
                    exec_call.call_id(),
                    ToolExecutionOwner::PlakLocal,
                    ToolResultKind::Success,
                    vec![ToolResultPart::Text(TextBlock::new("cell done", None)?)],
                    exec_call.correlation().clone(),
                )?),
                ContentBlock::ToolResult(ToolResultBlock::new(
                    wait_call.call_id(),
                    ToolExecutionOwner::PlakLocal,
                    ToolResultKind::Success,
                    vec![ToolResultPart::Text(TextBlock::new("waited", None)?)],
                    wait_call.correlation().clone(),
                )?),
            ],
            Provenance::ToolExecution,
        )?;
        // The Direct target exposes only structured functions.
        let request = crate::test_support::model_request_builder(
            ConversationView::new(vec![call_item, result_item]),
            workflow_metadata()?,
        )
        .tools(vec![tool("exec_command", false)?])
        .reasoning(ReasoningPolicy::Enabled {
            effort: Some(ReasoningEffort::Medium),
            requirement: CapabilityRequirement::Required,
        })
        .build()?;
        let lowered = lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_codex(false, true, "reasoning.encrypted_content"),
                None,
            ),
        )?;
        let input = lowered.body()["input"].as_array().expect("input");
        assert_eq!(input[0]["type"], "function_call");
        assert_eq!(input[0]["name"], "exec");
        assert_eq!(
            input[0]["arguments"], program,
            "original string input, verbatim"
        );
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["name"], "wait");
        assert_eq!(input[1]["arguments"], r#"{"cell":"c1"}"#);
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[2]["call_id"], "call_exec_1");
        assert_eq!(
            input[2]["output"],
            json!([
                {"type":"input_text","text":"/home/op"},
                {"type":"input_text","text":"cell done"}
            ]),
            "the cell's yield folds into its one output in source order"
        );
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["call_id"], "call_wait_1");
        assert_eq!(input[3]["output"], "waited");
        assert!(
            input
                .iter()
                .all(|item| item["type"] != "custom_tool_call_output"),
            "a function call never owns a custom_tool_call_output: {input:?}"
        );
        let affinity = Digest::sha256(DigestDomain::CacheAffinity, 1, &[b"codex-lite-round"])?;
        let lite = lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_codex_lite("reasoning.encrypted_content"),
                None,
            )
            .with_cache_affinity(Some(&affinity)),
        )?;
        let lite_input = lite.body()["input"].as_array().ok_or("lite input")?;
        assert_eq!(lite_input[0]["type"], "additional_tools");
        assert_eq!(lite_input[0]["role"], "developer");
        assert_eq!(lite_input[0]["tools"][0]["type"], "namespace");
        assert_eq!(lite_input[0]["tools"][0]["name"], "functions");
        assert_eq!(
            lite_input[0]["tools"][0]["tools"][0]["name"],
            "exec_command"
        );
        assert_eq!(lite_input[1]["type"], "function_call");
        assert_eq!(lite_input[1]["call_id"], "call_exec_1");
        assert_eq!(lite_input[3]["type"], "function_call_output");
        assert_eq!(lite_input[3]["call_id"], "call_exec_1");
        assert_eq!(lite_input[4]["call_id"], "call_wait_1");
        assert!(lite.body().get("tools").is_none());
        assert!(lite.body().get("instructions").is_none());
        assert_eq!(lite.body()["parallel_tool_calls"], false);
        assert_eq!(lite.body()["reasoning"]["context"], "all_turns");
        assert_eq!(lite.body()["text"]["verbosity"], "low");
        Ok(())
    }

    #[test]
    fn fresh_codex_body_matches_portable_reasoning_projection() -> TestResult {
        let (protocol, family, model) = target_values()?;
        let affinity = Digest::sha256(DigestDomain::CacheAffinity, 1, &[b"offline-pass"])?;
        let reasoning = ContentBlock::Reasoning(ReasoningBlock::new(
            ReasoningVisibility::OpaqueReplay,
            None,
            Some(ReplayEnvelope::new(
                protocol.clone(),
                Some(family.clone()),
                ReplayScope::ExactModel(model.clone()),
                ReplayKindId::new("responses.output_item")?,
                1,
                DataClassification::Sensitive,
                RawValue::from_string(
                    json!({
                        "type": "reasoning", "id": "fixture-reasoning",
                        "encrypted_content": "fixture-encrypted", "summary": []
                    })
                    .to_string(),
                )?,
            )?),
        )?);
        let call = ToolCallBlock::new(
            id::<CallId>(930)?,
            ToolName::new("lookup")?,
            ToolExecutionOwner::PlakLocal,
            ToolCallState::Requested,
            ToolCorrelation::default(),
            r#"{"q":"cache"}"#,
        )?;
        let items = vec![
            user(931, "keep reading")?,
            ConversationItem::new(
                id::<EntryId>(932)?,
                Actor::Assistant,
                vec![reasoning.clone()],
                Provenance::ModelOutput,
            )?,
            ConversationItem::new(
                id::<EntryId>(933)?,
                Actor::Assistant,
                vec![
                    reasoning,
                    ContentBlock::Text(TextBlock::new("looking up", None)?),
                    ContentBlock::ToolCall(call.clone()),
                ],
                Provenance::ModelOutput,
            )?,
            ConversationItem::new(
                id::<EntryId>(934)?,
                Actor::Tool,
                vec![ContentBlock::ToolResult(ToolResultBlock::new(
                    call.call_id(),
                    ToolExecutionOwner::PlakLocal,
                    ToolResultKind::Success,
                    vec![ToolResultPart::Text(TextBlock::new("result", None)?)],
                    ToolCorrelation::default(),
                )?)],
                Provenance::ToolExecution,
            )?,
        ];
        // The agent's portable_compaction_history drops reasoning and elides
        // items with no surviving blocks; keep that exact projection here
        // (pinned by portable_history_drops_reasoning_and_elides_reasoning_only_items).
        let mut portable = Vec::new();
        for item in &items {
            let blocks = item
                .blocks()
                .iter()
                .filter(|block| !matches!(block, ContentBlock::Reasoning(_)))
                .cloned()
                .collect::<Vec<_>>();
            if !blocks.is_empty() {
                portable.push(ConversationItem::new(
                    item.id(),
                    item.actor(),
                    blocks,
                    item.provenance(),
                )?);
            }
        }
        let lower = |items| -> Result<LoweredResponsesRequest, Box<dyn std::error::Error>> {
            let request = crate::test_support::model_request_builder(
                ConversationView::new(items),
                metadata()?,
            )
            .build()?;
            Ok(lower_responses_request(
                &request,
                ResponsesRequestConfig::new(
                    &protocol,
                    Some(&family),
                    &model,
                    ResponsesRequestPolicy::openai_codex(
                        false,
                        false,
                        "reasoning.encrypted_content",
                    ),
                    None,
                )
                .with_cache_affinity(Some(&affinity)),
            )?)
        };
        let unchanged = lower(items)?;
        let portable = lower(portable)?;
        assert_eq!(
            unchanged.body_bytes()?,
            portable.body_bytes()?,
            "Fix A must compare the real serialized Codex body"
        );
        assert!(!String::from_utf8(unchanged.body_bytes()?)?.contains("fixture-encrypted"));
        Ok(())
    }

    #[test]
    fn result_before_call_fails_with_exact_protected_group_error() -> TestResult {
        let (protocol, family, model) = target_values()?;
        let call = ToolCallBlock::new(
            id::<CallId>(30)?,
            ToolName::new("lookup")?,
            ToolExecutionOwner::PlakLocal,
            ToolCallState::Requested,
            ToolCorrelation::default(),
            r#"{"q":"order"}"#,
        )?;
        let result = ToolResultBlock::new(
            call.call_id(),
            ToolExecutionOwner::PlakLocal,
            ToolResultKind::Success,
            vec![ToolResultPart::Text(TextBlock::new("found", None)?)],
            ToolCorrelation::default(),
        )?;
        let request = crate::test_support::model_request_builder(
            ConversationView::new(vec![
                ConversationItem::new(
                    id::<EntryId>(31)?,
                    Actor::Tool,
                    vec![ContentBlock::ToolResult(result)],
                    Provenance::ToolExecution,
                )?,
                ConversationItem::new(
                    id::<EntryId>(32)?,
                    Actor::Assistant,
                    vec![ContentBlock::ToolCall(call)],
                    Provenance::ModelOutput,
                )?,
            ]),
            metadata()?,
        )
        .tools(vec![tool("lookup", false)?])
        .build()?;
        assert!(matches!(
            lower_responses_request(
                &request,
                ResponsesRequestConfig::new(
                    &protocol,
                    Some(&family),
                    &model,
                    ResponsesRequestPolicy::openai_codex(
                        false,
                        true,
                        "reasoning.encrypted_content"
                    ),
                    None,
                ),
            ),
            Err(ResponsesRequestError::ProtectedTool(
                ResponsesProtectedToolError::ResultBeforeCall
            ))
        ));
        Ok(())
    }

    // `output_replay_group_is_binding_checked_and_exact` was deleted here: it
    // was `#[cfg(any())]` — compiled out unconditionally and silently green —
    // and it no longer builds, because `ResponsesReplayContext::new` has since
    // grown the request authority, the output coordinates, the provider
    // continuation and an expiry. Reviving it would mean writing a new test
    // against the current contract, not restoring this one; the binding checks
    // it once covered are exercised by the replay tests below.

    #[test]
    fn provider_stored_without_active_artifact_replays_full_canonical_request() -> TestResult {
        let (protocol, family, model) = target_values()?;
        let request = crate::test_support::model_request_builder(
            ConversationView::new(vec![user(45, "first")?, user(46, "second")?]),
            metadata()?,
        )
        .build()?;
        let lowered = lower_responses_request(
            &request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_platform(
                    ResponsesStoragePolicy::ProviderStored,
                    false,
                    false,
                ),
                None,
            ),
        )?;
        assert_eq!(lowered.replay_mode(), &ResponsesReplayMode::Full);
        assert_eq!(lowered.body()["input"].as_array().expect("input").len(), 2);
        assert!(lowered.body().get("previous_response_id").is_none());
        Ok(())
    }

    #[test]
    fn openai_platform_programmatic_request_and_replay_are_exact_in_both_storage_modes()
    -> TestResult {
        let (protocol, family, _) = target_values()?;
        let model = ModelId::new("gpt-5.6-sol")?;
        let base_tool = tool_with_concurrency("lookup", false, ToolConcurrency::Serial)?;
        let programmatic_tool = ConfiguredTool::new(
            base_tool.definition().clone(),
            base_tool.execution(),
            ToolCallerDisposition::ProviderProgrammaticOnly,
        );
        let producing_request = crate::test_support::model_request_builder(
            ConversationView::new(vec![user(145, "look up one value")?]),
            metadata()?,
        )
        .tools(vec![programmatic_tool.clone()])
        .hosted_tools(vec![HostedTool::ProgrammaticComposition])
        .resolved_harness_plan_identity(harness_plan_identity()?)
        .build()?;
        let initial = lower_responses_request(
            &producing_request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_platform(
                    ResponsesStoragePolicy::Stateless,
                    false,
                    false,
                ),
                None,
            ),
        )?;
        assert_eq!(
            initial.body()["tools"],
            json!([
                {
                    "type":"function", "name":"lookup", "description":"lookup description",
                    // No `$schema`: the wire projection strips the dialect URI.
                    "parameters":{
                        "type":"object", "properties":{"q":{"type":"string"}},
                        "required":["q"], "additionalProperties":false
                    },
                    "strict":false, "allowed_callers":["programmatic"]
                },
                {"type":"programmatic_tool_calling"}
            ])
        );

        let program_native = json!({
            "type":"program", "id":"prog_1", "call_id":"pcall_1",
            "code":"await lookup({q:'x'})", "fingerprint":"opaque-fingerprint-1"
        });
        let call_native = json!({
            "type":"function_call", "id":"fc_1", "call_id":"call_1", "name":"lookup",
            "arguments":"{\"q\":\"x\"}",
            "caller":{"type":"program","caller_id":"pcall_1"}
        });
        let replay = |native: &Value| {
            Ok::<_, Box<dyn std::error::Error>>(ReplayEnvelope::new(
                protocol.clone(),
                Some(family.clone()),
                ReplayScope::ExactModel(model.clone()),
                ReplayKindId::new("openai.output_item")?,
                2,
                DataClassification::Sensitive,
                RawValue::from_string(native.to_string())?,
            )?)
        };
        let program_replay = replay(&program_native)?;
        let call_replay = replay(&call_native)?;
        let lineage = ToolCallerLineage::provider_programmatic(
            NamespaceId::new("openai.responses.program")?,
            "pcall_1",
        )?;
        let call = ToolCallBlock::new(
            id::<CallId>(146)?,
            ToolName::new("lookup")?,
            ToolExecutionOwner::PlakLocal,
            ToolCallState::Requested,
            ToolCorrelation::new(Some("call_1".to_owned()), None, lineage.clone())?,
            r#"{"q":"x"}"#,
        )?;
        let program_item = ConversationItem::new(
            id::<EntryId>(147)?,
            Actor::Assistant,
            vec![ContentBlock::Opaque(OpaqueBlock::new(
                program_replay.clone(),
                OpaqueFallback::CompatibleReplay,
            )?)],
            Provenance::ModelOutput,
        )?;
        let call_item = ConversationItem::new(
            id::<EntryId>(148)?,
            Actor::Assistant,
            vec![
                ContentBlock::ToolCall(call.clone()),
                ContentBlock::Opaque(OpaqueBlock::new(
                    call_replay.clone(),
                    OpaqueFallback::CompatibleReplay,
                )?),
            ],
            Provenance::ModelOutput,
        )?;
        let result_item = ConversationItem::new(
            id::<EntryId>(149)?,
            Actor::Tool,
            vec![ContentBlock::ToolResult(ToolResultBlock::new(
                call.call_id(),
                ToolExecutionOwner::PlakLocal,
                ToolResultKind::Success,
                vec![ToolResultPart::Text(TextBlock::new("found", None)?)],
                ToolCorrelation::new(Some("call_1".to_owned()), None, lineage)?,
            )?)],
            Provenance::ToolExecution,
        )?;
        let next_request = crate::test_support::model_request_builder(
            ConversationView::from_shared(
                CanonicalHistory::new(vec![
                    HistorySegment::new(
                        HistorySegmentKind::RecentIntent,
                        vec![user(145, "look up one value")?],
                    )?,
                    HistorySegment::new(
                        HistorySegmentKind::ReasoningReplay,
                        vec![program_item.clone()],
                    )?,
                    HistorySegment::new(
                        HistorySegmentKind::ToolExchange,
                        vec![call_item.clone(), result_item.clone()],
                    )?,
                ])?,
                WorkingProjection::default(),
            ),
            metadata()?,
        )
        .tools(vec![programmatic_tool.clone()])
        .hosted_tools(vec![HostedTool::ProgrammaticComposition])
        .resolved_harness_plan_identity(harness_plan_identity()?)
        .build()?;

        let second_call_native = json!({
            "type":"function_call", "id":"fc_2", "call_id":"call_2", "name":"lookup",
            "arguments":"{\"q\":\"y\"}",
            "caller":{"type":"program","caller_id":"pcall_1"}
        });
        let second_call_replay = replay(&second_call_native)?;
        let second_lineage = ToolCallerLineage::provider_programmatic(
            NamespaceId::new("openai.responses.program")?,
            "pcall_1",
        )?;
        let second_call = ToolCallBlock::new(
            id::<CallId>(150)?,
            ToolName::new("lookup")?,
            ToolExecutionOwner::PlakLocal,
            ToolCallState::Requested,
            ToolCorrelation::new(Some("call_2".to_owned()), None, second_lineage.clone())?,
            r#"{"q":"y"}"#,
        )?;
        let second_call_item = ConversationItem::new(
            id::<EntryId>(151)?,
            Actor::Assistant,
            vec![
                ContentBlock::ToolCall(second_call.clone()),
                ContentBlock::Opaque(OpaqueBlock::new(
                    second_call_replay.clone(),
                    OpaqueFallback::CompatibleReplay,
                )?),
            ],
            Provenance::ModelOutput,
        )?;
        let second_result_item = ConversationItem::new(
            id::<EntryId>(152)?,
            Actor::Tool,
            vec![ContentBlock::ToolResult(ToolResultBlock::new(
                second_call.call_id(),
                ToolExecutionOwner::PlakLocal,
                ToolResultKind::Success,
                vec![ToolResultPart::Text(TextBlock::new("found again", None)?)],
                ToolCorrelation::new(Some("call_2".to_owned()), None, second_lineage)?,
            )?)],
            Provenance::ToolExecution,
        )?;
        let mut final_segments = next_request.input().history().segments().to_vec();
        final_segments.push(HistorySegment::new(
            HistorySegmentKind::ToolExchange,
            vec![second_call_item, second_result_item],
        )?);
        let final_request = crate::test_support::model_request_builder(
            ConversationView::from_shared(
                CanonicalHistory::new(final_segments)?,
                WorkingProjection::default(),
            ),
            metadata()?,
        )
        .tools(vec![programmatic_tool])
        .hosted_tools(vec![HostedTool::ProgrammaticComposition])
        .resolved_harness_plan_identity(harness_plan_identity()?)
        .build()?;
        let now = Timestamp::new(1_700_000_000, 0)?;
        for storage in [
            ResponsesStorageMode::ProviderStored,
            ResponsesStorageMode::Stateless,
        ] {
            let policy = if storage == ResponsesStorageMode::ProviderStored {
                ResponsesStoragePolicy::ProviderStored
            } else {
                ResponsesStoragePolicy::Stateless
            };
            let artifact = output_artifact(
                &producing_request,
                &next_request,
                "responses-ptc-test-route",
                vec![program_replay.clone(), call_replay.clone()],
                storage,
                "response-ptc-first-pause",
                None,
                now,
                b"openai-platform-ptc-replay",
            )?;
            let active = active_artifact(&artifact)?;
            let bridge = ResponsesArtifactReplayBridge::from_active_artifact(
                &active,
                &next_request,
                operation_authority(&next_request, "responses-ptc-test-route", policy)?,
                ResponsesReplayKind::new("openai.output_item", 2),
                None,
                (storage == ResponsesStorageMode::ProviderStored)
                    .then(|| ResponsesReplayKind::new("openai.previous_response_id", 1)),
                now,
            )?;
            assert_eq!(
                bridge.hosted_programs().live_program("pcall_1"),
                Some(("prog_1", "opaque-fingerprint-1"))
            );
            let continued = lower_responses_request(
                &next_request,
                ResponsesRequestConfig::new(
                    &protocol,
                    Some(&family),
                    &model,
                    ResponsesRequestPolicy::openai_platform(policy, false, false),
                    Some(bridge.context()),
                ),
            )?;
            if storage == ResponsesStorageMode::ProviderStored {
                assert!(matches!(
                    continued.replay_mode(),
                    ResponsesReplayMode::PreviousResponse {
                        prefix_items: 3,
                        ..
                    }
                ));
                assert_eq!(
                    continued.body()["previous_response_id"],
                    "response-ptc-first-pause"
                );
                assert_eq!(
                    continued.body()["input"],
                    json!([{"type":"function_call_output","call_id":"call_1","output":"found","caller":{"type":"program","caller_id":"pcall_1"}}])
                );
            } else {
                assert_eq!(continued.replay_mode(), &ResponsesReplayMode::Full);
                assert_eq!(
                    continued.body()["input"],
                    json!([
                        {"type":"message","role":"user","content":[{"type":"input_text","text":"look up one value"}]},
                        program_native,
                        call_native,
                        {"type":"function_call_output","call_id":"call_1","output":"found","caller":{"type":"program","caller_id":"pcall_1"}}
                    ])
                );
                assert_eq!(
                    continued.body()["input"][1]["fingerprint"],
                    "opaque-fingerprint-1"
                );
            }

            // The operation-budget wrap-up over this same programmatic history
            // withdraws tool selection only, so it keeps the bound replay and
            // continues the live program. Fresh lowering has no form for
            // hosted program items or provider-programmatic calls.
            let wrap_up = crate::test_support::model_request_builder(
                next_request.input().clone(),
                metadata()?,
            )
            .tools(next_request.tools().to_vec())
            .hosted_tools(next_request.hosted_tools().to_vec())
            .resolved_harness_plan_identity(harness_plan_identity()?)
            .tool_choice(ToolChoice::None)
            .build()?;
            let wrap_up_bridge = ResponsesArtifactReplayBridge::from_active_artifact(
                &active,
                &wrap_up,
                operation_authority(&wrap_up, "responses-ptc-test-route", policy)?,
                ResponsesReplayKind::new("openai.output_item", 2),
                None,
                (storage == ResponsesStorageMode::ProviderStored)
                    .then(|| ResponsesReplayKind::new("openai.previous_response_id", 1)),
                now,
            )?;
            assert_eq!(
                wrap_up_bridge.hosted_programs().live_program("pcall_1"),
                Some(("prog_1", "opaque-fingerprint-1"))
            );
            let wrapped = lower_responses_request(
                &wrap_up,
                ResponsesRequestConfig::new(
                    &protocol,
                    Some(&family),
                    &model,
                    ResponsesRequestPolicy::openai_platform(policy, false, false),
                    Some(wrap_up_bridge.context()),
                ),
            )?;
            assert_eq!(wrapped.body()["tool_choice"], "none");
            assert_eq!(wrapped.body()["tools"], continued.body()["tools"]);
            assert_eq!(wrapped.body()["input"], continued.body()["input"]);
            assert_eq!(wrapped.replay_mode(), continued.replay_mode());
            let fresh = lower_responses_request(
                &wrap_up,
                ResponsesRequestConfig::new(
                    &protocol,
                    Some(&family),
                    &model,
                    ResponsesRequestPolicy::openai_platform(policy, false, false),
                    None,
                ),
            );
            assert!(matches!(
                fresh.map_err(|error| {
                    crate::ResponsesAttemptBuildError::Request(error).preparation_error_kind()
                }),
                Err(plakous_model::ModelPreparationErrorKind::ProtectedHistory)
            ));

            let second_artifact = output_artifact(
                &next_request,
                &final_request,
                "responses-ptc-test-route",
                vec![second_call_replay.clone()],
                storage,
                "response-ptc-second-pause",
                None,
                now,
                b"openai-platform-ptc-second-replay",
            )?;
            assert_ne!(
                artifact.production().covered_projection(),
                second_artifact.production().covered_projection(),
                "the second pause must advance artifact coverage"
            );
            assert_eq!(artifact.production().covered_projection_items(), 3);
            assert_eq!(second_artifact.production().covered_projection_items(), 5);
            let second_active = active_artifact(&second_artifact)?;
            let second_bridge = ResponsesArtifactReplayBridge::from_active_artifact(
                &second_active,
                &final_request,
                operation_authority(&final_request, "responses-ptc-test-route", policy)?,
                ResponsesReplayKind::new("openai.output_item", 2),
                None,
                (storage == ResponsesStorageMode::ProviderStored)
                    .then(|| ResponsesReplayKind::new("openai.previous_response_id", 1)),
                now,
            )?;
            assert_eq!(second_bridge.hosted_programs().live_program_count(), 1);
            assert_eq!(
                second_bridge.hosted_programs().live_program("pcall_1"),
                Some(("prog_1", "opaque-fingerprint-1"))
            );
            let final_program_output = json!({
                "type":"program_output", "id":"program_output_1", "call_id":"pcall_1",
                "result":"found + found again", "status":"completed"
            });
            let mut completed_programs = second_bridge.hosted_programs().clone();
            completed_programs.admit_replayed_output(
                &parse_output_item::<ResponsesHostDecodeError>(&final_program_output)?,
                crate::ResponsesCanonicalPolicy::openai_platform(),
            )?;
            assert_eq!(completed_programs.live_program_count(), 0);
            let final_continuation = lower_responses_request(
                &final_request,
                ResponsesRequestConfig::new(
                    &protocol,
                    Some(&family),
                    &model,
                    ResponsesRequestPolicy::openai_platform(policy, false, false),
                    Some(second_bridge.context()),
                ),
            )?;
            if storage == ResponsesStorageMode::ProviderStored {
                assert!(matches!(
                    final_continuation.replay_mode(),
                    ResponsesReplayMode::PreviousResponse {
                        prefix_items: 5,
                        ..
                    }
                ));
                assert_eq!(
                    final_continuation.body()["previous_response_id"],
                    "response-ptc-second-pause"
                );
                assert_eq!(
                    final_continuation.body()["input"],
                    json!([{"type":"function_call_output","call_id":"call_2","output":"found again","caller":{"type":"program","caller_id":"pcall_1"}}])
                );
            } else {
                assert_eq!(final_continuation.replay_mode(), &ResponsesReplayMode::Full);
                assert!(
                    final_continuation
                        .body()
                        .get("previous_response_id")
                        .is_none()
                );
                assert_eq!(
                    final_continuation.body()["input"],
                    json!([
                        {"type":"message","role":"user","content":[{"type":"input_text","text":"look up one value"}]},
                        program_native,
                        call_native,
                        {"type":"function_call_output","call_id":"call_1","output":"found","caller":{"type":"program","caller_id":"pcall_1"}},
                        second_call_native,
                        {"type":"function_call_output","call_id":"call_2","output":"found again","caller":{"type":"program","caller_id":"pcall_1"}}
                    ])
                );
            }

            let wrong_program = replay(&json!({
                "type":"program", "id":"prog_1", "call_id":"pcall_1",
                "code":"await lookup({q:'x'})", "fingerprint":"wrong-fingerprint"
            }))?;
            let wrong_caller = replay(&json!({
                "type":"function_call", "id":"fc_1", "call_id":"call_1",
                "name":"lookup", "arguments":"{\"q\":\"x\"}",
                "caller":{"type":"program","caller_id":"wrong-program"}
            }))?;
            let mutated_views = [
                ConversationView::new(vec![
                    user(145, "look up one value")?,
                    ConversationItem::new(
                        id::<EntryId>(147)?,
                        Actor::Assistant,
                        vec![ContentBlock::Opaque(OpaqueBlock::new(
                            wrong_program.clone(),
                            OpaqueFallback::CompatibleReplay,
                        )?)],
                        Provenance::ModelOutput,
                    )?,
                    call_item.clone(),
                    result_item.clone(),
                ]),
                ConversationView::new(vec![
                    user(145, "look up one value")?,
                    program_item.clone(),
                    ConversationItem::new(
                        id::<EntryId>(148)?,
                        Actor::Assistant,
                        vec![
                            ContentBlock::ToolCall(call.clone()),
                            ContentBlock::Opaque(OpaqueBlock::new(
                                wrong_caller.clone(),
                                OpaqueFallback::CompatibleReplay,
                            )?),
                        ],
                        Provenance::ModelOutput,
                    )?,
                    result_item.clone(),
                ]),
                ConversationView::new(vec![
                    user(145, "look up one value")?,
                    call_item.clone(),
                    program_item.clone(),
                    result_item.clone(),
                ]),
            ];
            let mutated_replays = [
                vec![wrong_program, call_replay.clone()],
                vec![program_replay.clone(), wrong_caller],
                vec![call_replay.clone(), program_replay.clone()],
            ];
            for (mutation_index, (mutated_view, mutated_replay)) in
                mutated_views.into_iter().zip(mutated_replays).enumerate()
            {
                let mutated_request =
                    crate::test_support::model_request_builder(mutated_view, metadata()?)
                        .tools(vec![ConfiguredTool::new(
                            base_tool.definition().clone(),
                            base_tool.execution(),
                            ToolCallerDisposition::ProviderProgrammaticOnly,
                        )])
                        .hosted_tools(vec![HostedTool::ProgrammaticComposition])
                        .resolved_harness_plan_identity(harness_plan_identity()?)
                        .build()?;
                let mutated = output_artifact(
                    &producing_request,
                    &mutated_request,
                    "responses-ptc-test-route",
                    mutated_replay,
                    storage,
                    "response-ptc-first-pause",
                    None,
                    now,
                    format!("openai-platform-ptc-mutation-{mutation_index}").as_bytes(),
                )?;
                assert!(
                    ResponsesArtifactReplayBridge::from_active_artifact(
                        &active_artifact(&mutated)?,
                        if mutation_index == 0 {
                            &next_request
                        } else {
                            &mutated_request
                        },
                        operation_authority(
                            if mutation_index == 0 {
                                &next_request
                            } else {
                                &mutated_request
                            },
                            "responses-ptc-test-route",
                            policy,
                        )?,
                        ResponsesReplayKind::new("openai.output_item", 2),
                        None,
                        (storage == ResponsesStorageMode::ProviderStored)
                            .then(|| ResponsesReplayKind::new("openai.previous_response_id", 1)),
                        now,
                    )
                    .is_err(),
                    "storage {storage:?} replay mutation {mutation_index} was admitted"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn active_artifact_wrapper_binding_revision_family_and_bytes_fail_closed() -> TestResult {
        let now = Timestamp::new(1_700_000_000, 0)?;
        let expires = Timestamp::new(1_800_000_000, 0)?;
        let (request, active) =
            active_replay_fixture(ResponsesStorageMode::ProviderStored, Some(expires))?;
        let binding = active.binding();
        let current_binding = operation_authority(
            &request,
            "responses-test-route",
            ResponsesStoragePolicy::ProviderStored,
        )?
        .artifact_binding()
        .identity();
        assert_ne!(binding, current_binding);
        let bytes = active.bytes().to_vec();
        let mut tampered_bytes = bytes.clone();
        let last = tampered_bytes
            .last_mut()
            .ok_or_else(|| std::io::Error::other("empty active bytes"))?;
        *last ^= 0x01;
        let wrappers = [
            (
                "family",
                ActiveFamilyArtifact::new(
                    "messages",
                    RESPONSES_ARTIFACT_SCHEMA_REVISION,
                    RESPONSES_ARTIFACT_CODEC_REVISION,
                    binding,
                    bytes.clone(),
                )?,
            ),
            (
                "schema revision",
                ActiveFamilyArtifact::new(
                    "responses",
                    RESPONSES_ARTIFACT_SCHEMA_REVISION.saturating_add(1),
                    RESPONSES_ARTIFACT_CODEC_REVISION,
                    binding,
                    bytes.clone(),
                )?,
            ),
            (
                "codec revision",
                ActiveFamilyArtifact::new(
                    "responses",
                    RESPONSES_ARTIFACT_SCHEMA_REVISION,
                    RESPONSES_ARTIFACT_CODEC_REVISION.saturating_add(1),
                    binding,
                    bytes.clone(),
                )?,
            ),
            (
                "generic binding",
                ActiveFamilyArtifact::new(
                    "responses",
                    RESPONSES_ARTIFACT_SCHEMA_REVISION,
                    RESPONSES_ARTIFACT_CODEC_REVISION,
                    Digest::sha256(DigestDomain::ModelBinding, 1, &[b"foreign-neutral-binding"])?,
                    bytes,
                )?,
            ),
            (
                "rebound current-operation wrapper",
                ActiveFamilyArtifact::new(
                    "responses",
                    RESPONSES_ARTIFACT_SCHEMA_REVISION,
                    RESPONSES_ARTIFACT_CODEC_REVISION,
                    current_binding,
                    active.bytes().to_vec(),
                )?,
            ),
            (
                "family bytes",
                ActiveFamilyArtifact::new(
                    "responses",
                    RESPONSES_ARTIFACT_SCHEMA_REVISION,
                    RESPONSES_ARTIFACT_CODEC_REVISION,
                    binding,
                    tampered_bytes,
                )?,
            ),
        ];
        for (axis, wrapper) in &wrappers {
            assert!(
                ResponsesArtifactReplayBridge::from_active_artifact(
                    wrapper,
                    &request,
                    operation_authority(
                        &request,
                        "responses-test-route",
                        ResponsesStoragePolicy::ProviderStored,
                    )?,
                    ResponsesReplayKind::new("openai.output_item", 2),
                    None,
                    Some(ResponsesReplayKind::new("openai.previous_response_id", 1)),
                    now,
                )
                .is_err(),
                "invalid active artifact {axis} was admitted"
            );
        }
        Ok(())
    }

    #[test]
    fn active_artifact_expiry_storage_and_replay_kinds_fail_closed() -> TestResult {
        let now = Timestamp::new(1_700_000_000, 0)?;
        let expires = Timestamp::new(1_800_000_000, 0)?;
        let (request, active) =
            active_replay_fixture(ResponsesStorageMode::ProviderStored, Some(expires))?;
        let admit = |storage, output, opaque, previous, at| {
            ResponsesArtifactReplayBridge::from_active_artifact(
                &active,
                &request,
                operation_authority(&request, "responses-test-route", storage).expect("authority"),
                output,
                opaque,
                previous,
                at,
            )
        };
        assert!(matches!(
            admit(
                ResponsesStoragePolicy::ProviderStored,
                ResponsesReplayKind::new("openai.output_item", 2),
                None,
                Some(ResponsesReplayKind::new("openai.previous_response_id", 1)),
                expires,
            ),
            Err(ResponsesRequestError::Replay(
                ResponsesReplayError::ArtifactExpired
            ))
        ));
        for (axis, rejected) in [
            (
                "storage",
                admit(
                    ResponsesStoragePolicy::Stateless,
                    ResponsesReplayKind::new("openai.output_item", 2),
                    None,
                    Some(ResponsesReplayKind::new("openai.previous_response_id", 1)),
                    now,
                ),
            ),
            (
                "output kind revision",
                admit(
                    ResponsesStoragePolicy::ProviderStored,
                    ResponsesReplayKind::new("openai.output_item", 3),
                    None,
                    Some(ResponsesReplayKind::new("openai.previous_response_id", 1)),
                    now,
                ),
            ),
            (
                "opaque kind",
                admit(
                    ResponsesStoragePolicy::ProviderStored,
                    ResponsesReplayKind::new("openai.output_item", 2),
                    Some(ResponsesReplayKind::new("openai.opaque_output_item", 1)),
                    Some(ResponsesReplayKind::new("openai.previous_response_id", 1)),
                    now,
                ),
            ),
            (
                "missing previous-response kind",
                admit(
                    ResponsesStoragePolicy::ProviderStored,
                    ResponsesReplayKind::new("openai.output_item", 2),
                    None,
                    None,
                    now,
                ),
            ),
        ] {
            assert!(
                rejected.is_err(),
                "active artifact {axis} drift was admitted"
            );
        }
        Ok(())
    }

    #[test]
    fn active_artifact_turns_one_two_three_and_reopen_use_latest_stored_response() -> TestResult {
        let (protocol, family, model) = target_values()?;
        let replay = ReplayEnvelope::new(
            protocol.clone(),
            Some(family.clone()),
            ReplayScope::ExactModel(model.clone()),
            ReplayKindId::new("openai.output_item")?,
            2,
            DataClassification::Sensitive,
            RawValue::from_string(
                json!({
                    "id": "reasoning-1",
                    "type": "reasoning",
                    "status": "completed",
                    "summary": [],
                    "encrypted_content": "encrypted-1"
                })
                .to_string(),
            )?,
        )?;
        let producing_prefix =
            HistorySegment::new(HistorySegmentKind::RecentIntent, vec![user(49, "start")?])?;
        let producing_request = crate::test_support::model_request_builder(
            ConversationView::from_shared(
                CanonicalHistory::new(vec![producing_prefix.clone()])?,
                WorkingProjection::default(),
            ),
            metadata()?,
        )
        .resolved_harness_plan_identity(harness_plan_identity()?)
        .build()?;
        let output_segment = HistorySegment::new(
            HistorySegmentKind::ReasoningReplay,
            vec![ConversationItem::new(
                id::<EntryId>(50)?,
                Actor::Assistant,
                vec![ContentBlock::Opaque(OpaqueBlock::new(
                    replay.clone(),
                    OpaqueFallback::CompatibleReplay,
                )?)],
                Provenance::ModelOutput,
            )?],
        )?;
        let next_request = crate::test_support::model_request_builder(
            ConversationView::from_shared(
                CanonicalHistory::new(vec![
                    producing_prefix,
                    output_segment,
                    HistorySegment::new(
                        HistorySegmentKind::RecentIntent,
                        vec![user(51, "continue")?],
                    )?,
                ])?,
                WorkingProjection::default(),
            ),
            metadata()?,
        )
        .resolved_harness_plan_identity(harness_plan_identity()?)
        .build()?;
        let now = Timestamp::new(1_700_000_000, 0)?;
        let expires = Timestamp::new(1_800_000_000, 0)?;
        let stored = output_artifact(
            &producing_request,
            &next_request,
            "responses-test-route",
            vec![replay.clone()],
            ResponsesStorageMode::ProviderStored,
            "response-1",
            Some(expires),
            now,
            b"stored-round-trip",
        )?;
        let output_kind = ResponsesReplayKind::new("openai.output_item", 2);
        let previous_kind = ResponsesReplayKind::new("openai.previous_response_id", 1);
        let stored = active_artifact(&stored)?;
        let bridge = ResponsesArtifactReplayBridge::from_active_artifact(
            &stored,
            &next_request,
            operation_authority(
                &next_request,
                "responses-test-route",
                ResponsesStoragePolicy::ProviderStored,
            )?,
            output_kind,
            None,
            Some(previous_kind),
            now,
        )?;
        let lowered = lower_responses_request(
            &next_request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_platform(
                    ResponsesStoragePolicy::ProviderStored,
                    false,
                    false,
                ),
                Some(bridge.context()),
            ),
        )?;
        assert_eq!(lowered.body()["previous_response_id"], "response-1");
        assert_eq!(lowered.body()["input"].as_array().expect("input").len(), 1);
        assert_eq!(lowered.body()["input"][0]["content"][0]["text"], "continue");

        let replay_two = ReplayEnvelope::new(
            protocol.clone(),
            Some(family.clone()),
            ReplayScope::ExactModel(model.clone()),
            ReplayKindId::new("openai.output_item")?,
            2,
            DataClassification::Sensitive,
            RawValue::from_string(
                json!({
                    "id": "reasoning-2",
                    "type": "reasoning",
                    "status": "completed",
                    "summary": [],
                    "encrypted_content": "encrypted-2"
                })
                .to_string(),
            )?,
        )?;
        let mut third_segments = next_request.input().history().segments().to_vec();
        third_segments.push(HistorySegment::new(
            HistorySegmentKind::ReasoningReplay,
            vec![ConversationItem::new(
                id::<EntryId>(52)?,
                Actor::Assistant,
                vec![ContentBlock::Opaque(OpaqueBlock::new(
                    replay_two.clone(),
                    OpaqueFallback::CompatibleReplay,
                )?)],
                Provenance::ModelOutput,
            )?],
        )?);
        third_segments.push(HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![user(53, "third turn")?],
        )?);
        let third_request = crate::test_support::model_request_builder(
            ConversationView::from_shared(
                CanonicalHistory::new(third_segments)?,
                WorkingProjection::default(),
            ),
            metadata()?,
        )
        .resolved_harness_plan_identity(harness_plan_identity()?)
        .build()?;
        let latest = output_artifact(
            &next_request,
            &third_request,
            "responses-test-route",
            vec![replay_two],
            ResponsesStorageMode::ProviderStored,
            "response-2",
            Some(expires),
            now,
            b"stored-third-turn",
        )?;
        let reopened: ActiveFamilyArtifact =
            serde_json::from_value(serde_json::to_value(active_artifact(&latest)?)?)?;
        let reopened_bridge = ResponsesArtifactReplayBridge::from_active_artifact(
            &reopened,
            &third_request,
            operation_authority(
                &third_request,
                "responses-test-route",
                ResponsesStoragePolicy::ProviderStored,
            )?,
            output_kind,
            None,
            Some(previous_kind),
            now,
        )?;
        let third = lower_responses_request(
            &third_request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_platform(
                    ResponsesStoragePolicy::ProviderStored,
                    false,
                    false,
                ),
                Some(reopened_bridge.context()),
            ),
        )?;
        assert_eq!(third.body()["previous_response_id"], "response-2");
        assert_eq!(third.body()["input"].as_array().expect("input").len(), 1);
        assert_eq!(third.body()["input"][0]["content"][0]["text"], "third turn");

        assert!(matches!(
            ResponsesArtifactReplayBridge::from_active_artifact(
                &stored,
                &next_request,
                operation_authority(
                    &next_request,
                    "responses-test-route",
                    ResponsesStoragePolicy::ProviderStored,
                )?,
                output_kind,
                None,
                Some(previous_kind),
                expires,
            ),
            Err(ResponsesRequestError::Replay(
                ResponsesReplayError::ArtifactExpired
            ))
        ));

        let stateless = output_artifact(
            &producing_request,
            &next_request,
            "responses-test-route",
            vec![replay],
            ResponsesStorageMode::Stateless,
            "unused-stateless-response",
            Some(expires),
            now,
            b"stateless-round-trip",
        )?;
        let stateless = active_artifact(&stateless)?;
        let stateless_bridge = ResponsesArtifactReplayBridge::from_active_artifact(
            &stateless,
            &next_request,
            operation_authority(
                &next_request,
                "responses-test-route",
                ResponsesStoragePolicy::Stateless,
            )?,
            output_kind,
            None,
            None,
            now,
        )?;
        let replayed = lower_responses_request(
            &next_request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
                Some(stateless_bridge.context()),
            ),
        )?;
        assert_eq!(replayed.body()["input"][1]["id"], "reasoning-1");
        assert_eq!(replayed.body()["input"][1]["type"], "reasoning");

        let foreign = operation_authority(
            &next_request,
            "foreign-route",
            ResponsesStoragePolicy::Stateless,
        )?;
        assert!(matches!(
            ResponsesArtifactReplayBridge::from_active_artifact(
                &stateless,
                &next_request,
                foreign,
                output_kind,
                None,
                None,
                now,
            ),
            Err(ResponsesRequestError::Replay(
                ResponsesReplayError::ArtifactTargetMismatch { axis: "route" }
            ))
        ));

        // The operation-budget wrap-up withdraws tool selection over the same
        // history. The binding leaves tool selection out of its prefix
        // identity, so the wrap-up still continues this artifact.
        let wrap_up_request =
            crate::test_support::model_request_builder(next_request.input().clone(), metadata()?)
                .resolved_harness_plan_identity(harness_plan_identity()?)
                .tool_choice(ToolChoice::None)
                .build()?;
        let wrap_up = ResponsesArtifactReplayBridge::from_active_artifact(
            &stateless,
            &wrap_up_request,
            operation_authority(
                &wrap_up_request,
                "responses-test-route",
                ResponsesStoragePolicy::Stateless,
            )?,
            output_kind,
            None,
            None,
            now,
        )?;
        let replayed_wrap_up = lower_responses_request(
            &wrap_up_request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
                Some(wrap_up.context()),
            ),
        )?;
        assert_eq!(replayed_wrap_up.body()["input"][1]["id"], "reasoning-1");
        assert!(replayed_wrap_up.body().to_string().contains("encrypted-1"));
        // Any other stable-prefix difference is not this artifact's
        // continuation: the route prepares fresh and still lowers the complete
        // history.
        let drifted_request =
            crate::test_support::model_request_builder(next_request.input().clone(), metadata()?)
                .resolved_harness_plan_identity(harness_plan_identity()?)
                .instructions(vec![InstructionBlock::new(
                    InstructionSource::System,
                    "answer briefly",
                )?])
                .tool_choice(ToolChoice::None)
                .build()?;
        let drifted = ResponsesArtifactReplayBridge::from_active_artifact(
            &stateless,
            &drifted_request,
            operation_authority(
                &drifted_request,
                "responses-test-route",
                ResponsesStoragePolicy::Stateless,
            )?,
            output_kind,
            None,
            None,
            now,
        );
        assert!(matches!(
            drifted,
            Err(ResponsesRequestError::Replay(
                ResponsesReplayError::ArtifactStablePrefixMismatch
            ))
        ));
        assert!(drifted.is_err_and(|error| error.artifact_is_not_for_this_request()));
        // Fresh lowering of that history as the canonicalizer stores it
        // (native reasoning is `Omit`) sends every portable item and no
        // native reasoning.
        let segments = wrap_up_request.input().history().segments();
        let ContentBlock::Opaque(reasoning) = &segments[1].items()[0].blocks()[0] else {
            return Err("fixture reasoning is not opaque".into());
        };
        let stored_reasoning = HistorySegment::new(
            HistorySegmentKind::ReasoningReplay,
            vec![ConversationItem::new(
                id::<EntryId>(50)?,
                Actor::Assistant,
                vec![ContentBlock::Opaque(OpaqueBlock::new(
                    reasoning.replay().clone(),
                    OpaqueFallback::Omit,
                )?)],
                Provenance::ModelOutput,
            )?],
        )?;
        let stored_wrap_up = crate::test_support::model_request_builder(
            ConversationView::from_shared(
                CanonicalHistory::new(vec![
                    segments[0].clone(),
                    stored_reasoning,
                    segments[2].clone(),
                ])?,
                WorkingProjection::default(),
            ),
            metadata()?,
        )
        .resolved_harness_plan_identity(harness_plan_identity()?)
        .tool_choice(ToolChoice::None)
        .build()?;
        let fresh = lower_responses_request(
            &stored_wrap_up,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
                None,
            ),
        )?;
        let fresh = fresh.body().to_string();
        assert!(fresh.contains("start") && fresh.contains("continue"));
        assert!(!fresh.contains("encrypted-1"));
        // A hard target axis still wins over the soft prefix miss.
        assert!(matches!(
            ResponsesArtifactReplayBridge::from_active_artifact(
                &stateless,
                &wrap_up_request,
                operation_authority(
                    &wrap_up_request,
                    "foreign-route",
                    ResponsesStoragePolicy::Stateless,
                )?,
                output_kind,
                None,
                None,
                now,
            ),
            Err(ResponsesRequestError::Replay(
                ResponsesReplayError::ArtifactTargetMismatch { axis: "route" }
            ))
        ));
        // Every other hard axis also wins over a tool-free prefix: a foreign
        // tool plan and a different storage mode.
        let foreign_plan = Digest::sha256(
            DigestDomain::ResolvedHarnessPlan,
            1,
            &[b"responses-foreign-tool-plan"],
        )?;
        let foreign_plan_wrap_up =
            crate::test_support::model_request_builder(next_request.input().clone(), metadata()?)
                .resolved_harness_plan_identity(foreign_plan)
                .tool_choice(ToolChoice::None)
                .build()?;
        let foreign_tool_plan = ResponsesArtifactReplayBridge::from_active_artifact(
            &stateless,
            &foreign_plan_wrap_up,
            operation_authority(
                &foreign_plan_wrap_up,
                "responses-test-route",
                ResponsesStoragePolicy::Stateless,
            )?,
            output_kind,
            None,
            None,
            now,
        );
        assert!(
            matches!(
                foreign_tool_plan,
                Err(ResponsesRequestError::Replay(
                    ResponsesReplayError::ArtifactTargetMismatch { axis: "tool-plan" }
                ))
            ),
            "tool-plan must stay hard under a tool-free prefix: {foreign_tool_plan:?}"
        );
        let foreign_storage = ResponsesArtifactReplayBridge::from_active_artifact(
            &stateless,
            &wrap_up_request,
            operation_authority(
                &wrap_up_request,
                "responses-test-route",
                ResponsesStoragePolicy::ProviderStored,
            )?,
            output_kind,
            None,
            None,
            now,
        );
        assert!(
            matches!(
                foreign_storage,
                Err(ResponsesRequestError::Replay(
                    ResponsesReplayError::ArtifactTargetMismatch {
                        axis: "storage-mode"
                    }
                ))
            ),
            "storage-mode must stay hard under a tool-free prefix: {foreign_storage:?}"
        );

        let original = next_request.input().history().segments();
        let malformed_histories = [
            vec![original[0].clone()],
            vec![
                original[0].clone(),
                original[2].clone(),
                original[1].clone(),
            ],
            vec![
                original[1].clone(),
                original[0].clone(),
                original[2].clone(),
            ],
        ];
        for (index, segments) in malformed_histories.into_iter().enumerate() {
            let malformed = crate::test_support::model_request_builder(
                ConversationView::from_shared(
                    CanonicalHistory::new(segments)?,
                    WorkingProjection::default(),
                ),
                metadata()?,
            )
            .resolved_harness_plan_identity(harness_plan_identity()?)
            .build()?;
            let authority = operation_authority(
                &malformed,
                "responses-test-route",
                ResponsesStoragePolicy::Stateless,
            )?;
            assert!(
                ResponsesArtifactReplayBridge::from_active_artifact(
                    &stateless,
                    &malformed,
                    authority,
                    output_kind,
                    None,
                    None,
                    now,
                )
                .is_err(),
                "gap/reorder/truncation mutation {index} was accepted"
            );
        }
        Ok(())
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the vertical test proves the complete project-codec-append-next-request chain in both modes"
    )]
    fn project_codec_append_and_next_request_are_exact_in_both_storage_modes() -> TestResult {
        let (protocol, family, model) = target_values()?;
        let replay_projection = ResponsesReplayProjection::new(
            protocol.clone(),
            Some(family.clone()),
            model.clone(),
            ReplayKindId::new("openai.output_item")?,
            2,
        )?;
        let response = ResponsesResponse::parse(
            &json!({
                "id":"response-vertical","object":"response","status":"completed","model":"gpt-test",
                "output":[{
                    "id":"reasoning-vertical","type":"reasoning","status":"completed","summary":[],
                    "encrypted_content":"encrypted-vertical"
                }]
            }),
            ResponsesUsagePolicy::DeriveTotalWhenOmitted,
            ResponsesResponseLimits::default(),
        )?;
        let prefix =
            HistorySegment::new(HistorySegmentKind::RecentIntent, vec![user(90, "first")?])?;
        let producing_request = crate::test_support::model_request_builder(
            ConversationView::from_shared(
                CanonicalHistory::new(vec![prefix.clone()])?,
                WorkingProjection::default(),
            ),
            metadata()?,
        )
        .resolved_harness_plan_identity(harness_plan_identity()?)
        .build()?;
        let output_kind = ResponsesReplayKind::new("openai.output_item", 2);
        let previous_kind = ResponsesReplayKind::new("openai.previous_response_id", 1);
        let now = Timestamp::new(1_700_000_000, 0)?;
        let provider_request = ResponsesProviderRequestBinding::new(
            Digest::sha256(
                DigestDomain::ProviderOperation,
                1,
                &[b"request-host-project-codec-physical"],
            )?,
            Digest::sha256(
                DigestDomain::ProviderOperation,
                1,
                &[b"request-host-project-codec-terminal"],
            )?,
            None,
        )?;

        for (index, storage) in [
            ResponsesStorageMode::ProviderStored,
            ResponsesStorageMode::Stateless,
        ]
        .into_iter()
        .enumerate()
        {
            let projected = project_terminal_response(
                response.clone(),
                &replay_projection,
                storage,
                provider_request.clone(),
            )?;
            let replay = projected
                .artifact_candidate()
                .and_then(|candidate| candidate.replay().first())
                .map(|item| item.envelope().clone())
                .ok_or_else(|| std::io::Error::other("missing projected replay"))?;
            let canonical = ConversationItem::new(
                id::<EntryId>(91 + u128::try_from(index)?)?,
                Actor::Assistant,
                vec![ContentBlock::Opaque(OpaqueBlock::new(
                    replay,
                    OpaqueFallback::CompatibleReplay,
                )?)],
                Provenance::ModelOutput,
            )?;
            let paired = projected.pair_canonical(&producing_request, vec![canonical])?;
            let (_, _, _, paired_artifact) = paired.into_parts();
            let paired_artifact =
                paired_artifact.ok_or_else(|| std::io::Error::other("missing paired artifact"))?;
            let canonical_output = paired_artifact.canonical_output().to_vec();
            let output_segment =
                HistorySegment::new(HistorySegmentKind::ReasoningReplay, canonical_output)?;
            let next = crate::test_support::model_request_builder(
                ConversationView::from_shared(
                    CanonicalHistory::new(vec![
                        prefix.clone(),
                        output_segment,
                        HistorySegment::new(
                            HistorySegmentKind::RecentIntent,
                            vec![user(93 + u128::try_from(index)?, "next")?],
                        )?,
                    ])?,
                    WorkingProjection::default(),
                ),
                metadata()?,
            )
            .resolved_harness_plan_identity(harness_plan_identity()?)
            .build()?;
            let plan = descriptor("responses-test-route")?
                .validate_request(producing_request.clone(), ValidationMode::Strict)?;
            let covered_projection = next.input().canonical_item_prefix_identity(2)?;
            let sealed = paired_artifact.seal(&plan, covered_projection)?;
            let (
                _canonical_output,
                production,
                replay_contract,
                replay,
                continuation,
                candidate_storage,
            ) = sealed.into_parts();
            assert_eq!(production.output_items(), 1);
            let artifact = ResponsesArtifact::new(
                operation_binding(&producing_request, "responses-test-route")?,
                production,
                replay_contract,
                replay,
                continuation,
                None,
                candidate_storage,
                None,
                now,
            )?;
            let active = active_artifact(&artifact)?;
            let (policy, previous) = if storage == ResponsesStorageMode::ProviderStored {
                (ResponsesStoragePolicy::ProviderStored, Some(previous_kind))
            } else {
                (ResponsesStoragePolicy::Stateless, None)
            };
            let bridge = ResponsesArtifactReplayBridge::from_active_artifact(
                &active,
                &next,
                operation_authority(&next, "responses-test-route", policy)?,
                output_kind,
                None,
                previous,
                now,
            )?;
            let request_policy = if storage == ResponsesStorageMode::ProviderStored {
                ResponsesRequestPolicy::openai_platform(policy, false, false)
            } else {
                ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content")
            };
            let lowered = lower_responses_request(
                &next,
                ResponsesRequestConfig::new(
                    &protocol,
                    Some(&family),
                    &model,
                    request_policy,
                    Some(bridge.context()),
                ),
            )?;
            let input = lowered.body()["input"].as_array().expect("input");
            if storage == ResponsesStorageMode::ProviderStored {
                assert_eq!(input.len(), 1);
                assert_eq!(lowered.body()["previous_response_id"], "response-vertical");
            } else {
                assert_eq!(input.len(), 3);
                assert_eq!(input[1]["id"], "reasoning-vertical");
            }
        }
        Ok(())
    }

    #[test]
    fn latest_artifact_authorizes_full_prefix_and_disambiguates_same_digest_groups() -> TestResult {
        let (protocol, family, model) = target_values()?;
        let now = Timestamp::new(1_700_000_000, 0)?;
        let output_kind = ResponsesReplayKind::new("openai.output_item", 2);
        let raw = |id: &str| -> Result<ReplayEnvelope, Box<dyn std::error::Error>> {
            Ok(ReplayEnvelope::new(
                protocol.clone(),
                Some(family.clone()),
                ReplayScope::ExactModel(model.clone()),
                ReplayKindId::new("openai.output_item")?,
                2,
                DataClassification::Sensitive,
                RawValue::from_string(
                    json!({
                        "id": id,
                        "type": "reasoning",
                        "status":"completed",
                        "summary": [],
                        "encrypted_content": format!("encrypted-{id}")
                    })
                    .to_string(),
                )?,
            )?)
        };
        let replay_one = raw("reasoning-1")?;
        let replay_two = replay_one.clone();
        let prefix =
            HistorySegment::new(HistorySegmentKind::RecentIntent, vec![user(80, "first")?])?;
        let output_one = HistorySegment::new(
            HistorySegmentKind::ReasoningReplay,
            vec![ConversationItem::new(
                id::<EntryId>(81)?,
                Actor::Assistant,
                vec![ContentBlock::Opaque(OpaqueBlock::new(
                    replay_one.clone(),
                    OpaqueFallback::CompatibleReplay,
                )?)],
                Provenance::ModelOutput,
            )?],
        )?;
        let between =
            HistorySegment::new(HistorySegmentKind::RecentIntent, vec![user(82, "second")?])?;
        let output_two = HistorySegment::new(
            HistorySegmentKind::ReasoningReplay,
            vec![ConversationItem::new(
                id::<EntryId>(83)?,
                Actor::Assistant,
                vec![ContentBlock::Opaque(OpaqueBlock::new(
                    replay_two.clone(),
                    OpaqueFallback::CompatibleReplay,
                )?)],
                Provenance::ModelOutput,
            )?],
        )?;
        let after =
            HistorySegment::new(HistorySegmentKind::RecentIntent, vec![user(84, "third")?])?;
        let request_at_two = crate::test_support::model_request_builder(
            ConversationView::from_shared(
                CanonicalHistory::new(vec![prefix.clone(), output_one.clone(), between.clone()])?,
                WorkingProjection::default(),
            ),
            metadata()?,
        )
        .resolved_harness_plan_identity(harness_plan_identity()?)
        .build()?;
        let next_request = crate::test_support::model_request_builder(
            ConversationView::from_shared(
                CanonicalHistory::new(vec![prefix, output_one, between, output_two, after])?,
                WorkingProjection::default(),
            ),
            metadata()?,
        )
        .resolved_harness_plan_identity(harness_plan_identity()?)
        .build()?;
        let artifact_two = output_artifact(
            &request_at_two,
            &next_request,
            "responses-test-route",
            vec![replay_two],
            ResponsesStorageMode::Stateless,
            "unused-second-group",
            None,
            now,
            b"same-digest-group-two",
        )?;
        let encoded_two = ResponsesArtifactCodec::encode(&artifact_two)?;
        let active_two = active_artifact(&artifact_two)?;
        assert!(!String::from_utf8_lossy(&encoded_two).contains("reasoning-1"));
        let bridge = ResponsesArtifactReplayBridge::from_active_artifact(
            &active_two,
            &next_request,
            operation_authority(
                &next_request,
                "responses-test-route",
                ResponsesStoragePolicy::Stateless,
            )?,
            output_kind,
            None,
            None,
            now,
        )?;
        let lowered = lower_responses_request(
            &next_request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
                Some(bridge.context()),
            ),
        )?;
        let input = lowered.body()["input"].as_array().expect("input");
        assert_eq!(input.len(), 5);
        assert_eq!(input[1]["id"], "reasoning-1");
        assert_eq!(input[3]["id"], "reasoning-1");

        let grok = lower_responses_request(
            &next_request,
            ResponsesRequestConfig::new(
                &protocol,
                Some(&family),
                &model,
                ResponsesRequestPolicy::supergrok_stateless(),
                Some(bridge.context()),
            ),
        )?;
        let grok_input = grok.body()["input"].as_array().expect("Grok input");
        assert_eq!(grok_input.len(), 5);
        assert_eq!(grok_input[1]["id"], "reasoning-1");
        assert_eq!(grok_input[3]["id"], "reasoning-1");

        Ok(())
    }

    #[test]
    fn replay_coordinate_construction_rejects_first_bound_crossing() -> TestResult {
        assert_eq!(
            bounded_replay_coordinate_count(0, MAX_REPLAY_PREFIX_BINDINGS)?,
            MAX_REPLAY_PREFIX_BINDINGS
        );
        assert!(matches!(
            bounded_replay_coordinate_count(MAX_REPLAY_PREFIX_BINDINGS, 1),
            Err(ResponsesRequestError::Replay(
                ResponsesReplayError::TooManyPrefixBindings
            ))
        ));
        assert!(matches!(
            bounded_replay_coordinate_count(usize::MAX, 1),
            Err(ResponsesRequestError::Replay(
                ResponsesReplayError::InvalidOutputCoordinates
            ))
        ));
        Ok(())
    }

    #[test]
    fn strict_router_policy_is_closed_and_rejects_unbounded_or_ambient_values() -> TestResult {
        let policy = ResponsesRouterPolicy::strict("anthropic/claude", "deny", true)?;
        assert_eq!(
            policy.wire_value(),
            serde_json::json!({
                "only": ["anthropic/claude"],
                "order": ["anthropic/claude"],
                "allow_fallbacks": false,
                "require_parameters": true,
                "data_collection": "deny",
                "zdr": true,
            })
        );
        for invalid in ["", "auto", "anthropic claude", &"x".repeat(257)] {
            assert!(ResponsesRouterPolicy::strict(invalid, "deny", true).is_err());
        }
        assert!(ResponsesRouterPolicy::strict("anthropic/claude", "Allow", true).is_err());
        assert_eq!(
            policy.execution_identity()?,
            Digest::sha256(
                DigestDomain::ModelBinding,
                1,
                &[
                    b"responses-router-policy-v1",
                    b"anthropic/claude",
                    b"deny",
                    b"zdr",
                ],
            )?
        );
        assert_ne!(
            ResponsesRequestPolicy::openrouter_stateless().execution_identity()?,
            ResponsesRequestPolicy::supergrok_stateless().execution_identity()?
        );
        Ok(())
    }

    #[test]
    fn gateway_managed_router_policy_is_exact_endpoint_neutral_and_identity_bearing() -> TestResult
    {
        let policy = ResponsesRouterPolicy::gateway_managed(None, None)?;
        let wire = policy.wire_value();
        assert_eq!(
            wire,
            serde_json::json!({
                "allow_fallbacks": false,
                "require_parameters": true,
            })
        );
        assert!(wire.get("only").is_none());
        assert!(wire.get("order").is_none());
        assert!(wire.get("endpoint").is_none());
        assert_ne!(
            policy.execution_identity()?,
            ResponsesRouterPolicy::strict("anthropic/claude", "deny", true)?
                .execution_identity()?
        );
        let selected = ResponsesRouterPolicy::gateway_managed(Some("deny".to_owned()), Some(true))?;
        assert_eq!(
            selected.wire_value(),
            serde_json::json!({
                "allow_fallbacks": false,
                "require_parameters": true,
                "data_collection": "deny",
                "zdr": true,
            })
        );
        let selected_false =
            ResponsesRouterPolicy::gateway_managed(Some("deny".to_owned()), Some(false))?;
        let selected_zdr_only = ResponsesRouterPolicy::gateway_managed(None, Some(true))?;
        assert_ne!(policy.execution_identity()?, selected.execution_identity()?);
        assert_ne!(
            selected.execution_identity()?,
            selected_false.execution_identity()?
        );
        assert_ne!(
            policy.execution_identity()?,
            selected_zdr_only.execution_identity()?
        );
        assert!(ResponsesRouterPolicy::gateway_managed(Some("DENY".to_owned()), None).is_err());
        Ok(())
    }
}
