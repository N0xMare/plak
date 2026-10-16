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
    /// Top-level `instructions` string (OpenAI Platform default).
    TopLevelInstructions,
    /// First `input[]` item as `{role:"system", type:"message", content:[...]}` (SuperGrok).
    InputSystemMessageFirst,
    /// Codex keeps top-level `tools` and prefixes `input[]` with an identified
    /// developer instruction message instead of a top-level `instructions` string.
    CodexDeveloperInstructions,
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
        let mut policy = Self::openai_platform(ResponsesStoragePolicy::Stateless, true, false);
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
            instruction_placement: InstructionPlacementPolicy::CodexDeveloperInstructions,
            ..policy
        }
    }

    /// Codex Responses Lite keeps client tools and instructions in the replayed input prefix.
    ///
    /// Its wire carries user `input_image` as the Standard wire does; the
    /// catalog's `ImageInput` fact gates each model at admission.
    #[must_use]
    pub const fn openai_codex_lite(encrypted_reasoning_include: &'static str) -> Self {
        let mut policy = Self::openai_codex(true, false, encrypted_reasoning_include);
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
            true,
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
            true,
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
            match self.instruction_placement {
                InstructionPlacementPolicy::TopLevelInstructions
                | InstructionPlacementPolicy::InputSystemMessageFirst => 0,
                InstructionPlacementPolicy::CodexResponsesLite => 1,
                InstructionPlacementPolicy::CodexDeveloperInstructions => 2,
            },
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

mod input;
mod policy;
mod replay;
mod tools;
use input::{ToolResultMatches, derive_tool_calls, lower_item};
#[cfg(test)]
use input::{lower_program_caller, lower_tool_result};
#[cfg(test)]
use policy::validate_tools;
pub use policy::{
    compile_provider_configuration, compile_provider_configuration_with_implementation,
    compile_provider_configuration_with_implementation_and_router_policy,
};
pub(crate) use replay::validate_authority_for_config;
use replay::{
    ReplayAuthority, exact_output_replay, omit_output_only_input_fields, replay_authority,
    replay_output_for_policy, replay_output_item, select_replay, validate_output_replay_sequence,
    validate_replay_context,
};
pub use replay::{
    ResponsesArtifactReplayBridge, ResponsesOutputReplayCoordinate, ResponsesPrefixBinding,
    ResponsesReplayContext, ResponsesReplayKind, ResponsesRequestAuthority,
};
#[cfg(test)]
use replay::{bounded_replay_coordinate_count, operation_authority_digest, prefix_binding_digest};
#[cfg(test)]
use tools::lower_allowed_callers;
pub use tools::{
    canonical_local_tools, is_string_custom_tool_definition,
    is_unprojected_string_custom_tool_definition,
};
use tools::{
    lower_hosted_tool, lower_projected_tool_choice, lower_tool_definitions,
    lowered_local_tool_names,
};

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
    if let Some(projection) = request.tool_projection() {
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
    if let Some(projection) = request.tool_projection() {
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
        request.tool_projection(),
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
    let codex_instructions = matches!(
        config.policy.instruction_placement,
        InstructionPlacementPolicy::CodexDeveloperInstructions
            | InstructionPlacementPolicy::CodexResponsesLite
    );
    // Codex derives prefix item ids from the thread's cache affinity, so
    // retries and reopened sessions keep their identity.
    let affinity_bytes = || {
        config
            .cache_affinity
            .map(Digest::identity_bytes)
            .ok_or(ResponsesRequestOptionError::PromptCacheAffinity)
    };
    let mut prefix = Vec::new();
    if config.policy.is_codex_responses_lite() {
        if matches!(request.reasoning(), ReasoningPolicy::Disabled) {
            return Err(ResponsesRequestOptionError::UnknownReasoningPolicy.into());
        }
        if !request.hosted_tools().is_empty() {
            return Err(ResponsesRequestOptionError::NativeOptions.into());
        }
        affinity_bytes()?;
        if !tools.is_empty() {
            let tool_bytes =
                serde_json::to_vec(&tools).map_err(|_| ResponsesRequestError::Serialization)?;
            let tool_id = plakous_model::sha256_hex(&[
                b"codex-responses-lite-tools-v1",
                &affinity_bytes()?,
                &tool_bytes,
            ]);
            prefix.push(json!({
                "type": "additional_tools",
                "id": format!("at_{}", &tool_id[..32]),
                "role": "developer",
                "tools": [{
                    "type": "namespace",
                    "name": "functions",
                    "description": "",
                    "tools": tools,
                }],
            }));
        }
    }
    if codex_instructions && let Some(instructions) = instruction_text {
        let instruction_id = plakous_model::sha256_hex(&[
            b"codex-responses-lite-instructions-v1",
            &affinity_bytes()?,
            instructions.as_bytes(),
        ]);
        let mut message = message_item("developer", vec![input_text_part(&instructions)]);
        message["id"] = json!(format!("msg_{}", &instruction_id[..32]));
        prefix.push(message);
    }
    if let Some(Value::Array(input)) = body.get_mut("input") {
        input.splice(0..0, prefix);
    }
    if config.policy.is_codex_responses_lite() {
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
        local_tools: lowered_local_tool_names(request.tools(), request.tool_projection()),
        custom_tools,
        replay_mode,
    })
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

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit fixtures use expect/unwrap for clarity"
)]
mod tests;
