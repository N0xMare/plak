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
    #[allow(
        clippy::too_many_lines,
        reason = "one linear host step whose ordering is the invariant"
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
mod tests;
