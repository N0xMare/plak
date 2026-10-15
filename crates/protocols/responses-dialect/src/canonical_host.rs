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

mod terminal;
pub use terminal::{
    ResponsesCanonicalTerminal, project_canonical_terminal,
    project_canonical_terminal_with_hosted_programs,
};
use terminal::{
    local_or_advertised_tool, message_phase, output_identity, require_lite_advertised_tool,
    tool_caller_lineage, validate_opaque_kind,
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
    Local,
    /// Execute through one exact configured MCP server.
    Mcp(McpServerId),
}

impl ResponsesCanonicalToolOwner {
    fn canonical(&self) -> ToolExecutionOwner {
        match self {
            Self::Local => ToolExecutionOwner::Local,
            Self::Mcp(server) => ToolExecutionOwner::Mcp {
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
            ResponsesCanonicalToolOwner::Local,
        )
    }

    /// A locally executed arbitrary-input custom tool.
    #[must_use]
    pub const fn local_custom(name: ToolName) -> Self {
        Self::new(
            name,
            ResponsesCanonicalToolKind::Custom,
            ResponsesCanonicalToolOwner::Local,
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
            let projected = if let Some(projection) = request.tool_projection() {
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

    #[allow(
        clippy::too_many_lines,
        reason = "one linear host step whose ordering is the invariant"
    )]
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

    #[allow(
        clippy::too_many_lines,
        reason = "one linear host step whose ordering is the invariant"
    )]
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

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "unit tests use deterministic fixture construction"
)]
mod tests;
