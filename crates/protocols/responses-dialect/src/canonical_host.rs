//! Provider-neutral canonical projection for the Responses family.
//!
//! Incremental stream events are observations only: a missing event makes the
//! live projector enter a sticky gap and cannot affect terminal success.  The
//! authoritative terminal [`ModelResponse`] is rebuilt independently from the
//! complete typed aggregate and exact replay candidates.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use plakous_model::{
    Actor, BlockCompleted, BlockStart, BlockStarted, CitationAdded, CitationBlock, CitationRange,
    CitationSource, CitationTarget, CitationUnit, CompactionBlock, ContentBlock, ConversationItem,
    Digest, McpServerId, ModelEvent, ModelId, ModelProviderRequestId, ModelResponse,
    ModelResponseMetadata, OpaqueBlock, OpaqueFallback, OutputLocation, Provenance, ReasoningBlock,
    ReasoningDelta, ReasoningVisibility, RefusalBlock, ResponseStarted, TextBlock, TextDelta,
    TextPhase, ToolCallBlock, ToolCallState, ToolCorrelation, ToolExecutionOwner, ToolInputDelta,
    ToolName, UsageUpdate,
};
use plakous_responses::{
    CanonicalIdSources, FamilyProjectionError, ProjectedBlock, ProjectedItemKind, ProjectorLimits,
    ProjectorPolicy, ReasoningEmitMode, ResponseProjectorCore, ToolArgsFinishPolicy,
};
use serde_json::Value;
use serde_json::value::RawValue;

use crate::{
    ResponsesAnnotation, ResponsesContentPart, ResponsesFamilyStreamEvent, ResponsesOutputItem,
    ResponsesReasoningContentPart, ResponsesReasoningSummaryPart, ResponsesResponseStatus,
    ResponsesStreamEventKind, ResponsesTerminalProjection,
};

const SYNTHETIC_BLOCK_BASE: u32 = u32::MAX;

/// Canonical interpretation of one advertised Responses tool definition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ResponsesCanonicalToolKind {
    /// JSON function arguments.
    Function,
    /// Arbitrary UTF-8 custom-tool input.
    Custom,
}

/// Closed runtime owner data for a canonical Responses tool call.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ResponsesCanonicalToolOwner {
    /// Execute through the local Plakous tool runtime.
    PlakLocal,
    /// Execute through one exact configured MCP server.
    PlakMcp(McpServerId),
}

impl ResponsesCanonicalToolOwner {
    fn canonical(&self) -> ToolExecutionOwner {
        match self {
            Self::PlakLocal => ToolExecutionOwner::PlakLocal,
            Self::PlakMcp(server) => ToolExecutionOwner::PlakMcp {
                server: server.clone(),
            },
        }
    }
}

/// One admitted provider-neutral tool fact used by canonical projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponsesCanonicalTool {
    name: ToolName,
    kind: ResponsesCanonicalToolKind,
    owner: ResponsesCanonicalToolOwner,
}

impl ResponsesCanonicalTool {
    /// Creates one exact tool projection fact.
    #[must_use]
    pub const fn new(
        name: ToolName,
        kind: ResponsesCanonicalToolKind,
        owner: ResponsesCanonicalToolOwner,
    ) -> Self {
        Self { name, kind, owner }
    }

    /// A locally executed JSON function.
    #[must_use]
    pub const fn local_function(name: ToolName) -> Self {
        Self::new(
            name,
            ResponsesCanonicalToolKind::Function,
            ResponsesCanonicalToolOwner::PlakLocal,
        )
    }

    /// A locally executed arbitrary-input custom tool.
    #[must_use]
    pub const fn local_custom(name: ToolName) -> Self {
        Self::new(
            name,
            ResponsesCanonicalToolKind::Custom,
            ResponsesCanonicalToolOwner::PlakLocal,
        )
    }

    /// Model-facing tool name.
    #[must_use]
    pub const fn name(&self) -> &ToolName {
        &self.name
    }

    /// Expected Responses wire call kind.
    #[must_use]
    pub const fn kind(&self) -> ResponsesCanonicalToolKind {
        self.kind
    }

    /// Bound runtime execution owner.
    #[must_use]
    pub const fn owner(&self) -> &ResponsesCanonicalToolOwner {
        &self.owner
    }

    fn execution_owner(&self) -> ToolExecutionOwner {
        self.owner.canonical()
    }
}

/// Closed optional canonical surfaces for one admitted Responses dialect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResponsesCanonicalPolicy {
    enabled: u8,
}

impl ResponsesCanonicalPolicy {
    const CUSTOM_TOOLS: u8 = 1;
    const CITATIONS: u8 = 1 << 1;
    const RETAIN_UNKNOWN: u8 = 1 << 2;
    const PROGRAMMATIC_CALLERS: u8 = 1 << 3;
    const NATIVE_COMPACTION: u8 = 1 << 4;
    const CODEX_LITE_NAMESPACE: u8 = 1 << 5;
    const GROK_LENGTH_TEXT: u8 = 1 << 6;

    /// Codex Responses semantics: custom tools, citations, and bounded unknowns.
    #[must_use]
    pub const fn codex() -> Self {
        Self {
            enabled: Self::CUSTOM_TOOLS | Self::CITATIONS | Self::RETAIN_UNKNOWN,
        }
    }

    /// Only stable text, refusal, reasoning, and JSON function calls.
    #[must_use]
    pub const fn minimal() -> Self {
        Self { enabled: 0 }
    }

    /// Grok Responses semantics: JSON functions and bounded unknowns only.
    #[must_use]
    pub const fn grok() -> Self {
        Self {
            enabled: Self::RETAIN_UNKNOWN | Self::GROK_LENGTH_TEXT,
        }
    }

    /// xAI Platform Responses semantics: provider-neutral bounded unknowns.
    ///
    /// This grants no xAI-specific response-item discriminator authority.
    #[must_use]
    pub const fn xai_platform() -> Self {
        Self {
            enabled: Self::RETAIN_UNKNOWN,
        }
    }

    /// OpenAI Platform Responses semantics, including hosted program callers.
    #[must_use]
    pub const fn openai_platform() -> Self {
        Self {
            enabled: Self::CUSTOM_TOOLS
                | Self::CITATIONS
                | Self::RETAIN_UNKNOWN
                | Self::PROGRAMMATIC_CALLERS,
        }
    }

    /// OpenRouter Responses semantics: JSON functions and bounded unknowns.
    #[must_use]
    pub const fn openrouter() -> Self {
        Self {
            enabled: Self::RETAIN_UNKNOWN,
        }
    }

    const fn has(self, capability: u8) -> bool {
        self.enabled & capability != 0
    }

    pub(crate) const fn allows_grok_length_text(self) -> bool {
        self.has(Self::GROK_LENGTH_TEXT)
    }

    const fn with_native_compaction(self) -> Self {
        Self {
            enabled: self.enabled | Self::NATIVE_COMPACTION,
        }
    }

    pub(crate) const fn with_codex_lite_namespace(self) -> Self {
        Self {
            enabled: self.enabled | Self::CODEX_LITE_NAMESPACE,
        }
    }
}

/// Immutable provider-neutral facts for Responses-to-canonical projection.
#[derive(Clone, Debug)]
pub struct ResponsesCanonicalDialectData {
    expected_model: ModelId,
    model_binding: plakous_responses::ResponseModelBinding,
    policy: ResponsesCanonicalPolicy,
    limits: ProjectorLimits,
    max_unknown_event_bytes: usize,
    tools: BTreeMap<ToolName, ResponsesCanonicalTool>,
    router_provider: Option<String>,
    native_compaction_binding: Option<plakous_model::CompactionReplayBinding>,
}

impl ResponsesCanonicalDialectData {
    /// Validates exact model, surface, bound, and tool facts.
    ///
    /// # Errors
    ///
    /// Fails closed for zero bounds, duplicate tools, or custom tools without
    /// the corresponding admitted canonical surface.
    pub fn new(
        expected_model: ModelId,
        policy: ResponsesCanonicalPolicy,
        limits: ProjectorLimits,
        max_unknown_event_bytes: usize,
        tools: impl IntoIterator<Item = ResponsesCanonicalTool>,
    ) -> Result<Self, ResponsesCanonicalProjectError> {
        if limits.max_events == 0
            || limits.max_items == 0
            || limits.max_blocks == 0
            || limits.max_accumulated_bytes == 0
            || max_unknown_event_bytes == 0
            || max_unknown_event_bytes > limits.max_accumulated_bytes
        {
            return Err(ResponsesCanonicalProjectError::InvalidConfiguration(
                "canonical projection limits",
            ));
        }
        let mut admitted = BTreeMap::new();
        for tool in tools {
            if tool.kind == ResponsesCanonicalToolKind::Custom
                && !policy.has(ResponsesCanonicalPolicy::CUSTOM_TOOLS)
            {
                return Err(ResponsesCanonicalProjectError::InvalidConfiguration(
                    "custom tool without admitted custom-tool surface",
                ));
            }
            if admitted.insert(tool.name.clone(), tool).is_some() {
                return Err(ResponsesCanonicalProjectError::InvalidConfiguration(
                    "duplicate canonical tool name",
                ));
            }
        }
        Ok(Self {
            expected_model,
            model_binding: plakous_responses::ResponseModelBinding::ExactRequest,
            policy,
            limits,
            max_unknown_event_bytes,
            tools: admitted,
            router_provider: None,
            native_compaction_binding: None,
        })
    }

    pub(crate) fn validate_tool_owner_projection(
        &self,
        request: &plakous_model::ModelRequest,
    ) -> Result<(), ResponsesCanonicalProjectError> {
        for configured in request.tools() {
            let projected = if let Some(projection) = request.tool_projection_v2() {
                projection
                    .definitions()
                    .iter()
                    .filter(|entry| entry.covers_canonical_name(configured.definition().name()))
                    .map(plakous_model::ProjectedToolDefinitionV2::exposed_name)
                    .collect::<Vec<_>>()
            } else {
                vec![configured.definition().name()]
            };
            if projected.is_empty() {
                continue;
            }
            for name in projected {
                let tool = self.tools.get(name).ok_or(
                    ResponsesCanonicalProjectError::InvalidConfiguration(
                        "canonical tool owner projection is incomplete",
                    ),
                )?;
                configured
                    .validate_owner_projection(&tool.execution_owner())
                    .map_err(|_| {
                        ResponsesCanonicalProjectError::InvalidConfiguration(
                            "canonical tool owner projection is not executable",
                        )
                    })?;
            }
        }
        Ok(())
    }

    /// Codex projection facts with default hard bounds.
    ///
    /// # Errors
    ///
    /// Fails closed for duplicate tool names.
    pub fn codex(
        expected_model: ModelId,
        tools: impl IntoIterator<Item = ResponsesCanonicalTool>,
    ) -> Result<Self, ResponsesCanonicalProjectError> {
        let mut data = Self::new(
            expected_model,
            ResponsesCanonicalPolicy::codex(),
            ProjectorLimits::default(),
            1024 * 1024,
            tools,
        )?;
        // ChatGPT Codex OAuth may echo a snapshot or routed backend id.
        data.model_binding = plakous_responses::ResponseModelBinding::ProviderReported;
        Ok(data)
    }

    /// Admits Codex OAuth remote-compact output items for this prepared request.
    #[must_use]
    pub fn with_native_compaction(
        mut self,
        binding: plakous_model::CompactionReplayBinding,
    ) -> Self {
        self.policy = self.policy.with_native_compaction();
        self.native_compaction_binding = Some(binding);
        self
    }

    /// Admits only the `functions` namespace emitted for local Codex Lite tools.
    #[must_use]
    pub fn with_codex_lite_namespace(mut self) -> Self {
        self.policy = self.policy.with_codex_lite_namespace();
        self
    }

    /// Grok projection facts with default hard bounds.
    ///
    /// # Errors
    ///
    /// Fails closed for duplicate tool names or any custom-tool declaration.
    pub fn grok(
        expected_model: ModelId,
        tools: impl IntoIterator<Item = ResponsesCanonicalTool>,
    ) -> Result<Self, ResponsesCanonicalProjectError> {
        let mut data = Self::new(
            expected_model,
            ResponsesCanonicalPolicy::grok(),
            ProjectorLimits::default(),
            1024 * 1024,
            tools,
        )?;
        data.model_binding = plakous_responses::ResponseModelBinding::ProviderReported;
        Ok(data)
    }

    /// xAI Platform bounded-unknown projection facts with default bounds.
    ///
    /// # Errors
    ///
    /// Fails closed if the exact empty-tool projection cannot be constructed.
    pub fn xai_platform(expected_model: ModelId) -> Result<Self, ResponsesCanonicalProjectError> {
        let mut data = Self::new(
            expected_model,
            ResponsesCanonicalPolicy::xai_platform(),
            ProjectorLimits::default(),
            1024 * 1024,
            [],
        )?;
        data.model_binding = plakous_responses::ResponseModelBinding::ProviderReported;
        Ok(data)
    }

    /// OpenAI Platform projection facts with hosted programmatic caller support.
    ///
    /// # Errors
    ///
    /// Fails closed for duplicate tool names.
    pub fn openai_platform(
        expected_model: ModelId,
        tools: impl IntoIterator<Item = ResponsesCanonicalTool>,
    ) -> Result<Self, ResponsesCanonicalProjectError> {
        Self::new(
            expected_model,
            ResponsesCanonicalPolicy::openai_platform(),
            ProjectorLimits::default(),
            1024 * 1024,
            tools,
        )
    }

    /// OpenRouter projection facts with default hard bounds.
    ///
    /// # Errors
    ///
    /// Fails closed for duplicate tool names or any custom-tool declaration.
    pub fn openrouter(
        expected_model: ModelId,
        tools: impl IntoIterator<Item = ResponsesCanonicalTool>,
    ) -> Result<Self, ResponsesCanonicalProjectError> {
        Self::new(
            expected_model,
            ResponsesCanonicalPolicy::openrouter(),
            ProjectorLimits::default(),
            1024 * 1024,
            tools,
        )
    }

    /// Requires the terminal's OpenRouter routing proof to name one exact provider.
    ///
    /// # Errors
    ///
    /// Rejects empty, oversized, or non-canonical provider identities.
    pub fn with_router_provider(
        mut self,
        provider: impl Into<String>,
    ) -> Result<Self, ResponsesCanonicalProjectError> {
        let provider = provider.into();
        if provider.is_empty()
            || provider.len() > 256
            || !provider.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b' ')
            })
        {
            return Err(ResponsesCanonicalProjectError::InvalidConfiguration(
                "router provider identity",
            ));
        }
        self.router_provider = Some(provider);
        Ok(self)
    }

    /// Exact expected provider-reported model.
    #[must_use]
    pub const fn expected_model(&self) -> &ModelId {
        &self.expected_model
    }

    /// Returns how provider-reported response model identity is bound.
    #[must_use]
    pub const fn model_binding(&self) -> plakous_responses::ResponseModelBinding {
        self.model_binding
    }

    /// Pinned optional canonical surfaces.
    #[must_use]
    pub const fn policy(&self) -> ResponsesCanonicalPolicy {
        self.policy
    }

    /// Hard projection bounds.
    #[must_use]
    pub const fn limits(&self) -> ProjectorLimits {
        self.limits
    }

    /// Admitted tool facts by canonical name.
    #[must_use]
    pub const fn tools(&self) -> &BTreeMap<ToolName, ResponsesCanonicalTool> {
        &self.tools
    }
}

/// Provider-neutral failure from authoritative canonical terminal projection.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ResponsesCanonicalProjectError {
    /// Immutable dialect/projector data was invalid.
    #[error("invalid Responses canonical projection configuration: {0}")]
    InvalidConfiguration(&'static str),
    /// A complete terminal aggregate was inconsistent or unsupported.
    #[error("invalid Responses canonical terminal: {0}")]
    InvalidTerminal(&'static str),
    /// A provider surface was not admitted for canonical semantics.
    #[error("unsupported Responses canonical surface: {0}")]
    Unsupported(&'static str),
    /// A configured resource bound was exceeded.
    #[error("Responses canonical projection exceeded {0}")]
    ResourceLimit(&'static str),
    /// Shared incremental projection rejected a structural transition.
    #[error(transparent)]
    Family(#[from] FamilyProjectionError),
    /// Canonical content validation failed.
    #[error("Responses canonical content is invalid")]
    Content(#[from] plakous_model::ContentError),
    /// Canonical identity generation failed.
    #[error("Responses canonical identity generation failed")]
    Identity(#[from] plakous_model::IdentityError),
    /// Bounded opaque event serialization failed.
    #[error("Responses canonical opaque serialization failed")]
    Json(#[from] serde_json::Error),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ObservationItemKind {
    Message { phase: Option<TextPhase> },
    Reasoning,
    Function,
    Custom,
    Opaque,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct HostedProgramFact {
    item_id: String,
    fingerprint: String,
}

/// Exact live OpenAI hosted-program state inherited from validated replay.
///
/// A program remains live until its one terminal `program_output`.  The native
/// item ID and fingerprint are retained even though later caller/output items
/// name only the program call ID; this prevents a loose caller string from
/// becoming continuation authority.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ResponsesHostedProgramContext {
    live: BTreeMap<String, HostedProgramFact>,
    seen: BTreeSet<String>,
}

impl ResponsesHostedProgramContext {
    #[cfg(test)]
    pub(crate) fn test_openai_platform_live_program(
        item_id: &str,
        call_id: &str,
        fingerprint: &str,
    ) -> Result<Self, ResponsesCanonicalProjectError> {
        let mut context = Self::default();
        context.admit_program(&serde_json::json!({
            "type": "program",
            "id": item_id,
            "call_id": call_id,
            "code": "test",
            "fingerprint": fingerprint,
        }))?;
        Ok(context)
    }

    /// Returns the exact native item ID and fingerprint for one live call ID.
    #[must_use]
    pub fn live_program(&self, call_id: &str) -> Option<(&str, &str)> {
        self.live
            .get(call_id)
            .map(|fact| (fact.item_id.as_str(), fact.fingerprint.as_str()))
    }

    /// Returns the number of exact programs still awaiting terminal output.
    #[must_use]
    pub fn live_program_count(&self) -> usize {
        self.live.len()
    }

    fn admit_program(&mut self, native: &Value) -> Result<(), ResponsesCanonicalProjectError> {
        let item_id = required_hosted_string(native, "program id")?;
        let call_id = required_hosted_string(native, "program call_id")?;
        let fingerprint = required_hosted_string(native, "program fingerprint")?;
        if !self.seen.insert(call_id.to_owned())
            || self
                .live
                .insert(
                    call_id.to_owned(),
                    HostedProgramFact {
                        item_id: item_id.to_owned(),
                        fingerprint: fingerprint.to_owned(),
                    },
                )
                .is_some()
        {
            return Err(ResponsesCanonicalProjectError::InvalidTerminal(
                "program call_id was admitted more than once",
            ));
        }
        Ok(())
    }

    fn require_caller(&self, caller_id: &str) -> Result<(), ResponsesCanonicalProjectError> {
        if !self.live.contains_key(caller_id) {
            return Err(ResponsesCanonicalProjectError::InvalidTerminal(
                "program caller does not name a prior live program",
            ));
        }
        Ok(())
    }

    fn complete_program(&mut self, native: &Value) -> Result<(), ResponsesCanonicalProjectError> {
        let call_id = required_hosted_string(native, "program_output call_id")?;
        if self.live.remove(call_id).is_none() {
            return Err(ResponsesCanonicalProjectError::InvalidTerminal(
                "program_output does not name a prior live program",
            ));
        }
        Ok(())
    }

    pub(crate) fn admit_replayed_output(
        &mut self,
        item: &ResponsesOutputItem,
        policy: ResponsesCanonicalPolicy,
    ) -> Result<(), ResponsesCanonicalProjectError> {
        self.apply_output(item, policy)
    }

    fn apply_output(
        &mut self,
        item: &ResponsesOutputItem,
        policy: ResponsesCanonicalPolicy,
    ) -> Result<(), ResponsesCanonicalProjectError> {
        match item {
            ResponsesOutputItem::FunctionCall { native, .. }
            | ResponsesOutputItem::CustomToolCall { native, .. } => {
                let lineage = tool_caller_lineage(native, policy)?;
                if let Some(caller_id) = lineage.provider_programmatic_execution_id() {
                    self.require_caller(caller_id)?;
                }
            }
            ResponsesOutputItem::Opaque { kind, native } if kind == "program" => {
                self.admit_program(native)?;
            }
            ResponsesOutputItem::Opaque { kind, native } if kind == "program_output" => {
                self.complete_program(native)?;
            }
            _ => {}
        }
        Ok(())
    }
}

fn required_hosted_string<'a>(
    native: &'a Value,
    field: &'static str,
) -> Result<&'a str, ResponsesCanonicalProjectError> {
    let wire_name = field.rsplit_once(' ').map_or(field, |(_, name)| name);
    native
        .get(wire_name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(ResponsesCanonicalProjectError::InvalidTerminal(field))
}

/// Best-effort incremental Responses observation projector.
///
/// This value is deliberately not accepted by [`project_canonical_terminal`].
/// Any missing prerequisite enters a sticky gap; later observations are
/// ignored and terminal authority remains unaffected.
pub struct ResponsesCanonicalObservationProjector {
    data: ResponsesCanonicalDialectData,
    plan_digest: Digest,
    provider_request_id: Option<ModelProviderRequestId>,
    core: ResponseProjectorCore,
    items: BTreeMap<u32, ObservationItemKind>,
    hosted_programs: ResponsesHostedProgramContext,
    tool_callers: BTreeMap<u32, plakous_model::ToolCallerLineage>,
    synthetic_blocks: u32,
    response_model: Option<ModelId>,
    gapped: bool,
}

impl ResponsesCanonicalObservationProjector {
    /// Creates an observation-only projector with independent identity sources.
    #[must_use]
    pub fn new(
        data: ResponsesCanonicalDialectData,
        plan_digest: Digest,
        provider_request_id: Option<ModelProviderRequestId>,
        ids: CanonicalIdSources,
    ) -> Self {
        Self::new_with_hosted_programs(
            data,
            plan_digest,
            provider_request_id,
            ids,
            ResponsesHostedProgramContext::default(),
        )
    }

    /// Creates an observation projector with exact validated prior program state.
    #[must_use]
    pub fn new_with_hosted_programs(
        data: ResponsesCanonicalDialectData,
        plan_digest: Digest,
        provider_request_id: Option<ModelProviderRequestId>,
        ids: CanonicalIdSources,
        hosted_programs: ResponsesHostedProgramContext,
    ) -> Self {
        let policy = ProjectorPolicy {
            tool_args: ToolArgsFinishPolicy::SoftSeedEmptyOnly,
            reasoning_emit: ReasoningEmitMode::Live,
        };
        let core = ResponseProjectorCore::new_with_policy(ids, data.limits, policy);
        Self {
            data,
            plan_digest,
            provider_request_id,
            core,
            items: BTreeMap::new(),
            hosted_programs,
            tool_callers: BTreeMap::new(),
            synthetic_blocks: 0,
            response_model: None,
            gapped: false,
        }
    }

    /// Projects one delivered family observation.
    ///
    /// A missing earlier observation or any other projection disagreement
    /// produces no error and makes this projector sticky-gapped. This method
    /// therefore cannot turn receiver saturation into a model failure.
    pub fn project(&mut self, event: ResponsesFamilyStreamEvent) -> Vec<ModelEvent> {
        if self.gapped {
            return Vec::new();
        }
        let mut output = Vec::new();
        if self.try_project(event, &mut output).is_err() {
            self.gapped = true;
            self.core.mark_failed();
            output.clear();
        }
        output
    }

    /// Whether at least one observation prerequisite was missing or invalid.
    #[must_use]
    pub const fn gapped(&self) -> bool {
        self.gapped
    }

    fn try_project(
        &mut self,
        event: ResponsesFamilyStreamEvent,
        output: &mut Vec<ModelEvent>,
    ) -> Result<(), ResponsesCanonicalProjectError> {
        self.core.begin_event()?;
        match event {
            ResponsesStreamEventKind::ResponseCreated { response_id, model } => {
                let response_id =
                    response_id.ok_or(ResponsesCanonicalProjectError::InvalidTerminal(
                        "response.created omitted response identity",
                    ))?;
                let model = model.ok_or(ResponsesCanonicalProjectError::InvalidTerminal(
                    "response.created omitted model identity",
                ))?;
                self.start_response(response_id, &model, output)
            }
            ResponsesStreamEventKind::ResponseInProgress { response_id, model } => {
                if self.core.started() {
                    self.validate_response_identity(&response_id, &model)
                } else {
                    self.start_response(response_id, &model, output)
                }
            }
            ResponsesStreamEventKind::OutputItemAdded { output_index, item } => {
                self.add_item(output_index, &item, output)
            }
            ResponsesStreamEventKind::OutputItemDone { output_index, item } => {
                self.finish_item(output_index, &item, output)
            }
            ResponsesStreamEventKind::ContentPartAdded {
                output_index,
                content_index,
                part,
            } => self.add_content(output_index, content_index, &part, output),
            ResponsesStreamEventKind::ContentPartDone {
                output_index,
                content_index,
                part,
            } => self
                .core
                .finish_content_validate(output_index, content_index, &part, output)
                .map_err(Into::into),
            ResponsesStreamEventKind::OutputTextDelta {
                output_index,
                content_index,
                delta,
            }
            | ResponsesStreamEventKind::RefusalDelta {
                output_index,
                content_index,
                delta,
            } => self
                .core
                .append_text(output_index, content_index, &delta, output)
                .map_err(Into::into),
            ResponsesStreamEventKind::ReasoningSummaryPartAdded {
                output_index,
                summary_index,
                ..
            } => self
                .core
                .start_reasoning_block(output_index, summary_index, output)
                .map_err(Into::into),
            ResponsesStreamEventKind::ReasoningSummaryTextDelta {
                output_index,
                summary_index,
                delta,
            } => self
                .core
                .append_reasoning(output_index, summary_index, &delta, output)
                .map_err(Into::into),
            ResponsesStreamEventKind::ReasoningTextDelta {
                output_index,
                content_index,
                delta,
                ..
            } => {
                if self
                    .core
                    .block(self.core.location(output_index, content_index)?)
                    .is_err()
                {
                    self.core.start_reasoning_block_with_visibility(
                        output_index,
                        content_index,
                        ReasoningVisibility::Visible,
                        output,
                    )?;
                }
                self.core
                    .append_reasoning(output_index, content_index, &delta, output)
                    .map_err(Into::into)
            }
            ResponsesStreamEventKind::ReasoningSummaryPartDone {
                output_index,
                summary_index,
                part,
            } => {
                let text = part.get("text").and_then(Value::as_str).ok_or(
                    ResponsesCanonicalProjectError::InvalidTerminal(
                        "reasoning summary part omitted text",
                    ),
                )?;
                self.core
                    .finish_reasoning_summary_part(output_index, summary_index, text, None, output)
                    .map_err(Into::into)
            }
            ResponsesStreamEventKind::FunctionCallArgumentsDelta {
                output_index,
                delta,
                ..
            } => self
                .core
                .append_tool(output_index, &delta, output)
                .map_err(Into::into),
            ResponsesStreamEventKind::CustomToolCallInputDelta {
                output_index,
                delta,
                ..
            } => self
                .core
                .append_tool_with_emit(output_index, &delta, false, output)
                .map_err(Into::into),
            ResponsesStreamEventKind::FunctionCallArgumentsDone {
                output_index,
                arguments,
                ..
            } => self
                .core
                .finish_function_arguments(output_index, &arguments, output)
                .map_err(Into::into),
            ResponsesStreamEventKind::CustomToolCallInputDone {
                output_index,
                input,
                ..
            } => self
                .core
                .finish_function_arguments_with(
                    output_index,
                    &input,
                    ToolArgsFinishPolicy::SoftSeedEmptyOnly,
                    false,
                    output,
                )
                .map_err(Into::into),
            ResponsesStreamEventKind::OutputTextAnnotationAdded {
                output_index,
                content_index,
                annotation_index,
                annotation,
            } => self.add_annotation(
                output_index,
                content_index,
                annotation_index,
                &annotation,
                output,
            ),
            ResponsesStreamEventKind::Unknown { kind, native } => {
                self.unknown_event(&kind, &native, output)
            }
            ResponsesStreamEventKind::Opaque { kind } => {
                self.unknown_event(&kind, &serde_json::json!({"type": kind}), output)
            }
            ResponsesStreamEventKind::OutputTextDone { .. }
            | ResponsesStreamEventKind::RefusalDone { .. }
            | ResponsesStreamEventKind::ReasoningSummaryTextDone { .. }
            | ResponsesStreamEventKind::ReasoningTextDone { .. }
            | ResponsesStreamEventKind::ResponseCompleted(_)
            | ResponsesStreamEventKind::ResponseIncomplete(_)
            | ResponsesStreamEventKind::ResponseFailed(_)
            | ResponsesStreamEventKind::Error(_) => Ok(()),
        }
    }

    fn start_response(
        &mut self,
        response_id: String,
        model: &str,
        output: &mut Vec<ModelEvent>,
    ) -> Result<(), ResponsesCanonicalProjectError> {
        if self.data.model_binding == plakous_responses::ResponseModelBinding::ExactRequest
            && model != self.data.expected_model.as_str()
        {
            return Err(ResponsesCanonicalProjectError::InvalidTerminal(
                "stream model differs from admitted model",
            ));
        }
        let reported_model = ModelId::new(model)?;
        let metadata = ModelResponseMetadata::new(
            Some(reported_model.clone()),
            self.provider_request_id.clone(),
        );
        self.response_model = Some(reported_model);
        self.core
            .start_response(response_id, metadata, self.plan_digest, output)
            .map_err(Into::into)
    }

    fn validate_response_identity(
        &self,
        response_id: &str,
        model: &str,
    ) -> Result<(), ResponsesCanonicalProjectError> {
        if self.core.response_id() != Some(response_id)
            || self.response_model.as_ref().map(ModelId::as_str) != Some(model)
        {
            return Err(ResponsesCanonicalProjectError::InvalidTerminal(
                "stream response identity changed",
            ));
        }
        Ok(())
    }

    fn add_item(
        &mut self,
        output_index: u32,
        item: &ResponsesOutputItem,
        output: &mut Vec<ModelEvent>,
    ) -> Result<(), ResponsesCanonicalProjectError> {
        self.core.require_started()?;
        let (kind, projected, provider_id, remote_call_id) = match item {
            ResponsesOutputItem::Message {
                id, role, native, ..
            } => {
                if role != "assistant" {
                    return Err(ResponsesCanonicalProjectError::InvalidTerminal(
                        "message output role is not assistant",
                    ));
                }
                (
                    ObservationItemKind::Message {
                        phase: Some(message_phase(native)?),
                    },
                    ProjectedItemKind::Message,
                    id.clone(),
                    None,
                )
            }
            ResponsesOutputItem::Reasoning { id, .. } => (
                ObservationItemKind::Reasoning,
                ProjectedItemKind::Reasoning,
                id.clone(),
                None,
            ),
            ResponsesOutputItem::FunctionCall { id, call_id, .. } => (
                ObservationItemKind::Function,
                ProjectedItemKind::Function,
                id.clone(),
                Some(call_id.clone()),
            ),
            ResponsesOutputItem::CustomToolCall { id, call_id, .. } => {
                self.require_policy(ResponsesCanonicalPolicy::CUSTOM_TOOLS, "custom tools")?;
                (
                    ObservationItemKind::Custom,
                    ProjectedItemKind::Function,
                    id.clone(),
                    Some(call_id.clone()),
                )
            }
            ResponsesOutputItem::Opaque { kind, native } => {
                validate_opaque_kind(kind, self.data.policy)?;
                self.require_policy(ResponsesCanonicalPolicy::RETAIN_UNKNOWN, "unknown output")?;
                self.admit_hosted_item(kind, native)?;
                (
                    ObservationItemKind::Opaque,
                    ProjectedItemKind::Opaque,
                    item.native()
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    None,
                )
            }
            ResponsesOutputItem::Compaction { id, .. } => (
                ObservationItemKind::Opaque,
                ProjectedItemKind::Opaque,
                id.clone(),
                None,
            ),
        };
        let retained = item.native().to_string().len();
        self.core.reserve_bytes(retained)?;
        let entry =
            self.core
                .register_item(output_index, projected, provider_id, remote_call_id, true)?;
        if self.items.insert(output_index, kind).is_some() {
            return Err(ResponsesCanonicalProjectError::InvalidTerminal(
                "output item was added more than once",
            ));
        }
        match item {
            ResponsesOutputItem::FunctionCall {
                call_id,
                name,
                native,
                ..
            } => self.start_tool(
                output_index,
                entry,
                call_id,
                name,
                ResponsesCanonicalToolKind::Function,
                native,
                true,
                output,
            ),
            ResponsesOutputItem::CustomToolCall {
                call_id,
                name,
                native,
                ..
            } => self.start_tool(
                output_index,
                entry,
                call_id,
                name,
                ResponsesCanonicalToolKind::Custom,
                native,
                false,
                output,
            ),
            _ => Ok(()),
        }
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "wire call identity and observation output are one transition"
    )]
    fn start_tool(
        &mut self,
        output_index: u32,
        entry: plakous_model::EntryId,
        remote_call_id: &str,
        name: &str,
        expected_kind: ResponsesCanonicalToolKind,
        native: &Value,
        emit: bool,
        output: &mut Vec<ModelEvent>,
    ) -> Result<(), ResponsesCanonicalProjectError> {
        let caller_lineage = self.validated_tool_caller(native)?;
        if self
            .tool_callers
            .insert(output_index, caller_lineage.clone())
            .is_some()
        {
            return Err(ResponsesCanonicalProjectError::InvalidTerminal(
                "tool caller was admitted more than once",
            ));
        }
        let name = ToolName::new(name.to_owned())?;
        let (owner, _) = local_or_advertised_tool(&self.data.tools, &name, expected_kind)?;
        require_lite_advertised_tool(native, self.data.policy, &self.data.tools, &name)?;
        let correlation =
            ToolCorrelation::new(Some(remote_call_id.to_owned()), None, caller_lineage)?;
        self.core
            .start_tool_block_with(
                output_index,
                entry,
                correlation,
                name,
                owner,
                ToolCallState::Requested,
                "",
                emit,
                output,
            )
            .map_err(Into::into)
    }

    fn validated_tool_caller(
        &self,
        native: &Value,
    ) -> Result<plakous_model::ToolCallerLineage, ResponsesCanonicalProjectError> {
        let caller_lineage = tool_caller_lineage(native, self.data.policy)?;
        if let Some(caller_id) = caller_lineage.provider_programmatic_execution_id() {
            self.hosted_programs.require_caller(caller_id)?;
        }
        Ok(caller_lineage)
    }

    fn validate_completed_tool_caller(
        &self,
        output_index: u32,
        native: &Value,
    ) -> Result<(), ResponsesCanonicalProjectError> {
        let caller_lineage = self.validated_tool_caller(native)?;
        if self.tool_callers.get(&output_index) != Some(&caller_lineage) {
            return Err(ResponsesCanonicalProjectError::InvalidTerminal(
                "tool caller changed before completion",
            ));
        }
        Ok(())
    }

    fn admit_hosted_item(
        &mut self,
        kind: &str,
        native: &Value,
    ) -> Result<(), ResponsesCanonicalProjectError> {
        match kind {
            "program" => self.hosted_programs.admit_program(native),
            "program_output" => self.hosted_programs.complete_program(native),
            _ => Ok(()),
        }
    }

    fn add_content(
        &mut self,
        output_index: u32,
        content_index: u32,
        part: &Value,
        output: &mut Vec<ModelEvent>,
    ) -> Result<(), ResponsesCanonicalProjectError> {
        self.validate_provider_block_index(content_index)?;
        let Some(ObservationItemKind::Message { phase }) = self.items.get(&output_index).copied()
        else {
            return Err(ResponsesCanonicalProjectError::InvalidTerminal(
                "content part does not target a message",
            ));
        };
        match part.get("type").and_then(Value::as_str) {
            Some("output_text") => self
                .core
                .start_message_content_block(output_index, content_index, phase, false, output)
                .map_err(Into::into),
            Some("refusal") => self
                .core
                .start_message_content_block(output_index, content_index, None, true, output)
                .map_err(Into::into),
            _ => Err(ResponsesCanonicalProjectError::Unsupported(
                "message content part",
            )),
        }
    }

    fn finish_item(
        &mut self,
        output_index: u32,
        item: &ResponsesOutputItem,
        output: &mut Vec<ModelEvent>,
    ) -> Result<(), ResponsesCanonicalProjectError> {
        let observed = self.items.get(&output_index).copied().ok_or(
            ResponsesCanonicalProjectError::InvalidTerminal(
                "output item completed before it was observed",
            ),
        )?;
        let expected = match observed {
            ObservationItemKind::Message { .. } => ProjectedItemKind::Message,
            ObservationItemKind::Reasoning => ProjectedItemKind::Reasoning,
            ObservationItemKind::Function | ObservationItemKind::Custom => {
                ProjectedItemKind::Function
            }
            ObservationItemKind::Opaque => ProjectedItemKind::Opaque,
        };
        let (provider_id, remote_call_id) = output_identity(item);
        let Some(canonical_index) = self.core.prepare_finish_item(
            output_index,
            expected,
            provider_id,
            remote_call_id,
            true,
        )?
        else {
            return Ok(());
        };
        match (observed, item) {
            (
                ObservationItemKind::Function,
                ResponsesOutputItem::FunctionCall {
                    arguments, native, ..
                },
            ) => {
                self.validate_completed_tool_caller(output_index, native)?;
                self.core
                    .finish_function_arguments(output_index, arguments, output)?;
            }
            (
                ObservationItemKind::Custom,
                ResponsesOutputItem::CustomToolCall { input, native, .. },
            ) => {
                self.validate_completed_tool_caller(output_index, native)?;
                self.core.finish_function_arguments_with(
                    output_index,
                    input,
                    ToolArgsFinishPolicy::SoftSeedEmptyOnly,
                    false,
                    output,
                )?;
                let entry = self.core.item(output_index)?.entry_id;
                let block = self.core.materialize_tool_block(output_index)?;
                output.push(ModelEvent::BlockStarted(BlockStarted::new(
                    entry,
                    self.core.location(output_index, 0)?,
                    BlockStart::Complete(block),
                )));
            }
            (ObservationItemKind::Message { .. }, ResponsesOutputItem::Message { content, .. }) => {
                self.validate_message_done(output_index, content, output)?;
            }
            (
                ObservationItemKind::Reasoning,
                ResponsesOutputItem::Reasoning {
                    summary, content, ..
                },
            ) => {
                self.validate_reasoning_done(output_index, summary, content, output)?;
            }
            (ObservationItemKind::Opaque, ResponsesOutputItem::Opaque { .. }) => {}
            _ => {
                return Err(ResponsesCanonicalProjectError::InvalidTerminal(
                    "output item type changed before completion",
                ));
            }
        }
        self.core.mark_item_done(canonical_index)?;
        Ok(())
    }

    fn validate_message_done(
        &mut self,
        output_index: u32,
        content: &[ResponsesContentPart],
        output: &mut Vec<ModelEvent>,
    ) -> Result<(), ResponsesCanonicalProjectError> {
        for (index, part) in content.iter().enumerate() {
            let provider_block = u32::try_from(index)
                .map_err(|_| ResponsesCanonicalProjectError::ResourceLimit("message blocks"))?;
            let expected = match part {
                ResponsesContentPart::OutputText { text, .. } => Some((true, text.as_str())),
                ResponsesContentPart::Refusal { refusal } => Some((false, refusal.as_str())),
                ResponsesContentPart::Opaque { .. } => None,
            };
            let Some((is_text, expected)) = expected else {
                continue;
            };
            let location = self.core.location(output_index, provider_block)?;
            if !ResponseProjectorCore::streamed_text_matches(
                self.core.block(location)?,
                is_text,
                expected,
            ) {
                return Err(ResponsesCanonicalProjectError::InvalidTerminal(
                    "message completion differs from delivered deltas",
                ));
            }
            self.core
                .complete_provider_block(output_index, provider_block, output)?;
        }
        Ok(())
    }

    fn validate_reasoning_done(
        &mut self,
        output_index: u32,
        summary: &[ResponsesReasoningSummaryPart],
        content: &[ResponsesReasoningContentPart],
        output: &mut Vec<ModelEvent>,
    ) -> Result<(), ResponsesCanonicalProjectError> {
        for (index, part) in summary.iter().enumerate() {
            let ResponsesReasoningSummaryPart::SummaryText(text) = part else {
                continue;
            };
            let provider_block = u32::try_from(index)
                .map_err(|_| ResponsesCanonicalProjectError::ResourceLimit("reasoning blocks"))?;
            self.core.finish_reasoning_summary_part(
                output_index,
                provider_block,
                text,
                None,
                output,
            )?;
        }
        for (index, part) in content.iter().enumerate() {
            let ResponsesReasoningContentPart::ReasoningText(text) = part;
            let provider_block = u32::try_from(index)
                .map_err(|_| ResponsesCanonicalProjectError::ResourceLimit("reasoning blocks"))?;
            self.core.finish_reasoning_summary_part(
                output_index,
                provider_block,
                text,
                None,
                output,
            )?;
        }
        Ok(())
    }

    fn add_annotation(
        &mut self,
        output_index: u32,
        content_index: u32,
        annotation_index: u32,
        annotation: &ResponsesAnnotation,
        output: &mut Vec<ModelEvent>,
    ) -> Result<(), ResponsesCanonicalProjectError> {
        self.require_policy(ResponsesCanonicalPolicy::CITATIONS, "citations")?;
        let ResponsesAnnotation::UrlCitation {
            start_index,
            end_index,
            url,
            title,
            native,
        } = annotation
        else {
            return Err(ResponsesCanonicalProjectError::Unsupported(
                "opaque annotation",
            ));
        };
        self.core.reserve_bytes(native.to_string().len())?;
        let target = self.core.location(output_index, content_index)?;
        if !matches!(self.core.block(target)?, ProjectedBlock::Text { .. }) {
            return Err(ResponsesCanonicalProjectError::InvalidTerminal(
                "citation does not target output text",
            ));
        }
        let item = self.core.item(output_index)?;
        let entry_id = item.entry_id;
        let provider_id = item.provider_id.as_deref().unwrap_or("output");
        let annotation_id = format!("{provider_id}:{content_index}:{annotation_index}");
        let source =
            CitationSource::new(url, annotation_id, Some(url.clone()), Some(title.clone()))?;
        let range = CitationRange::new(*start_index, *end_index, CitationUnit::UnicodeScalar)?;
        let block = CitationBlock::new(
            CitationTarget::new(entry_id, target.block_index()),
            source,
            Some(range),
        );
        let provider_block = SYNTHETIC_BLOCK_BASE
            .checked_sub(self.synthetic_blocks)
            .ok_or(ResponsesCanonicalProjectError::ResourceLimit(
                "synthetic citation blocks",
            ))?;
        self.synthetic_blocks = self.synthetic_blocks.saturating_add(1);
        let location = self.core.insert_block(
            output_index,
            provider_block,
            ProjectedBlock::Complete(ContentBlock::Citation(block.clone())),
        )?;
        output.push(ModelEvent::CitationAdded(CitationAdded::new(
            entry_id, location, block,
        )));
        Ok(())
    }

    fn unknown_event(
        &mut self,
        kind: &str,
        native: &Value,
        output: &mut Vec<ModelEvent>,
    ) -> Result<(), ResponsesCanonicalProjectError> {
        self.require_policy(ResponsesCanonicalPolicy::RETAIN_UNKNOWN, "unknown events")?;
        let retained = native.to_string().len();
        if retained > self.data.max_unknown_event_bytes {
            return Err(ResponsesCanonicalProjectError::ResourceLimit(
                "unknown event bytes",
            ));
        }
        self.core.reserve_bytes(retained)?;
        let raw = RawValue::from_string(serde_json::to_string(native)?)?;
        output.push(ModelEvent::ProviderEvent(
            plakous_model::ProviderEvent::new(kind, raw),
        ));
        Ok(())
    }

    fn validate_provider_block_index(
        &self,
        index: u32,
    ) -> Result<(), ResponsesCanonicalProjectError> {
        if usize::try_from(index).map_or(true, |index| index >= self.data.limits.max_blocks) {
            Err(ResponsesCanonicalProjectError::ResourceLimit(
                "provider output block index",
            ))
        } else {
            Ok(())
        }
    }

    fn require_policy(
        &self,
        capability: u8,
        surface: &'static str,
    ) -> Result<(), ResponsesCanonicalProjectError> {
        if self.data.policy.has(capability) {
            Ok(())
        } else {
            Err(ResponsesCanonicalProjectError::Unsupported(surface))
        }
    }
}

impl fmt::Debug for ResponsesCanonicalObservationProjector {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponsesCanonicalObservationProjector")
            .field("expected_model", &"<redacted>")
            .field("items", &self.items.len())
            .field("gapped", &self.gapped)
            .finish_non_exhaustive()
    }
}

/// Authoritative terminal canonical aggregate, independent of live delivery.
#[derive(Clone, Debug, PartialEq)]
pub struct ResponsesCanonicalTerminal {
    response: ModelResponse,
}

impl ResponsesCanonicalTerminal {
    /// Exact authoritative canonical response.
    #[must_use]
    pub const fn response(&self) -> &ModelResponse {
        &self.response
    }

    /// Exact canonical output in provider output-item order.
    #[must_use]
    pub fn canonical_output(&self) -> &[ConversationItem] {
        self.response.output()
    }

    /// Emits one complete self-consistent authoritative event sequence.
    ///
    /// The sequence opens the response, reconstructs every exact canonical
    /// block, emits optional usage, and closes with exactly one completion. It
    /// can therefore be consumed directly by `ModelResponseAssembler` even
    /// when every live observation was dropped.
    #[must_use]
    pub fn into_events(self) -> Vec<ModelEvent> {
        let mut events = Vec::new();
        events.push(ModelEvent::ResponseStarted(
            ResponseStarted::new(
                self.response.response_id().map(str::to_owned),
                self.response.plan_digest(),
            )
            .with_metadata(self.response.metadata().clone()),
        ));
        let mut item_index = 0_u32;
        for item in self.response.output() {
            let mut block_index = 0_u32;
            for block in item.blocks() {
                emit_authoritative_block(
                    &mut events,
                    item.id(),
                    OutputLocation::new(item_index, block_index),
                    block,
                );
                block_index = block_index.saturating_add(1);
            }
            item_index = item_index.saturating_add(1);
        }
        if let Some(usage) = self.response.usage() {
            events.push(ModelEvent::Usage(UsageUpdate::new(usage)));
        }
        events.push(ModelEvent::ResponseCompleted(self.response));
        events
    }
}

fn emit_authoritative_block(
    events: &mut Vec<ModelEvent>,
    item_id: plakous_model::EntryId,
    location: OutputLocation,
    block: &ContentBlock,
) {
    match block {
        ContentBlock::Text(text) => {
            events.push(ModelEvent::BlockStarted(BlockStarted::new(
                item_id,
                location,
                BlockStart::Text {
                    phase: text.phase(),
                },
            )));
            if !text.text().is_empty() {
                events.push(ModelEvent::TextDelta(TextDelta::new(
                    location,
                    text.text().as_bytes(),
                )));
            }
            events.push(ModelEvent::BlockCompleted(BlockCompleted::new(location)));
        }
        ContentBlock::Refusal(refusal) => {
            events.push(ModelEvent::BlockStarted(BlockStarted::new(
                item_id,
                location,
                BlockStart::Refusal,
            )));
            if !refusal.text().is_empty() {
                events.push(ModelEvent::TextDelta(TextDelta::new(
                    location,
                    refusal.text().as_bytes(),
                )));
            }
            events.push(ModelEvent::BlockCompleted(BlockCompleted::new(location)));
        }
        ContentBlock::Reasoning(reasoning) => {
            events.push(ModelEvent::BlockStarted(BlockStarted::new(
                item_id,
                location,
                BlockStart::Reasoning {
                    visibility: reasoning.visibility(),
                    replay: reasoning.replay().cloned(),
                },
            )));
            if let Some(text) = reasoning.text() {
                events.push(ModelEvent::ReasoningDelta(ReasoningDelta::new(
                    location,
                    text.as_bytes(),
                )));
            }
            events.push(ModelEvent::BlockCompleted(BlockCompleted::new(location)));
        }
        ContentBlock::ToolCall(call) if call.arguments().is_some() => {
            events.push(ModelEvent::BlockStarted(BlockStarted::new(
                item_id,
                location,
                BlockStart::ToolCall {
                    call_id: call.call_id(),
                    tool: call.exposed_name().clone(),
                    owner: call.owner().clone(),
                    state: call.state(),
                    correlation: call.correlation().clone(),
                },
            )));
            events.push(ModelEvent::ToolInputDelta(ToolInputDelta::new(
                location,
                call.raw_arguments().as_bytes(),
            )));
            events.push(ModelEvent::BlockCompleted(BlockCompleted::new(location)));
        }
        ContentBlock::Citation(citation) => {
            events.push(ModelEvent::CitationAdded(CitationAdded::new(
                item_id,
                location,
                citation.clone(),
            )));
        }
        ContentBlock::Image(_)
        | ContentBlock::Document(_)
        | ContentBlock::Audio(_)
        | ContentBlock::ToolCall(_)
        | ContentBlock::ToolResult(_)
        | ContentBlock::Compaction(_)
        | ContentBlock::Opaque(_)
        | _ => {
            events.push(ModelEvent::BlockStarted(BlockStarted::new(
                item_id,
                location,
                BlockStart::Complete(block.clone()),
            )));
        }
    }
}

/// Builds the authoritative canonical terminal from complete shared data only.
///
/// No live observation projector is accepted by this function.  Dropped,
/// saturated, delayed, or absent family observations therefore cannot affect
/// the returned IDs, output, usage, finish, or terminal success.
///
/// # Errors
///
/// Fails closed for failed/non-terminal responses, model/tool disagreement,
/// incomplete tool calls, replay count/order mismatch, or canonical bounds.
pub fn project_canonical_terminal(
    terminal: &ResponsesTerminalProjection,
    data: &ResponsesCanonicalDialectData,
    plan_digest: Digest,
    ids: &CanonicalIdSources,
) -> Result<ResponsesCanonicalTerminal, ResponsesCanonicalProjectError> {
    project_canonical_terminal_with_hosted_programs(
        terminal,
        data,
        plan_digest,
        ids,
        ResponsesHostedProgramContext::default(),
    )
}

/// Builds the authoritative terminal using exact validated prior hosted-program state.
///
/// # Errors
///
/// In addition to ordinary terminal errors, rejects caller/output references
/// to absent or already completed programs and duplicate program terminals.
pub fn project_canonical_terminal_with_hosted_programs(
    terminal: &ResponsesTerminalProjection,
    data: &ResponsesCanonicalDialectData,
    plan_digest: Digest,
    ids: &CanonicalIdSources,
    mut hosted_programs: ResponsesHostedProgramContext,
) -> Result<ResponsesCanonicalTerminal, ResponsesCanonicalProjectError> {
    let response = terminal.response();
    if data.model_binding == plakous_responses::ResponseModelBinding::ExactRequest
        && response.model() != data.expected_model.as_str()
    {
        return Err(ResponsesCanonicalProjectError::InvalidTerminal(
            "terminal model differs from admitted model",
        ));
    }
    if let Some(provider) = data.router_provider.as_deref() {
        validate_router_metadata(response.native(), data.expected_model.as_str(), provider)?;
    }
    if !matches!(
        response.status(),
        ResponsesResponseStatus::Completed | ResponsesResponseStatus::Incomplete
    ) {
        return Err(ResponsesCanonicalProjectError::InvalidTerminal(
            "terminal status is not canonical success",
        ));
    }
    let finish =
        terminal
            .finish()
            .cloned()
            .ok_or(ResponsesCanonicalProjectError::InvalidTerminal(
                "successful terminal omitted finish facts",
            ))?;
    let artifact =
        terminal
            .artifact_candidate()
            .ok_or(ResponsesCanonicalProjectError::InvalidTerminal(
                "successful terminal omitted exact replay candidates",
            ))?;
    if response.output().len() != artifact.replay().len()
        || artifact.output_count()
            != u32::try_from(response.output().len())
                .map_err(|_| ResponsesCanonicalProjectError::ResourceLimit("output items"))?
    {
        return Err(ResponsesCanonicalProjectError::InvalidTerminal(
            "terminal output and replay counts differ",
        ));
    }
    if response.output().len() > data.limits.max_items {
        return Err(ResponsesCanonicalProjectError::ResourceLimit(
            "output items",
        ));
    }
    validate_terminal_item_statuses(response.status(), response.output())?;
    for item in response.output() {
        hosted_programs.apply_output(item, data.policy)?;
    }

    let mut budget = TerminalBudget::new(data.limits);
    let mut output = Vec::with_capacity(response.output().len());
    for (output_index, (item, replay)) in
        response.output().iter().zip(artifact.replay()).enumerate()
    {
        let entry_id = ids.entry_id()?;
        let mut blocks = terminal_item_blocks(
            item,
            replay.envelope(),
            output_index,
            entry_id,
            data,
            ids,
            &mut budget,
        )?;
        budget.add_block(replay.envelope().payload().get().len())?;
        let blank_message = match item {
            ResponsesOutputItem::Message { content, .. } => content.iter().all(|part| match part {
                ResponsesContentPart::OutputText { text, annotations } => {
                    text.is_empty() && annotations.is_empty()
                }
                ResponsesContentPart::Refusal { refusal } => refusal.is_empty(),
                ResponsesContentPart::Opaque { .. } => false,
            }),
            _ => false,
        };
        let fallback = if matches!(item, ResponsesOutputItem::Reasoning { .. }) || blank_message {
            // Encrypted/native reasoning is valuable only to the producing
            // Responses target. Its visible summary, when present, is already
            // canonical; a foreign model must never be forced to replay or
            // understand the hidden native item. Blank known placeholders can
            // also be omitted; retained unknown parts are never blank.
            OpaqueFallback::Omit
        } else {
            OpaqueFallback::CompatibleReplay
        };
        blocks.push(ContentBlock::Opaque(OpaqueBlock::new(
            replay.envelope().clone(),
            fallback,
        )?));
        output.push(ConversationItem::new(
            entry_id,
            Actor::Assistant,
            blocks,
            Provenance::ModelOutput,
        )?);
    }
    let reported_model = ModelId::new(response.model())?;
    let metadata = ModelResponseMetadata::new(
        Some(reported_model),
        terminal.provider_request().canonical_provider_request_id(),
    );
    let response = ModelResponse::new(
        output,
        finish,
        terminal.usage(),
        Some(response.id().to_owned()),
        plan_digest,
    )
    .with_metadata(metadata);
    Ok(ResponsesCanonicalTerminal { response })
}

fn validate_router_metadata(
    native: &Value,
    model: &str,
    provider: &str,
) -> Result<(), ResponsesCanonicalProjectError> {
    let invalid = || ResponsesCanonicalProjectError::InvalidTerminal("router metadata mismatch");
    let metadata = native
        .as_object()
        .and_then(|object| object.get("openrouter_metadata"))
        .and_then(Value::as_object)
        .ok_or_else(invalid)?;
    if metadata.get("requested").and_then(Value::as_str) != Some(model)
        || metadata.get("strategy").and_then(Value::as_str) != Some("direct")
        || metadata.get("attempt").and_then(Value::as_u64) != Some(1)
        || metadata.get("is_byok").and_then(Value::as_bool) != Some(false)
    {
        return Err(invalid());
    }
    let endpoints = metadata
        .get("endpoints")
        .and_then(Value::as_object)
        .ok_or_else(invalid)?;
    let available = endpoints
        .get("available")
        .and_then(Value::as_array)
        .ok_or_else(invalid)?;
    let [selected] = available.as_slice() else {
        return Err(invalid());
    };
    let selected = selected.as_object().ok_or_else(invalid)?;
    if endpoints.get("total").and_then(Value::as_u64) != Some(1)
        || selected.get("provider").and_then(Value::as_str) != Some(provider)
        || selected.get("model").and_then(Value::as_str) != Some(model)
        || selected.get("selected").and_then(Value::as_bool) != Some(true)
    {
        return Err(invalid());
    }
    if let Some(attempts) = metadata.get("attempts").filter(|value| !value.is_null()) {
        let attempts = attempts.as_array().ok_or_else(invalid)?;
        let [successful] = attempts.as_slice() else {
            return Err(invalid());
        };
        let successful = successful.as_object().ok_or_else(invalid)?;
        if successful.get("provider").and_then(Value::as_str) != Some(provider)
            || successful.get("model").and_then(Value::as_str) != Some(model)
            || successful.get("status").and_then(Value::as_u64) != Some(200)
        {
            return Err(invalid());
        }
    }
    if metadata
        .get("pipeline")
        .filter(|value| !value.is_null())
        .is_some_and(|value| !value.as_array().is_some_and(Vec::is_empty))
        || metadata
            .get("params")
            .filter(|value| !value.is_null())
            .is_some_and(|value| !value.as_object().is_some_and(serde_json::Map::is_empty))
    {
        return Err(invalid());
    }
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "terminal item projection binds exact item/replay/identity/config facts"
)]
fn terminal_item_blocks(
    item: &ResponsesOutputItem,
    _replay: &plakous_model::ReplayEnvelope,
    output_index: usize,
    entry_id: plakous_model::EntryId,
    data: &ResponsesCanonicalDialectData,
    ids: &CanonicalIdSources,
    budget: &mut TerminalBudget,
) -> Result<Vec<ContentBlock>, ResponsesCanonicalProjectError> {
    let mut blocks = Vec::new();
    match item {
        ResponsesOutputItem::Message {
            id,
            role,
            content,
            native,
            ..
        } => {
            if role != "assistant" {
                return Err(ResponsesCanonicalProjectError::InvalidTerminal(
                    "message output role is not assistant",
                ));
            }
            let phase = message_phase(native)?;
            for (content_index, part) in content.iter().enumerate() {
                match part {
                    ResponsesContentPart::OutputText { text, annotations } => {
                        if text.is_empty() && annotations.is_empty() {
                            continue;
                        }
                        budget.add_block(text.len())?;
                        let target_block = u32::try_from(blocks.len()).map_err(|_| {
                            ResponsesCanonicalProjectError::ResourceLimit("message blocks")
                        })?;
                        blocks.push(ContentBlock::Text(TextBlock::new(
                            text.clone(),
                            Some(phase),
                        )?));
                        for (annotation_index, annotation) in annotations.iter().enumerate() {
                            match annotation {
                                ResponsesAnnotation::UrlCitation {
                                    start_index,
                                    end_index,
                                    url,
                                    title,
                                    ..
                                } => {
                                    require_terminal_policy(
                                        data,
                                        ResponsesCanonicalPolicy::CITATIONS,
                                        "citations",
                                    )?;
                                    let scalar_len =
                                        u64::try_from(text.chars().count()).map_err(|_| {
                                            ResponsesCanonicalProjectError::ResourceLimit(
                                                "citation text offsets",
                                            )
                                        })?;
                                    if *end_index > scalar_len {
                                        return Err(
                                            ResponsesCanonicalProjectError::InvalidTerminal(
                                                "citation range exceeds output text",
                                            ),
                                        );
                                    }
                                    budget.add_block(url.len().saturating_add(title.len()))?;
                                    let provider_id = id.as_deref().unwrap_or("output");
                                    let annotation_id = format!(
                                        "{provider_id}:{output_index}:{content_index}:{annotation_index}"
                                    );
                                    let source = CitationSource::new(
                                        url,
                                        annotation_id,
                                        Some(url.clone()),
                                        Some(title.clone()),
                                    )?;
                                    let range = CitationRange::new(
                                        *start_index,
                                        *end_index,
                                        CitationUnit::UnicodeScalar,
                                    )?;
                                    blocks.push(ContentBlock::Citation(CitationBlock::new(
                                        CitationTarget::new(entry_id, target_block),
                                        source,
                                        Some(range),
                                    )));
                                }
                                ResponsesAnnotation::Opaque { .. } => {
                                    require_terminal_policy(
                                        data,
                                        ResponsesCanonicalPolicy::RETAIN_UNKNOWN,
                                        "opaque annotation",
                                    )?;
                                }
                            }
                        }
                    }
                    ResponsesContentPart::Refusal { refusal } => {
                        if refusal.is_empty() {
                            continue;
                        }
                        budget.add_block(refusal.len())?;
                        blocks.push(ContentBlock::Refusal(RefusalBlock::new(refusal.clone())?));
                    }
                    ResponsesContentPart::Opaque { .. } => require_terminal_policy(
                        data,
                        ResponsesCanonicalPolicy::RETAIN_UNKNOWN,
                        "opaque message content",
                    )?,
                }
            }
        }
        ResponsesOutputItem::Reasoning {
            summary, content, ..
        } => {
            for part in summary {
                match part {
                    ResponsesReasoningSummaryPart::SummaryText(text) => {
                        if text.is_empty() {
                            continue;
                        }
                        budget.add_block(text.len())?;
                        blocks.push(ContentBlock::Reasoning(ReasoningBlock::new(
                            ReasoningVisibility::Summary,
                            Some(text.clone()),
                            None,
                        )?));
                    }
                    ResponsesReasoningSummaryPart::Opaque { .. } => require_terminal_policy(
                        data,
                        ResponsesCanonicalPolicy::RETAIN_UNKNOWN,
                        "opaque reasoning summary",
                    )?,
                }
            }
            for part in content {
                let ResponsesReasoningContentPart::ReasoningText(text) = part;
                if text.is_empty() {
                    continue;
                }
                budget.add_block(text.len())?;
                blocks.push(ContentBlock::Reasoning(ReasoningBlock::new(
                    ReasoningVisibility::Visible,
                    Some(text.clone()),
                    None,
                )?));
            }
        }
        ResponsesOutputItem::FunctionCall {
            call_id,
            name,
            arguments,
            native,
            ..
        } => blocks.push(terminal_tool_block(
            call_id,
            name,
            arguments,
            native,
            ResponsesCanonicalToolKind::Function,
            data,
            ids,
            budget,
        )?),
        ResponsesOutputItem::CustomToolCall {
            call_id,
            name,
            input,
            native,
            ..
        } => {
            require_terminal_policy(data, ResponsesCanonicalPolicy::CUSTOM_TOOLS, "custom tools")?;
            blocks.push(terminal_tool_block(
                call_id,
                name,
                input,
                native,
                ResponsesCanonicalToolKind::Custom,
                data,
                ids,
                budget,
            )?);
        }
        ResponsesOutputItem::Opaque { kind, .. } => {
            validate_opaque_kind(kind, data.policy)?;
            require_terminal_policy(
                data,
                ResponsesCanonicalPolicy::RETAIN_UNKNOWN,
                "unknown output",
            )?;
        }
        ResponsesOutputItem::Compaction {
            encrypted_content,
            native,
            ..
        } => {
            require_terminal_policy(
                data,
                ResponsesCanonicalPolicy::NATIVE_COMPACTION,
                "Responses native compaction is not qualified",
            )?;
            budget.add_block(
                encrypted_content
                    .len()
                    .saturating_add(native.to_string().len()),
            )?;
            blocks.push(ContentBlock::Compaction(
                CompactionBlock::new(encrypted_content.clone())?
                    .with_native_item(native.to_string())
                    .with_native_binding(data.native_compaction_binding.clone().ok_or(
                        ResponsesCanonicalProjectError::InvalidConfiguration(
                            "native compaction producing binding",
                        ),
                    )?),
            ));
        }
    }
    Ok(blocks)
}

#[allow(
    clippy::too_many_arguments,
    reason = "tool projection validates every identity-bearing wire fact together"
)]
fn terminal_tool_block(
    remote_call_id: &str,
    name: &str,
    arguments: &str,
    native: &Value,
    expected_kind: ResponsesCanonicalToolKind,
    data: &ResponsesCanonicalDialectData,
    ids: &CanonicalIdSources,
    budget: &mut TerminalBudget,
) -> Result<ContentBlock, ResponsesCanonicalProjectError> {
    let caller_lineage = tool_caller_lineage(native, data.policy)?;
    let name = ToolName::new(name.to_owned())?;
    let (owner, kind) = local_or_advertised_tool(&data.tools, &name, expected_kind)?;
    require_lite_advertised_tool(native, data.policy, &data.tools, &name)?;
    let canonical_arguments = match kind {
        ResponsesCanonicalToolKind::Function => {
            let value = crate::response_host::parse_unique_json_value(arguments).map_err(|_| {
                ResponsesCanonicalProjectError::InvalidTerminal(
                    "function call arguments are not strict JSON",
                )
            })?;
            if !value.is_object() {
                return Err(ResponsesCanonicalProjectError::InvalidTerminal(
                    "function call arguments are not a JSON object",
                ));
            }
            arguments.to_owned()
        }
        ResponsesCanonicalToolKind::Custom => serde_json::to_string(arguments)?,
    };
    budget.add_block(canonical_arguments.len())?;
    let correlation = ToolCorrelation::new(Some(remote_call_id.to_owned()), None, caller_lineage)?;
    Ok(ContentBlock::ToolCall(ToolCallBlock::new(
        ids.call_id()?,
        name,
        owner,
        ToolCallState::Requested,
        correlation,
        canonical_arguments,
    )?))
}

fn local_or_advertised_tool(
    tools: &BTreeMap<ToolName, ResponsesCanonicalTool>,
    name: &ToolName,
    expected_kind: ResponsesCanonicalToolKind,
) -> Result<(ToolExecutionOwner, ResponsesCanonicalToolKind), ResponsesCanonicalProjectError> {
    match tools.get(name) {
        Some(tool) if tool.kind != expected_kind => {
            Err(ResponsesCanonicalProjectError::InvalidTerminal(
                "tool call kind differs from advertised schema",
            ))
        }
        Some(tool) => Ok((tool.owner.canonical(), tool.kind)),
        None => Ok((ToolExecutionOwner::PlakLocal, expected_kind)),
    }
}

fn require_lite_advertised_tool(
    native: &Value,
    policy: ResponsesCanonicalPolicy,
    tools: &BTreeMap<ToolName, ResponsesCanonicalTool>,
    name: &ToolName,
) -> Result<(), ResponsesCanonicalProjectError> {
    if policy.has(ResponsesCanonicalPolicy::CODEX_LITE_NAMESPACE)
        && native
            .get("namespace")
            .is_some_and(|value| !value.is_null())
        && !tools.contains_key(name)
    {
        return Err(ResponsesCanonicalProjectError::InvalidTerminal(
            "namespaced Codex tool was not advertised",
        ));
    }
    Ok(())
}

fn validate_terminal_item_statuses(
    terminal: &ResponsesResponseStatus,
    output: &[ResponsesOutputItem],
) -> Result<(), ResponsesCanonicalProjectError> {
    for item in output {
        let status = crate::output_item_status(item);
        let completed = matches!(status, None | Some("completed" | "done"));
        let incomplete = status == Some("incomplete");
        if matches!(terminal, ResponsesResponseStatus::Completed) && !completed {
            return Err(ResponsesCanonicalProjectError::InvalidTerminal(
                "completed response contains a non-complete item",
            ));
        }
        if matches!(
            item,
            ResponsesOutputItem::FunctionCall { .. } | ResponsesOutputItem::CustomToolCall { .. }
        ) && !completed
        {
            return Err(ResponsesCanonicalProjectError::InvalidTerminal(
                "incomplete output cannot authorize a tool call",
            ));
        }
        if matches!(terminal, ResponsesResponseStatus::Incomplete) && !(completed || incomplete) {
            return Err(ResponsesCanonicalProjectError::InvalidTerminal(
                "incomplete response contains an invalid item status",
            ));
        }
    }
    Ok(())
}

fn output_identity(item: &ResponsesOutputItem) -> (Option<&str>, Option<&str>) {
    match item {
        ResponsesOutputItem::Message { id, .. }
        | ResponsesOutputItem::Reasoning { id, .. }
        | ResponsesOutputItem::Compaction { id, .. } => (id.as_deref(), None),
        ResponsesOutputItem::FunctionCall { id, call_id, .. }
        | ResponsesOutputItem::CustomToolCall { id, call_id, .. } => (id.as_deref(), Some(call_id)),
        ResponsesOutputItem::Opaque { native, .. } => {
            (native.get("id").and_then(Value::as_str), None)
        }
    }
}

fn message_phase(native: &Value) -> Result<TextPhase, ResponsesCanonicalProjectError> {
    match native.get("phase").and_then(Value::as_str) {
        None | Some("final" | "final_answer") => Ok(TextPhase::Final),
        Some("commentary") => Ok(TextPhase::Commentary),
        Some(_) => Err(ResponsesCanonicalProjectError::Unsupported("message phase")),
    }
}

fn tool_caller_lineage(
    native: &Value,
    policy: ResponsesCanonicalPolicy,
) -> Result<plakous_model::ToolCallerLineage, ResponsesCanonicalProjectError> {
    let namespace = native.get("namespace").filter(|value| !value.is_null());
    if native.get("caller_id").is_some()
        || native.get("container_id").is_some()
        || namespace.is_some_and(|value| {
            !policy.has(ResponsesCanonicalPolicy::CODEX_LITE_NAMESPACE)
                || value.as_str() != Some("functions")
        })
    {
        return Err(ResponsesCanonicalProjectError::Unsupported(
            "provider-hosted or namespaced tool call",
        ));
    }
    let Some(caller) = native.get("caller") else {
        return Ok(plakous_model::ToolCallerLineage::Direct);
    };
    if caller.is_null() {
        return Ok(plakous_model::ToolCallerLineage::Direct);
    }
    let Some(object) = caller.as_object() else {
        return Err(ResponsesCanonicalProjectError::InvalidTerminal(
            "tool caller is not an object",
        ));
    };
    if object.len() == 1 && object.get("type").and_then(Value::as_str) == Some("direct") {
        Ok(plakous_model::ToolCallerLineage::Direct)
    } else if object.len() == 2 && object.get("type").and_then(Value::as_str) == Some("program") {
        if !policy.has(ResponsesCanonicalPolicy::PROGRAMMATIC_CALLERS) {
            return Err(ResponsesCanonicalProjectError::Unsupported(
                "provider-programmatic tool caller",
            ));
        }
        let caller_id = object.get("caller_id").and_then(Value::as_str).ok_or(
            ResponsesCanonicalProjectError::InvalidTerminal("program caller lacks caller_id"),
        )?;
        Ok(plakous_model::ToolCallerLineage::provider_programmatic(
            plakous_model::NamespaceId::new("openai.responses.program")?,
            caller_id,
        )?)
    } else {
        Err(ResponsesCanonicalProjectError::Unsupported(
            "provider-hosted tool caller",
        ))
    }
}

fn validate_opaque_kind(
    kind: &str,
    policy: ResponsesCanonicalPolicy,
) -> Result<(), ResponsesCanonicalProjectError> {
    if matches!(kind, "program" | "program_output") {
        return if policy.has(ResponsesCanonicalPolicy::PROGRAMMATIC_CALLERS) {
            Ok(())
        } else {
            Err(ResponsesCanonicalProjectError::Unsupported(
                "provider-programmatic output item",
            ))
        };
    }
    if matches!(
        kind,
        "computer_call"
            | "web_search_call"
            | "file_search_call"
            | "approval_request"
            | "compaction"
    ) || kind.starts_with("function_call")
        || kind.starts_with("custom_tool_call")
        || kind.starts_with("message")
    {
        Err(ResponsesCanonicalProjectError::Unsupported(
            "declared output-item subvariant",
        ))
    } else {
        Ok(())
    }
}

fn require_terminal_policy(
    data: &ResponsesCanonicalDialectData,
    capability: u8,
    surface: &'static str,
) -> Result<(), ResponsesCanonicalProjectError> {
    if data.policy.has(capability) {
        Ok(())
    } else {
        Err(ResponsesCanonicalProjectError::Unsupported(surface))
    }
}

struct TerminalBudget {
    limits: ProjectorLimits,
    blocks: usize,
    bytes: usize,
}

impl TerminalBudget {
    const fn new(limits: ProjectorLimits) -> Self {
        Self {
            limits,
            blocks: 0,
            bytes: 0,
        }
    }

    fn add_block(&mut self, bytes: usize) -> Result<(), ResponsesCanonicalProjectError> {
        self.blocks =
            self.blocks
                .checked_add(1)
                .ok_or(ResponsesCanonicalProjectError::ResourceLimit(
                    "canonical blocks",
                ))?;
        self.bytes =
            self.bytes
                .checked_add(bytes)
                .ok_or(ResponsesCanonicalProjectError::ResourceLimit(
                    "canonical bytes",
                ))?;
        if self.blocks > self.limits.max_blocks {
            return Err(ResponsesCanonicalProjectError::ResourceLimit(
                "canonical blocks",
            ));
        }
        if self.bytes > self.limits.max_accumulated_bytes {
            return Err(ResponsesCanonicalProjectError::ResourceLimit(
                "canonical bytes",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "unit tests use deterministic fixture construction"
)]
mod tests {
    use std::sync::Arc;

    use plakous_model::{
        AttemptId, ContentBlock, DigestDomain, FinishReason, FixedClock, ModelEvent, ModelRequest,
        ModelResponseAssembler, ProtocolId, ReplayKindId, RequestMetadata, RunId,
        SequenceIdGenerator, Timestamp, ToolConcurrency, ToolDefinition, ToolEffect,
        ToolInputContract, ToolOutputContract, TurnId, Usage,
    };
    use plakous_responses::{ResponsesStorageMode, ResponsesStreamLimits};
    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::{
        ResponsesFamilySseDecoder, ResponsesReplayProjection, ResponsesResponse,
        ResponsesResponseLimits, ResponsesStreamDialectData, ResponsesUsagePolicy,
        project_terminal_response,
    };

    const MODEL: &str = "gpt-test";

    fn digest() -> Digest {
        Digest::sha256(DigestDomain::RequestPlan, 1, &[b"canonical-host-test"])
            .expect("test digest")
    }

    fn provider_request() -> crate::project_host::ResponsesProviderRequestBinding {
        crate::project_host::ResponsesProviderRequestBinding::new(
            Digest::sha256(
                DigestDomain::ProviderOperation,
                1,
                &[b"canonical-host-physical-request"],
            )
            .expect("physical request digest"),
            Digest::sha256(
                DigestDomain::ProviderOperation,
                1,
                &[b"canonical-host-terminal-response"],
            )
            .expect("terminal response digest"),
            Some(ModelProviderRequestId::new("req_canonical_1").expect("provider request ID")),
        )
        .expect("provider request binding")
    }

    fn ids() -> CanonicalIdSources {
        let clock = Arc::new(FixedClock::new(
            Timestamp::new(42, 0).expect("test timestamp"),
        ));
        let generator = Arc::new(
            SequenceIdGenerator::new((1_u128..=32).map(Uuid::from_u128))
                .expect("test identity sequence"),
        );
        CanonicalIdSources::new(clock, generator)
    }

    fn data() -> ResponsesCanonicalDialectData {
        ResponsesCanonicalDialectData::codex(
            ModelId::new(MODEL).expect("test model"),
            [ResponsesCanonicalTool::local_function(
                ToolName::new("lookup").expect("test tool"),
            )],
        )
        .expect("test dialect data")
    }

    fn terminal_json(status: &str) -> Value {
        let (details, item_status) = if status == "incomplete" {
            (json!({"reason":"max_output_tokens"}), "incomplete")
        } else {
            (Value::Null, "completed")
        };
        json!({
            "id":"resp_1",
            "object":"response",
            "status":status,
            "model":MODEL,
            "output":[
                {"id":"m1","type":"message","role":"assistant","status":item_status,
                 "content":[{"type":"output_text","text":"answer","annotations":[
                    {"type":"url_citation","start_index":0,"end_index":6,
                     "url":"https://example.test","title":"Example"}
                 ]}]},
                {"id":"r1","type":"reasoning","status":item_status,
                 "summary":[{"type":"summary_text","text":"checked"}],
                 "encrypted_content":"opaque"},
                {"id":"f1","type":"function_call","status":"completed",
                 "call_id":"call_1","name":"lookup","arguments":"{\"q\":1}"},
                {"id":"future1","type":"future_output","status":item_status,"payload":"opaque"}
            ],
            "usage":{"input_tokens":7,"output_tokens":5,
                     "input_tokens_details":{"cached_tokens":3},
                     "output_tokens_details":{"reasoning_tokens":2}},
            "incomplete_details":details
        })
    }

    fn replay_projection() -> ResponsesReplayProjection {
        ResponsesReplayProjection::new(
            ProtocolId::new("openai.responses").expect("test protocol"),
            None,
            ModelId::new(MODEL).expect("test model"),
            ReplayKindId::new("responses.output_item").expect("test replay kind"),
            1,
        )
        .expect("test replay projection")
    }

    fn terminal(status: &str) -> ResponsesTerminalProjection {
        terminal_from_json(&terminal_json(status))
    }

    fn terminal_from_json(value: &Value) -> ResponsesTerminalProjection {
        let response = ResponsesResponse::parse(
            value,
            ResponsesUsagePolicy::DeriveTotalWhenOmitted,
            ResponsesResponseLimits::default(),
        )
        .expect("test terminal response");
        project_terminal_response(
            response,
            &replay_projection(),
            ResponsesStorageMode::Stateless,
            provider_request(),
        )
        .expect("test terminal projection")
    }

    #[test]
    fn native_compaction_output_is_rejected_without_qualified_facet() {
        let mut value = terminal_json("completed");
        value["output"] = json!([{
            "id": "compact_1",
            "type": "compaction",
            "encrypted_content": "opaque-provider-ciphertext"
        }]);
        assert!(matches!(
            project_canonical_terminal(&terminal_from_json(&value), &data(), digest(), &ids()),
            Err(ResponsesCanonicalProjectError::Unsupported(
                "Responses native compaction is not qualified"
            ))
        ));
    }

    #[test]
    fn native_compaction_output_projects_when_facet_enrolled() {
        let mut value = terminal_json("completed");
        value["output"] = json!([{
            "id": "compact_1",
            "type": "compaction",
            "encrypted_content": "opaque-provider-ciphertext"
        }]);
        let terminal = project_canonical_terminal(
            &terminal_from_json(&value),
            &data().with_native_compaction(plakous_model::CompactionReplayBinding::new(
                ProtocolId::new("openai.responses").expect("protocol"),
                None,
                ModelId::new(MODEL).expect("model"),
                Digest::sha256(DigestDomain::ModelBinding, 1, &[b"fixture-account"])
                    .expect("account"),
            )),
            digest(),
            &ids(),
        )
        .expect("enrolled native compact projects");
        let blocks = terminal.response().output()[0].blocks();
        let ContentBlock::Compaction(block) = &blocks[0] else {
            panic!("expected compaction block, got {blocks:?}");
        };
        assert_eq!(block.summary(), "opaque-provider-ciphertext");
        assert!(
            block
                .native_item()
                .is_some_and(|item| item.contains("opaque-provider-ciphertext"))
        );
    }

    fn observation_projector(
        data: ResponsesCanonicalDialectData,
    ) -> ResponsesCanonicalObservationProjector {
        let mut projector =
            ResponsesCanonicalObservationProjector::new(data, digest(), None, ids());
        let started = projector.project(ResponsesStreamEventKind::ResponseCreated {
            response_id: Some("resp_stream_ptc".to_owned()),
            model: Some(MODEL.to_owned()),
        });
        assert!(!started.is_empty(), "response start must be observable");
        projector
    }

    fn parsed_output_item(value: Value) -> ResponsesOutputItem {
        crate::parse_output_item::<crate::ResponsesHostDecodeError>(&value)
            .expect("test output item")
    }

    fn add_observed_item(
        projector: &mut ResponsesCanonicalObservationProjector,
        output_index: u32,
        value: Value,
    ) -> Vec<ModelEvent> {
        projector.project(ResponsesStreamEventKind::OutputItemAdded {
            output_index,
            item: parsed_output_item(value),
        })
    }

    #[test]
    fn openrouter_terminal_requires_exact_single_provider_routing_proof() {
        let mut value = json!({
            "id":"resp_router","object":"response","status":"completed","model":MODEL,
            "output":[],"usage":{"input_tokens":1,"output_tokens":1},
            "openrouter_metadata": {
                "requested": MODEL, "strategy":"direct", "attempt":1, "is_byok":false,
                "endpoints":{"total":1,"available":[{
                    "provider":"OpenAI","model":MODEL,"selected":true
                }]},
                "attempts":[{"provider":"OpenAI","model":MODEL,"status":200}],
                "pipeline":[],"params":{}
            }
        });
        let data =
            ResponsesCanonicalDialectData::openrouter(ModelId::new(MODEL).expect("test model"), [])
                .and_then(|data| data.with_router_provider("OpenAI"))
                .expect("exact router facts");
        assert!(
            project_canonical_terminal(&terminal_from_json(&value), &data, digest(), &ids())
                .is_ok()
        );
        value["openrouter_metadata"]["endpoints"]["available"][0]["provider"] =
            Value::String("Fallback".to_owned());
        assert!(
            project_canonical_terminal(&terminal_from_json(&value), &data, digest(), &ids())
                .is_err()
        );
    }

    #[test]
    fn grok_preserves_consistent_provider_reported_actual_model() {
        let value = json!({
            "id":"resp_routed","object":"response","status":"completed",
            "model":"grok-routed-backend",
            "output":[{
                "id":"m-routed","type":"message","role":"assistant","status":"completed",
                "content":[{"type":"output_text","text":"answer","annotations":[]}]
            }],
            "usage":{"input_tokens":7,"output_tokens":2}
        });
        let response = ResponsesResponse::parse(
            &value,
            ResponsesUsagePolicy::DeriveTotalWhenOmitted,
            ResponsesResponseLimits::default(),
        )
        .expect("provider-reported terminal response");
        let terminal = crate::project_host::project_terminal_response_with_model_binding(
            response,
            &replay_projection(),
            ResponsesStorageMode::Stateless,
            provider_request(),
            plakous_responses::ResponseModelBinding::ProviderReported,
        )
        .expect("provider-reported projection");
        let grok =
            ResponsesCanonicalDialectData::grok(ModelId::new(MODEL).expect("selected model"), [])
                .expect("Grok canonical data");
        let projected = project_canonical_terminal(&terminal, &grok, digest(), &ids())
            .expect("Grok preserves routed actual identity");
        assert_eq!(
            projected.response().actual_model().map(ModelId::as_str),
            Some("grok-routed-backend")
        );

        let codex =
            ResponsesCanonicalDialectData::codex(ModelId::new(MODEL).expect("selected model"), [])
                .expect("Codex canonical data");
        let projected = project_canonical_terminal(&terminal, &codex, digest(), &ids())
            .expect("Codex OAuth preserves provider-reported stream model");
        assert_eq!(
            projected.response().actual_model().map(ModelId::as_str),
            Some("grok-routed-backend")
        );

        let platform = ResponsesCanonicalDialectData::openai_platform(
            ModelId::new(MODEL).expect("selected model"),
            [],
        )
        .expect("Platform canonical data");
        assert!(
            project_canonical_terminal(&terminal, &platform, digest(), &ids()).is_err(),
            "OpenAI Platform keeps ExactRequest"
        );
    }

    fn producing_request() -> ModelRequest {
        producing_request_with_tool(false)
    }

    fn producing_request_with_tool(custom: bool) -> ModelRequest {
        producing_request_with_tool_and_caller(
            custom,
            plakous_model::ToolCallerDisposition::DirectOnly,
        )
    }

    fn producing_request_with_tool_and_caller(
        custom: bool,
        caller: plakous_model::ToolCallerDisposition,
    ) -> ModelRequest {
        crate::test_support::model_request_builder(
            plakous_model::ConversationView::default(),
            RequestMetadata::for_turn(
                RunId::try_from(Uuid::from_u128(101)).expect("run"),
                TurnId::try_from(Uuid::from_u128(102)).expect("turn"),
                AttemptId::try_from(Uuid::from_u128(103)).expect("attempt"),
                Timestamp::new(42, 0).expect("request time"),
            ),
        )
        .tools(vec![
            plakous_model::ConfiguredTool::new(
            ToolDefinition::new(
                ToolName::new("lookup").expect("tool"),
                "1.0.0",
                "lookup",
                if custom {
                    ToolInputContract::utf8(65_536).expect("UTF-8 contract")
                } else {
                    ToolInputContract::structured(json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{},"required":[],"additionalProperties":false})).expect("schema")
                },
                ToolOutputContract::portable_model_content(),
                ToolConcurrency::Parallel,
                ToolEffect::ReadOnly,
            )
            .expect("tool definition"),
            plakous_model::ToolExecutionDisposition::Local,
            caller,
            ),
        ])
        .hosted_tools(
            (!matches!(caller, plakous_model::ToolCallerDisposition::DirectOnly))
                .then_some(plakous_model::HostedTool::ProgrammaticComposition)
                .into_iter()
                .collect(),
        )
        .build()
        .expect("producing request")
    }

    #[test]
    fn hosted_programmatic_owner_projection_retains_plak_mcp() {
        let programmatic = producing_request_with_tool_and_caller(
            false,
            plakous_model::ToolCallerDisposition::DirectAndProviderProgrammatic,
        );
        let local = ResponsesCanonicalDialectData::openai_platform(
            ModelId::new(MODEL).expect("model"),
            [ResponsesCanonicalTool::local_function(
                ToolName::new("lookup").expect("tool"),
            )],
        )
        .expect("local projection");
        assert!(local.validate_tool_owner_projection(&programmatic).is_ok());

        let mcp = ResponsesCanonicalDialectData::openai_platform(
            ModelId::new(MODEL).expect("model"),
            [ResponsesCanonicalTool::new(
                ToolName::new("lookup").expect("tool"),
                ResponsesCanonicalToolKind::Function,
                ResponsesCanonicalToolOwner::PlakMcp(
                    McpServerId::new("lookup-mcp").expect("server"),
                ),
            )],
        )
        .expect("MCP projection");
        assert!(mcp.validate_tool_owner_projection(&programmatic).is_ok());
        assert!(
            mcp.validate_tool_owner_projection(&producing_request())
                .is_ok(),
            "ordinary direct MCP projection must remain admitted"
        );
    }

    #[test]
    fn saturated_closed_and_no_receiver_delivery_produce_identical_terminal() {
        let projection = terminal("completed");
        let no_receiver = project_canonical_terminal(&projection, &data(), digest(), &ids())
            .expect("no-receiver terminal");

        let mut saturated =
            ResponsesCanonicalObservationProjector::new(data(), digest(), None, ids());
        let _ = saturated.project(ResponsesStreamEventKind::OutputTextDelta {
            output_index: 0,
            content_index: 0,
            delta: "dropped prerequisite".into(),
        });
        assert!(saturated.gapped(), "saturation-shaped gap must be sticky");
        let saturated_terminal = project_canonical_terminal(&projection, &data(), digest(), &ids())
            .expect("saturated terminal");

        let closed = ResponsesCanonicalObservationProjector::new(data(), digest(), None, ids());
        drop(closed);
        let closed_terminal = project_canonical_terminal(&projection, &data(), digest(), &ids())
            .expect("closed-receiver terminal");

        assert_eq!(no_receiver.response(), saturated_terminal.response());
        assert_eq!(no_receiver.response(), closed_terminal.response());
    }

    #[test]
    fn terminal_preserves_text_citation_reasoning_tool_unknown_usage_and_replay() {
        let raw = terminal("completed");
        let projected = project_canonical_terminal(&raw, &data(), digest(), &ids())
            .expect("canonical terminal");
        assert_eq!(
            projected.response().finish().reason(),
            FinishReason::ToolCalls
        );
        assert_eq!(
            projected.response().usage(),
            Some(Usage::new(7, 5, 2, 3, 0))
        );
        assert_eq!(
            projected.response().provider_request_id(),
            Some("req_canonical_1")
        );
        let output = projected.canonical_output();
        assert_eq!(output.len(), 4);
        assert!(matches!(
            output[0].blocks(),
            [ContentBlock::Text(text), ContentBlock::Citation(_), ContentBlock::Opaque(_)]
                if text.phase() == Some(TextPhase::Final) && text.text() == "answer"
        ));
        assert!(matches!(
            output[1].blocks(),
            [ContentBlock::Reasoning(reasoning), ContentBlock::Opaque(opaque)]
                if reasoning.visibility() == ReasoningVisibility::Summary
                    && reasoning.text() == Some("checked")
                    && opaque.fallback() == &OpaqueFallback::Omit
        ));
        assert!(matches!(
            output[2].blocks(),
            [ContentBlock::ToolCall(call), ContentBlock::Opaque(_)]
                if call.owner() == &ToolExecutionOwner::PlakLocal
                    && call.state() == ToolCallState::Requested
                    && call.correlation().remote_call_id() == Some("call_1")
                    && call.raw_arguments() == "{\"q\":1}"
        ));
        assert!(matches!(output[3].blocks(), [ContentBlock::Opaque(_)]));
        assert_eq!(
            output
                .iter()
                .map(|item| item
                    .blocks()
                    .iter()
                    .filter(|block| matches!(block, ContentBlock::Opaque(_)))
                    .count())
                .collect::<Vec<_>>(),
            vec![1, 1, 1, 1]
        );
        raw.pair_canonical(&producing_request(), projected.canonical_output().to_vec())
            .expect("exact raw/canonical pairing");
    }

    #[test]
    fn codex_final_after_multiple_execs_omits_empty_placeholders_but_retains_replay() {
        let value: Value = serde_json::from_str(include_str!(
            "../../../providers/openai/tests/fixtures/response_codex_final_empty_placeholders.json"
        ))
        .expect("Codex multi-exec terminal fixture");
        let raw = terminal_from_json(&value);
        let projected = project_canonical_terminal(&raw, &data(), digest(), &ids())
            .expect("empty provider placeholders are not canonical content");

        assert_eq!(projected.response().output_text(), "Done.");
        assert!(matches!(
            projected.canonical_output()[0].blocks(),
            [ContentBlock::Opaque(opaque)]
                if serde_json::from_str::<Value>(opaque.replay().payload().get())
                    .is_ok_and(|native| native["summary"][0]["text"] == "")
        ));
        assert!(matches!(
            projected.canonical_output()[1].blocks(),
            [ContentBlock::Text(text), ContentBlock::Opaque(_)] if text.text() == "Done."
        ));
        raw.pair_canonical(&producing_request(), projected.canonical_output().to_vec())
            .expect("empty placeholders do not break exact native/canonical pairing");
    }

    #[test]
    fn retained_unknown_message_content_requires_compatible_replay() {
        let terminal = json!({
            "id":"resp_unknown","object":"response","status":"completed","model":MODEL,
            "output":[{
                "id":"msg_unknown","type":"message","role":"assistant","status":"completed",
                "content":[{"type":"unknown_part","data":"opaque-content"}]
            }]
        });
        let raw = terminal_from_json(&terminal);
        let projected = project_canonical_terminal(&raw, &data(), digest(), &ids())
            .expect("unknown message part retained");
        assert!(matches!(
            projected.canonical_output()[0].blocks(),
            [ContentBlock::Opaque(opaque)]
                if *opaque.fallback() == OpaqueFallback::CompatibleReplay
        ));
        let request = crate::test_support::model_request_builder(
            plakous_model::ConversationView::new(projected.canonical_output().to_vec()),
            producing_request().metadata(),
        )
        .build()
        .expect("historical request");
        for policy in [
            crate::ResponsesRequestPolicy::openai_codex(
                false,
                false,
                "reasoning.encrypted_content",
            ),
            crate::ResponsesRequestPolicy::supergrok_stateless(),
            crate::ResponsesRequestPolicy::openrouter_stateless(),
        ] {
            let error = crate::lower_responses_request(
                &request,
                crate::ResponsesRequestConfig::new(
                    &ProtocolId::new("openai.responses").expect("protocol"),
                    None,
                    &ModelId::new(MODEL).expect("model"),
                    policy,
                    None,
                ),
            )
            .expect_err("unknown content must not be silently dropped on fresh prepare");
            assert!(matches!(
                error,
                crate::ResponsesRequestError::Content(
                    crate::ResponsesRequestContentError::OpaqueOnlyWithoutCompatibleReplay
                )
            ));
            assert_eq!(
                crate::ResponsesAttemptBuildError::Request(error).preparation_error_kind(),
                plakous_model::ModelPreparationErrorKind::ProtectedHistory,
            );
        }
    }

    /// A SuperGrok history holding a retained unknown output item, or an
    /// assistant refusal, cannot be lowered fresh: it fails `ProtectedHistory`.
    /// That is why the tool-free operation-budget wrap-up must keep the bound
    /// replay of the live artifact rather than prepare fresh.
    #[test]
    fn grok_fresh_lowering_refuses_retained_unknown_items_and_refusals() {
        let grok = ResponsesCanonicalDialectData::grok(
            ModelId::new(MODEL).expect("selected model"),
            [ResponsesCanonicalTool::local_function(
                ToolName::new("lookup").expect("test tool"),
            )],
        )
        .expect("Grok canonical data");
        let lower = |output: Vec<Value>| {
            let value = json!({
                "id":"resp_grok","object":"response","status":"completed","model":MODEL,
                "output": output,
                "usage":{"input_tokens":7,"output_tokens":5}
            });
            let raw = terminal_from_json(&value);
            let projected = project_canonical_terminal(&raw, &grok, digest(), &ids())
                .expect("Grok projects the terminal");
            let request = crate::test_support::model_request_builder(
                plakous_model::ConversationView::new(projected.canonical_output().to_vec()),
                producing_request().metadata(),
            )
            .tool_choice(plakous_model::ToolChoice::None)
            .build()
            .expect("wrap-up request");
            crate::lower_responses_request(
                &request,
                crate::ResponsesRequestConfig::new(
                    &ProtocolId::new("openai.responses").expect("protocol"),
                    None,
                    &ModelId::new(MODEL).expect("model"),
                    crate::ResponsesRequestPolicy::supergrok_stateless(),
                    None,
                ),
            )
            .map(|_| ())
        };
        let reasoning = json!({"id":"r1","type":"reasoning","status":"completed",
            "summary":[],"encrypted_content":"opaque"});
        let message = json!({"id":"m1","type":"message","role":"assistant","status":"completed",
            "content":[{"type":"output_text","text":"partial","annotations":[]}]});
        let unknown = json!({"id":"future1","type":"future_output","status":"completed",
            "payload":"opaque"});
        let refusal = json!({"id":"m2","type":"message","role":"assistant","status":"completed",
            "content":[{"type":"refusal","refusal":"no"}]});

        // Control: an ordinary Grok round lowers fresh.
        lower(vec![reasoning.clone(), message.clone()]).expect("ordinary history lowers fresh");

        let unknown_error = lower(vec![reasoning.clone(), message.clone(), unknown])
            .expect_err("retained unknown output blocks fresh lowering");
        assert_eq!(
            crate::ResponsesAttemptBuildError::Request(unknown_error).preparation_error_kind(),
            plakous_model::ModelPreparationErrorKind::ProtectedHistory,
        );
        let refusal_error = lower(vec![reasoning, message, refusal])
            .expect_err("historical refusal blocks fresh lowering");
        assert_eq!(
            crate::ResponsesAttemptBuildError::Request(refusal_error).preparation_error_kind(),
            plakous_model::ModelPreparationErrorKind::ProtectedHistory,
        );
    }

    #[test]
    fn terminal_with_only_empty_known_content_retains_exact_replay() {
        let empty = json!({
            "id":"resp_empty","object":"response","status":"completed","model":MODEL,
            "output":[{
                "id":"msg_empty","type":"message","role":"assistant","status":"completed",
                "content":[{"type":"output_text","text":"","annotations":[]}]
            }]
        });
        let raw = terminal_from_json(&empty);
        let projected = project_canonical_terminal(&raw, &data(), digest(), &ids())
            .expect("completed provider placeholder remains valid native output");
        assert_eq!(projected.response().output_text(), "");
        assert!(matches!(
            projected.canonical_output()[0].blocks(),
            [ContentBlock::Opaque(opaque)]
                if *opaque.fallback() == OpaqueFallback::Omit
        ));
        let request = crate::test_support::model_request_builder(
            plakous_model::ConversationView::new(projected.canonical_output().to_vec()),
            producing_request().metadata(),
        )
        .build()
        .expect("blank historical request");
        for policy in [
            crate::ResponsesRequestPolicy::openai_codex(
                false,
                false,
                "reasoning.encrypted_content",
            ),
            crate::ResponsesRequestPolicy::supergrok_stateless(),
            crate::ResponsesRequestPolicy::openrouter_stateless(),
        ] {
            let lowered = crate::lower_responses_request(
                &request,
                crate::ResponsesRequestConfig::new(
                    &ProtocolId::new("openai.responses").expect("protocol"),
                    None,
                    &ModelId::new(MODEL).expect("model"),
                    policy,
                    None,
                ),
            )
            .expect("blank historical messages omit without replay");
            assert_eq!(lowered.body()["input"], json!([]));
        }
        projected
            .response()
            .validate_for_commit()
            .expect("exact replay is a non-empty canonical assistant output");
        raw.pair_canonical(&producing_request(), projected.canonical_output().to_vec())
            .expect("replay-only output retains exact raw/canonical pairing");

        let no_output = json!({
            "id":"resp_no_output","object":"response","status":"completed","model":MODEL,
            "output":[]
        });
        let projected =
            project_canonical_terminal(&terminal_from_json(&no_output), &data(), digest(), &ids())
                .expect("empty aggregate remains representable until the commit boundary");
        assert!(projected.response().validate_for_commit().is_err());
    }

    #[test]
    fn authoritative_events_reassemble_the_exact_terminal_response() {
        let projected =
            project_canonical_terminal(&terminal("completed"), &data(), digest(), &ids())
                .expect("canonical terminal");
        let expected = projected.response().clone();
        let events = projected.into_events();
        assert!(matches!(
            events.first(),
            Some(ModelEvent::ResponseStarted(_))
        ));
        assert!(matches!(
            events.last(),
            Some(ModelEvent::ResponseCompleted(_))
        ));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ModelEvent::ResponseCompleted(_)))
                .count(),
            1
        );

        let mut machine = ModelResponseAssembler::new(digest()).expect("test assembler");
        let mut reconstructed = None;
        for event in events {
            if let Some(response) = machine.push(event).expect("self-consistent event stream") {
                assert!(
                    reconstructed.is_none(),
                    "terminal response must be emitted once"
                );
                reconstructed = Some(response);
            }
        }
        machine.finish_eof().expect("complete event stream");
        assert_eq!(reconstructed.as_ref(), Some(&expected));
    }

    #[test]
    fn incomplete_terminal_keeps_length_finish_and_never_depends_on_live_state() {
        let projected =
            project_canonical_terminal(&terminal("incomplete"), &data(), digest(), &ids())
                .expect("incomplete canonical terminal");
        assert_eq!(projected.response().finish().reason(), FinishReason::Length);
        assert_eq!(
            projected.response().finish().detail(),
            Some("max_output_tokens")
        );
        let events = projected.into_events();
        assert!(matches!(
            events.first(),
            Some(ModelEvent::ResponseStarted(_))
        ));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ModelEvent::Usage(_)))
        );
        assert!(matches!(
            events.last(),
            Some(ModelEvent::ResponseCompleted(_))
        ));
    }

    #[test]
    fn codex_and_grok_preserve_commentary_but_admit_optional_surfaces_differently() {
        let commentary = terminal_from_json(&json!({
            "id":"resp_commentary","object":"response","status":"completed","model":MODEL,
            "output":[{
                "id":"m-commentary","type":"message","role":"assistant","status":"completed",
                "phase":"commentary",
                "content":[{"type":"output_text","text":"working","annotations":[]}]
            }]
        }));
        let codex = ResponsesCanonicalDialectData::codex(ModelId::new(MODEL).expect("model"), [])
            .expect("Codex data");
        let grok = ResponsesCanonicalDialectData::grok(ModelId::new(MODEL).expect("model"), [])
            .expect("Grok data");
        for dialect in [&codex, &grok] {
            let projected = project_canonical_terminal(&commentary, dialect, digest(), &ids())
                .expect("commentary is common message semantics");
            assert!(matches!(
                projected.canonical_output()[0].blocks(),
                [ContentBlock::Text(text), ContentBlock::Opaque(_)]
                    if text.text() == "working"
                        && text.phase() == Some(TextPhase::Commentary)
            ));
        }

        let citation = terminal_from_json(&json!({
            "id":"resp_citation","object":"response","status":"completed","model":MODEL,
            "output":[{
                "id":"m-citation","type":"message","role":"assistant","status":"completed",
                "content":[{"type":"output_text","text":"source","annotations":[{
                    "type":"url_citation","start_index":0,"end_index":6,
                    "url":"https://example.test/source","title":"Source"
                }]}]
            }]
        }));
        assert!(project_canonical_terminal(&citation, &codex, digest(), &ids()).is_ok());
        assert!(matches!(
            project_canonical_terminal(&citation, &grok, digest(), &ids()),
            Err(ResponsesCanonicalProjectError::Unsupported("citations"))
        ));
    }

    #[test]
    fn codex_empty_custom_input_is_exact_and_grok_does_not_import_custom_policy() {
        let custom = terminal_from_json(&json!({
            "id":"resp_custom","object":"response","status":"completed","model":MODEL,
            "output":[{
                "id":"custom-1","type":"custom_tool_call","status":"completed",
                "call_id":"call-custom-1","name":"lookup","input":""
            }]
        }));
        let codex = ResponsesCanonicalDialectData::codex(
            ModelId::new(MODEL).expect("model"),
            [ResponsesCanonicalTool::local_custom(
                ToolName::new("lookup").expect("custom tool"),
            )],
        )
        .expect("Codex data");
        let projected = project_canonical_terminal(&custom, &codex, digest(), &ids())
            .expect("empty custom input is valid arbitrary input");
        assert!(matches!(
            projected.canonical_output()[0].blocks(),
            [ContentBlock::ToolCall(call), ContentBlock::Opaque(_)]
                if call.raw_arguments() == "\"\""
                    && call.arguments() == Some(&Value::String(String::new()))
        ));
        custom
            .clone()
            .pair_canonical(
                &producing_request_with_tool(true),
                projected.canonical_output().to_vec(),
            )
            .expect("empty custom input pairs exactly with native replay");

        let grok = ResponsesCanonicalDialectData::grok(ModelId::new(MODEL).expect("model"), [])
            .expect("Grok data");
        assert!(matches!(
            project_canonical_terminal(&custom, &grok, digest(), &ids()),
            Err(ResponsesCanonicalProjectError::Unsupported("custom tools"))
        ));
        assert!(
            ResponsesCanonicalDialectData::grok(
                ModelId::new(MODEL).expect("model"),
                [ResponsesCanonicalTool::local_custom(
                    ToolName::new("lookup").expect("custom tool"),
                )],
            )
            .is_err()
        );
    }

    #[test]
    fn codex_lite_accepts_only_advertised_functions_namespace_calls() {
        for (kind, arguments, tool) in [
            (
                "function_call",
                json!({"arguments":"{}"}),
                ResponsesCanonicalTool::local_function(ToolName::new("lookup").expect("tool")),
            ),
            (
                "custom_tool_call",
                json!({"input":"await tools.exec_command({cmd:'pwd'})"}),
                ResponsesCanonicalTool::local_custom(ToolName::new("lookup").expect("tool")),
            ),
        ] {
            let mut item = json!({
                "id":"call_item","type":kind,"status":"completed",
                "call_id":"call_remote","name":"lookup","namespace":"functions"
            });
            item.as_object_mut()
                .expect("item object")
                .extend(arguments.as_object().expect("argument object").clone());
            let mut response = json!({
                "id":"resp_lite","object":"response","status":"completed",
                "model":MODEL,"output":[item]
            });
            let standard = ResponsesCanonicalDialectData::codex(
                ModelId::new(MODEL).expect("model"),
                [tool.clone()],
            )
            .expect("standard data");
            let lite = standard.clone().with_codex_lite_namespace();
            assert!(
                project_canonical_terminal(
                    &terminal_from_json(&response),
                    &standard,
                    digest(),
                    &ids()
                )
                .is_err()
            );
            let projected =
                project_canonical_terminal(&terminal_from_json(&response), &lite, digest(), &ids())
                    .expect("exact Lite namespace must project");
            assert!(matches!(
                projected.canonical_output()[0].blocks(),
                [ContentBlock::ToolCall(call), ContentBlock::Opaque(_)]
                    if call.exposed_name().as_str() == "lookup"
            ));
            terminal_from_json(&response)
                .pair_canonical(
                    &producing_request_with_tool(kind == "custom_tool_call"),
                    projected.canonical_output().to_vec(),
                )
                .expect("namespaced raw item pairs with exact canonical replay");
            response["output"][0]["namespace"] = json!("foreign");
            assert!(
                project_canonical_terminal(&terminal_from_json(&response), &lite, digest(), &ids())
                    .is_err()
            );
            response["output"][0]["namespace"] = json!("functions");
            response["output"][0]["name"] = json!("unadvertised");
            assert!(
                project_canonical_terminal(&terminal_from_json(&response), &lite, digest(), &ids())
                    .is_err()
            );
        }
    }

    #[test]
    fn unadvertised_function_call_projects_as_local_requested() {
        let function = terminal_from_json(&json!({
            "id":"resp_hidden","object":"response","status":"completed","model":MODEL,
            "output":[{
                "id":"function-hidden","type":"function_call","status":"completed",
                "call_id":"call-hidden","name":"process_session_poll","arguments":"{}"
            }]
        }));
        let dialect = ResponsesCanonicalDialectData::codex(
            ModelId::new(MODEL).expect("model"),
            [ResponsesCanonicalTool::local_function(
                ToolName::new("lookup").expect("advertised tool"),
            )],
        )
        .expect("Codex data");
        let projected = project_canonical_terminal(&function, &dialect, digest(), &ids())
            .expect("unadvertised names must project as local requested calls");
        assert!(matches!(
            projected.canonical_output()[0].blocks(),
            [ContentBlock::ToolCall(call), ContentBlock::Opaque(_)]
                if call.exposed_name().as_str() == "process_session_poll"
                    && *call.owner() == ToolExecutionOwner::PlakLocal
                    && call.state() == ToolCallState::Requested
        ));
    }

    #[test]
    fn codex_and_grok_reject_non_object_or_non_unique_function_json() {
        for arguments in ["[]", "{\"key\":1,\"key\":2}", "not json"] {
            let function = terminal_from_json(&json!({
                "id":"resp_function","object":"response","status":"completed","model":MODEL,
                "output":[{
                    "id":"function-1","type":"function_call","status":"completed",
                    "call_id":"call-function-1","name":"lookup","arguments":arguments
                }]
            }));
            for dialect in [
                ResponsesCanonicalDialectData::codex(
                    ModelId::new(MODEL).expect("model"),
                    [ResponsesCanonicalTool::local_function(
                        ToolName::new("lookup").expect("function tool"),
                    )],
                )
                .expect("Codex data"),
                ResponsesCanonicalDialectData::grok(
                    ModelId::new(MODEL).expect("model"),
                    [ResponsesCanonicalTool::local_function(
                        ToolName::new("lookup").expect("function tool"),
                    )],
                )
                .expect("Grok data"),
            ] {
                assert!(
                    project_canonical_terminal(&function, &dialect, digest(), &ids()).is_err(),
                    "invalid function JSON was accepted: {arguments:?}"
                );
            }
        }
    }

    #[test]
    fn live_projector_emits_best_effort_text_reasoning_tools_and_unknowns() {
        let fixture = include_str!("../../../providers/openai/tests/fixtures/response_mixed.sse");
        let mut decoder = ResponsesFamilySseDecoder::new(
            ResponsesStreamDialectData::codex("gpt-5.4"),
            ResponsesStreamLimits::default(),
        );
        let mut family = decoder.push(fixture.as_bytes()).expect("fixture decode");
        family.extend(decoder.finish().expect("fixture finish"));
        let mut projector = ResponsesCanonicalObservationProjector::new(
            ResponsesCanonicalDialectData::codex(
                ModelId::new("gpt-5.4").expect("fixture model"),
                [ResponsesCanonicalTool::local_function(
                    ToolName::new("lookup").expect("fixture tool"),
                )],
            )
            .expect("fixture dialect"),
            digest(),
            None,
            ids(),
        );
        let events = family
            .into_iter()
            .flat_map(|event| projector.project(event))
            .collect::<Vec<_>>();
        assert!(!projector.gapped(), "complete fixture must not gap");
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ModelEvent::TextDelta(_)))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ModelEvent::ReasoningDelta(_)))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ModelEvent::ToolInputDelta(_)))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ModelEvent::ProviderEvent(_)))
        );
    }

    #[test]
    fn live_projector_emits_citations_and_custom_tool_completion() {
        let fixture =
            include_str!("../../../providers/openai/tests/fixtures/response_o1_surfaces.sse");
        let mut decoder = ResponsesFamilySseDecoder::new(
            ResponsesStreamDialectData::codex("gpt-5.4"),
            ResponsesStreamLimits::default(),
        );
        let mut family = decoder.push(fixture.as_bytes()).expect("fixture decode");
        family.extend(decoder.finish().expect("fixture finish"));
        let mut projector = ResponsesCanonicalObservationProjector::new(
            ResponsesCanonicalDialectData::codex(
                ModelId::new("gpt-5.4").expect("fixture model"),
                [
                    ResponsesCanonicalTool::local_function(
                        ToolName::new("lookup").expect("fixture function tool"),
                    ),
                    ResponsesCanonicalTool::local_custom(
                        ToolName::new("write_code").expect("fixture custom tool"),
                    ),
                ],
            )
            .expect("fixture dialect"),
            digest(),
            None,
            ids(),
        );
        let events = family
            .into_iter()
            .flat_map(|event| projector.project(event))
            .collect::<Vec<_>>();
        assert!(!projector.gapped(), "complete fixture must not gap");
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ModelEvent::CitationAdded(_)))
        );
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::BlockStarted(started)
                if matches!(started.block(), BlockStart::Complete(ContentBlock::ToolCall(call))
                    if call.exposed_name().as_str() == "write_code")
        )));
    }

    #[test]
    fn program_caller_keeps_local_owner_and_direct_lineage_is_neighbor() {
        let value = json!({
            "id":"resp_ptc","object":"response","status":"completed","model":MODEL,
            "output":[
                {"type":"program","id":"p1","call_id":"pc1","code":"await lookup({q:1})","fingerprint":"opaque"},
                {"type":"function_call","id":"f1","call_id":"fc1","name":"lookup","arguments":"{\"q\":1}","caller":{"type":"program","caller_id":"pc1"}},
                {"type":"program_output","id":"o1","call_id":"pc1","result":"ok","status":"completed"}
            ]
        });
        let platform = ResponsesCanonicalDialectData::openai_platform(
            ModelId::new(MODEL).expect("model"),
            [ResponsesCanonicalTool::local_function(
                ToolName::new("lookup").expect("tool"),
            )],
        )
        .expect("platform data");
        let terminal = terminal_from_json(&value);
        assert!(project_canonical_terminal(&terminal, &data(), digest(), &ids()).is_err());
        let projected = project_canonical_terminal(&terminal, &platform, digest(), &ids())
            .expect("programmatic local tool projection");
        let call = projected
            .canonical_output()
            .iter()
            .flat_map(ConversationItem::blocks)
            .find_map(|block| {
                if let ContentBlock::ToolCall(call) = block {
                    Some(call)
                } else {
                    None
                }
            })
            .expect("nested tool call");
        assert_eq!(call.owner(), &ToolExecutionOwner::PlakLocal);
        assert_eq!(
            call.correlation()
                .caller_lineage()
                .provider_programmatic_execution_id(),
            Some("pc1")
        );

        for caller in [Value::Null, json!({"type":"direct"})] {
            let direct = json!({
                "id":"resp_direct","object":"response","status":"completed","model":MODEL,
                "output":[
                    {"type":"function_call","id":"f1","call_id":"fc1","name":"lookup","arguments":"{}","caller":caller}
                ]
            });
            let projected =
                project_canonical_terminal(&terminal_from_json(&direct), &data(), digest(), &ids())
                    .expect("direct caller projection");
            let direct_call = projected
                .canonical_output()
                .iter()
                .flat_map(ConversationItem::blocks)
                .find_map(|block| match block {
                    ContentBlock::ToolCall(call) => Some(call),
                    _ => None,
                })
                .expect("direct tool call");
            assert!(matches!(
                direct_call.correlation().caller_lineage(),
                plakous_model::ToolCallerLineage::Direct
            ));
        }
    }

    #[test]
    fn streaming_program_caller_requires_one_exact_preceding_program_before_tool_emission() {
        let platform = || {
            ResponsesCanonicalDialectData::openai_platform(
                ModelId::new(MODEL).expect("model"),
                [ResponsesCanonicalTool::local_function(
                    ToolName::new("lookup").expect("tool"),
                )],
            )
            .expect("platform data")
        };

        for (case, prior, caller_id) in [
            ("missing", None, "pc_missing"),
            ("forward", None, "pc_future"),
            (
                "foreign",
                Some(json!({
                    "type":"future_program","id":"foreign","call_id":"pc_foreign"
                })),
                "pc_foreign",
            ),
        ] {
            let mut projector = observation_projector(platform());
            let mut output_index = 0;
            if let Some(prior) = prior {
                let _ = add_observed_item(&mut projector, output_index, prior);
                assert!(!projector.gapped(), "{case} prior must remain admissible");
                output_index += 1;
            }
            let emitted = add_observed_item(
                &mut projector,
                output_index,
                json!({
                    "type":"function_call","id":format!("f_{case}"),
                    "call_id":format!("fc_{case}"),"name":"lookup","arguments":"{}",
                    "caller":{"type":"program","caller_id":caller_id}
                }),
            );
            assert!(emitted.is_empty(), "{case} caller emitted a customer tool");
            assert!(projector.gapped(), "{case} caller did not fail closed");
        }

        let mut duplicate = observation_projector(platform());
        for (index, id) in [(0, "p1"), (1, "p2")] {
            let emitted = add_observed_item(
                &mut duplicate,
                index,
                json!({
                    "type":"program","id":id,"call_id":"pc_duplicate",
                    "code":"await lookup({})","fingerprint":"opaque"
                }),
            );
            assert!(
                index == 0 || emitted.is_empty(),
                "duplicate program emitted an observation"
            );
        }
        assert!(duplicate.gapped(), "duplicate program call_id was admitted");

        let mut ordered = observation_projector(platform());
        let _ = add_observed_item(
            &mut ordered,
            0,
            json!({
                "type":"program","id":"p1","call_id":"pc_ordered",
                "code":"await lookup({})","fingerprint":"opaque"
            }),
        );
        let emitted = add_observed_item(
            &mut ordered,
            1,
            json!({
                "type":"function_call","id":"f1","call_id":"fc_ordered",
                "name":"lookup","arguments":"{}",
                "caller":{"type":"program","caller_id":"pc_ordered"}
            }),
        );
        assert!(!ordered.gapped(), "ordered program caller must be admitted");
        assert!(emitted.iter().any(|event| matches!(
            event,
            ModelEvent::BlockStarted(started)
                if matches!(started.block(), BlockStart::ToolCall { correlation, .. }
                    if correlation.caller_lineage()
                        .provider_programmatic_execution_id() == Some("pc_ordered"))
        )));
    }

    fn prior_program_context() -> ResponsesHostedProgramContext {
        let mut context = ResponsesHostedProgramContext::default();
        context
            .admit_replayed_output(
                &parsed_output_item(json!({
                    "type":"program","id":"p_prior","call_id":"pc_prior",
                    "code":"await lookup({})","fingerprint":"fp-prior"
                })),
                ResponsesCanonicalPolicy::openai_platform(),
            )
            .expect("validated prior program");
        context
    }

    #[test]
    fn later_aggregate_and_stream_items_require_exact_prior_live_program() {
        let platform = || {
            ResponsesCanonicalDialectData::openai_platform(
                ModelId::new(MODEL).expect("model"),
                [ResponsesCanonicalTool::local_function(
                    ToolName::new("lookup").expect("tool"),
                )],
            )
            .expect("platform data")
        };
        let later_call = json!({
            "id":"resp_later_call","object":"response","status":"completed","model":MODEL,
            "output":[{
                "type":"function_call","id":"f_later","call_id":"fc_later",
                "name":"lookup","arguments":"{}",
                "caller":{"type":"program","caller_id":"pc_prior"}
            }]
        });
        assert!(
            project_canonical_terminal(
                &terminal_from_json(&later_call),
                &platform(),
                digest(),
                &ids()
            )
            .is_err(),
            "a loose caller string must not become authority"
        );
        project_canonical_terminal_with_hosted_programs(
            &terminal_from_json(&later_call),
            &platform(),
            digest(),
            &ids(),
            prior_program_context(),
        )
        .expect("exact prior program caller");

        let later_output = json!({
            "id":"resp_later_output","object":"response","status":"completed","model":MODEL,
            "output":[{
                "type":"program_output","id":"o_later","call_id":"pc_prior",
                "result":"done","status":"completed"
            }]
        });
        project_canonical_terminal_with_hosted_programs(
            &terminal_from_json(&later_output),
            &platform(),
            digest(),
            &ids(),
            prior_program_context(),
        )
        .expect("one exact prior program terminal");

        let mut stream = ResponsesCanonicalObservationProjector::new_with_hosted_programs(
            platform(),
            digest(),
            None,
            ids(),
            prior_program_context(),
        );
        let _ = stream.project(ResponsesStreamEventKind::ResponseCreated {
            response_id: Some("resp_stream_later".to_owned()),
            model: Some(MODEL.to_owned()),
        });
        let emitted = add_observed_item(&mut stream, 0, later_call["output"][0].clone());
        assert!(!stream.gapped());
        assert!(emitted.iter().any(|event| matches!(
            event,
            ModelEvent::BlockStarted(started)
                if matches!(started.block(), BlockStart::ToolCall { correlation, .. }
                    if correlation.caller_lineage()
                        .provider_programmatic_execution_id() == Some("pc_prior"))
        )));

        let mut final_stream = ResponsesCanonicalObservationProjector::new_with_hosted_programs(
            platform(),
            digest(),
            None,
            ids(),
            prior_program_context(),
        );
        let _ = final_stream.project(ResponsesStreamEventKind::ResponseCreated {
            response_id: Some("resp_stream_final".to_owned()),
            model: Some(MODEL.to_owned()),
        });
        let _ = add_observed_item(&mut final_stream, 0, later_output["output"][0].clone());
        assert!(!final_stream.gapped());
        assert_eq!(final_stream.hosted_programs.live_program_count(), 0);
    }

    #[test]
    fn program_terminal_mutations_fail_one_to_one_correlation() {
        let platform = ResponsesCanonicalDialectData::openai_platform(
            ModelId::new(MODEL).expect("model"),
            [ResponsesCanonicalTool::local_function(
                ToolName::new("lookup").expect("tool"),
            )],
        )
        .expect("platform data");
        let mutations = [
            json!([{
                "type":"function_call","id":"f","call_id":"fc","name":"lookup",
                "arguments":"{}","caller":{"type":"program","caller_id":"unmatched"}
            }]),
            json!([
                {"type":"program_output","id":"o","call_id":"pc","result":"early","status":"completed"},
                {"type":"program","id":"p","call_id":"pc","code":"x","fingerprint":"fp"}
            ]),
            json!([
                {"type":"program","id":"p","call_id":"pc","code":"x","fingerprint":"fp"},
                {"type":"program_output","id":"o1","call_id":"pc","result":"one","status":"completed"},
                {"type":"program_output","id":"o2","call_id":"pc","result":"two","status":"completed"}
            ]),
        ];
        for (index, output) in mutations.into_iter().enumerate() {
            let value = json!({
                "id":format!("resp_mutation_{index}"),"object":"response",
                "status":"completed","model":MODEL,"output":output
            });
            assert!(
                project_canonical_terminal(
                    &terminal_from_json(&value),
                    &platform,
                    digest(),
                    &ids()
                )
                .is_err(),
                "program mutation {index} was admitted"
            );
        }
    }

    #[test]
    fn two_live_programs_may_complete_out_of_order_in_aggregate_and_stream() {
        let platform = ResponsesCanonicalDialectData::openai_platform(
            ModelId::new(MODEL).expect("model"),
            [ResponsesCanonicalTool::local_function(
                ToolName::new("lookup").expect("tool"),
            )],
        )
        .expect("platform data");
        let output = json!([
            {"type":"program","id":"p1","call_id":"pc1","code":"x","fingerprint":"fp1"},
            {"type":"program","id":"p2","call_id":"pc2","code":"y","fingerprint":"fp2"},
            {"type":"program_output","id":"o2","call_id":"pc2","result":"second first","status":"completed"},
            {"type":"program_output","id":"o1","call_id":"pc1","result":"first second","status":"completed"}
        ]);
        let value = json!({
            "id":"resp_out_of_order_programs","object":"response",
            "status":"completed","model":MODEL,"output":output
        });

        let projected =
            project_canonical_terminal(&terminal_from_json(&value), &platform, digest(), &ids())
                .expect("exact call IDs allow non-FIFO program completion");
        let observed_call_ids = projected
            .canonical_output()
            .iter()
            .map(|item| {
                let opaque = item
                    .blocks()
                    .iter()
                    .find_map(|block| match block {
                        ContentBlock::Opaque(opaque) => Some(opaque),
                        _ => None,
                    })
                    .expect("program item replay");
                let native: Value = serde_json::from_str(opaque.replay().payload().get())
                    .expect("exact native replay JSON");
                native["call_id"]
                    .as_str()
                    .expect("program call_id")
                    .to_owned()
            })
            .collect::<Vec<_>>();
        assert_eq!(observed_call_ids, ["pc1", "pc2", "pc2", "pc1"]);

        let mut stream = observation_projector(platform);
        for (output_index, item) in output.as_array().expect("output array").iter().enumerate() {
            let _ = add_observed_item(
                &mut stream,
                u32::try_from(output_index).expect("bounded fixture index"),
                item.clone(),
            );
            assert!(
                !stream.gapped(),
                "provider-observed item {output_index} gapped"
            );
        }
        assert_eq!(stream.hosted_programs.live_program_count(), 0);
    }

    #[test]
    fn inherited_program_terminal_and_caller_mutations_fail_aggregate_and_stream_projection() {
        let platform = ResponsesCanonicalDialectData::openai_platform(
            ModelId::new(MODEL).expect("model"),
            [ResponsesCanonicalTool::local_function(
                ToolName::new("lookup").expect("tool"),
            )],
        )
        .expect("platform data");
        let mutations = [
            json!([
                {"type":"program_output","id":"o1","call_id":"pc_prior","result":"done","status":"completed"},
                {"type":"program_output","id":"o2","call_id":"pc_prior","result":"duplicate","status":"completed"}
            ]),
            json!([
                {"type":"program_output","id":"o","call_id":"pc_prior","result":"too early","status":"completed"},
                {"type":"function_call","id":"f","call_id":"fc","name":"lookup","arguments":"{}","caller":{"type":"program","caller_id":"pc_prior"}}
            ]),
            json!([
                {"type":"program_output","id":"o","call_id":"unmatched","result":"wrong","status":"completed"}
            ]),
            json!([
                {"type":"function_call","id":"f","call_id":"fc","name":"lookup","arguments":"{}","caller":{"type":"program","caller_id":"unmatched"}}
            ]),
        ];
        for (index, output) in mutations.into_iter().enumerate() {
            let value = json!({
                "id":format!("resp_inherited_mutation_{index}"),
                "object":"response", "status":"completed", "model":MODEL,
                "output":output
            });
            assert!(
                project_canonical_terminal_with_hosted_programs(
                    &terminal_from_json(&value),
                    &platform,
                    digest(),
                    &ids(),
                    prior_program_context(),
                )
                .is_err(),
                "inherited aggregate mutation {index} was admitted"
            );

            let mut stream = ResponsesCanonicalObservationProjector::new_with_hosted_programs(
                platform.clone(),
                digest(),
                None,
                ids(),
                prior_program_context(),
            );
            let _ = stream.project(ResponsesStreamEventKind::ResponseCreated {
                response_id: Some(format!("resp_stream_mutation_{index}")),
                model: Some(MODEL.to_owned()),
            });
            for (output_index, item) in value["output"]
                .as_array()
                .expect("mutation output")
                .iter()
                .enumerate()
            {
                let _ = add_observed_item(
                    &mut stream,
                    u32::try_from(output_index).expect("bounded fixture index"),
                    item.clone(),
                );
            }
            assert!(
                stream.gapped(),
                "inherited stream mutation {index} was admitted"
            );
        }
    }

    #[test]
    fn program_artifacts_and_callers_are_openai_platform_only_route_matrix() {
        let value = json!({
            "id":"resp_ptc_matrix","object":"response","status":"completed","model":MODEL,
            "output":[
                {"type":"program","id":"p1","call_id":"pc1","code":"await lookup({})","fingerprint":"opaque"},
                {"type":"function_call","id":"f1","call_id":"fc1","name":"lookup","arguments":"{}","caller":{"type":"program","caller_id":"pc1"}},
                {"type":"program_output","id":"o1","call_id":"pc1","result":"ok","status":"completed"}
            ]
        });
        let terminal = terminal_from_json(&value);
        let tool =
            || ResponsesCanonicalTool::local_function(ToolName::new("lookup").expect("tool"));
        let routes = [
            (
                "OpenAI Platform",
                ResponsesCanonicalDialectData::openai_platform(
                    ModelId::new(MODEL).expect("model"),
                    [tool()],
                )
                .expect("OpenAI Platform data"),
                true,
            ),
            (
                "Codex",
                ResponsesCanonicalDialectData::codex(ModelId::new(MODEL).expect("model"), [tool()])
                    .expect("Codex data"),
                false,
            ),
            (
                "Grok/SuperGrok",
                ResponsesCanonicalDialectData::grok(ModelId::new(MODEL).expect("model"), [tool()])
                    .expect("Grok data"),
                false,
            ),
            (
                "xAI Platform",
                ResponsesCanonicalDialectData::xai_platform(ModelId::new(MODEL).expect("model"))
                    .expect("xAI Platform data"),
                false,
            ),
            (
                "OpenRouter",
                ResponsesCanonicalDialectData::openrouter(
                    ModelId::new(MODEL).expect("model"),
                    [tool()],
                )
                .expect("OpenRouter data"),
                false,
            ),
        ];

        for (route, dialect, admitted) in routes {
            assert_eq!(
                project_canonical_terminal(&terminal, &dialect, digest(), &ids()).is_ok(),
                admitted,
                "{route} terminal program artifact policy drifted"
            );
            let mut projector = observation_projector(dialect);
            let emitted = add_observed_item(&mut projector, 0, value["output"][0].clone());
            assert_eq!(
                !projector.gapped(),
                admitted,
                "{route} streaming program artifact policy drifted"
            );
            if !admitted {
                assert!(emitted.is_empty(), "{route} promoted a program artifact");
            }
        }
    }

    #[test]
    fn bounded_unknown_output_is_provider_neutral() {
        let value = json!({
            "id":"resp_future","object":"response","status":"completed","model":MODEL,
            "output":[{"type":"future_provider_item","id":"future1","future":{"nested":true}}]
        });
        let terminal = terminal_from_json(&value);
        let platform =
            ResponsesCanonicalDialectData::xai_platform(ModelId::new(MODEL).expect("model"))
                .expect("platform data");
        let projected = project_canonical_terminal(&terminal, &platform, digest(), &ids())
            .expect("bounded unknown stays opaque");
        assert!(projected.response().output().iter().all(|item| {
            item.blocks()
                .iter()
                .all(|block| !matches!(block, ContentBlock::ToolCall(_)))
        }));
        let supergrok =
            ResponsesCanonicalDialectData::grok(ModelId::new(MODEL).expect("model"), [])
                .expect("SuperGrok data");
        assert!(project_canonical_terminal(&terminal, &supergrok, digest(), &ids()).is_ok());

        let duplicate = json!({
            "id":"resp_future_duplicate","object":"response","status":"completed","model":MODEL,
            "output":[
                {"type":"future_provider_item","id":"future1","future":{"nested":true}},
                {"type":"future_provider_item","id":"future1","future":{"nested":true}}
            ]
        });
        assert!(
            ResponsesResponse::parse(
                &duplicate,
                ResponsesUsagePolicy::DeriveTotalWhenOmitted,
                ResponsesResponseLimits::default(),
            )
            .is_err()
        );
    }
}
