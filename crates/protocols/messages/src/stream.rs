//! Messages-family incremental stream state machine and canonical projection.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use plakous_http::SseError;
use plakous_model::{
    Actor, BlockCompleted, BlockStart, BlockStarted, CallId, Clock, ContentBlock, ConversationItem,
    Digest, DigestDomain, EntryId, FamilyArtifactProducerFacts, FinishDetails, FinishReason,
    IdGenerator, ModelEvent, ModelId, ModelProviderRequestId, ModelResponse, ModelResponseMetadata,
    NamespaceId, OpaqueBlock, OpaqueFallback, OutputLocation, Provenance, ProviderEvent,
    ReasoningBlock, ReasoningDelta, ReasoningReplayBound, ReasoningVisibility,
    ReasoningVisibilityBound, RefusalBlock, ResponseStarted, TextBlock, TextDelta, ToolCallBlock,
    ToolCallState, ToolCallerDisposition, ToolCallerLineage, ToolCorrelation, ToolExecutionOwner,
    ToolInputDelta, ToolName, Usage, UsageUpdate, ValidatedRequestPlan,
};
use serde::Deserialize;
use serde_json::value::RawValue;

use crate::request::{
    CODE_EXECUTION_RESULT_REPLAY_KIND, MessagesRequestPolicy, REDACTED_REPLAY_KIND,
    SIGNATURE_REPLAY_KIND, ToolOwnership, bind_replay_payload, messages_artifact_binding,
    messages_artifact_configuration_identity, messages_tool_set_identity,
};
use crate::{
    MessagesEventKind, MessagesProviderUsage as ProviderUsage, MessagesStreamLimits,
    MessagesUsageUpdate, validate_named_event,
};

pub use plakous_http::{SseDecoder, SseLimits, SseMessage};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Closed family projector error kind.
pub enum MessagesStreamErrorKind {
    /// Framing, JSON, or canonical construction failed.
    InvalidResponse,
    /// A valid event violated stream ordering.
    InvalidEvent,
    /// Provider usage could not be normalized exactly.
    InvalidAccounting,
    /// A recognized wire semantic is outside the admitted profile.
    UnsupportedSemantic,
    /// An in-band error awaits exact dialect classification.
    ProviderError,
    /// EOF preceded the terminal event.
    IncompleteResponse,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Closed family failure phase.
pub enum MessagesStreamErrorPhase {
    /// Incremental stream projection.
    Stream,
    /// Terminal projection extraction.
    Complete,
}
#[derive(Clone, Debug, Eq, PartialEq)]
/// Redaction-safe neutral projector error with bounded dialect evidence.
pub struct MessagesStreamError {
    kind: MessagesStreamErrorKind,
    phase: MessagesStreamErrorPhase,
    message: String,
    provider_payload: Option<Vec<u8>>,
    visible_output: bool,
    hosted_effects_possible: bool,
}
impl MessagesStreamError {
    fn new(
        kind: MessagesStreamErrorKind,
        phase: MessagesStreamErrorPhase,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            phase,
            message: message.into(),
            provider_payload: None,
            visible_output: false,
            hosted_effects_possible: false,
        }
    }
    fn provider(payload: &[u8], visible_output: bool, hosted_effects_possible: bool) -> Self {
        Self {
            kind: MessagesStreamErrorKind::ProviderError,
            phase: MessagesStreamErrorPhase::Stream,
            message: "Messages stream contained an in-band provider error".into(),
            provider_payload: Some(payload.to_vec()),
            visible_output,
            hosted_effects_possible,
        }
    }
    fn incomplete() -> Self {
        Self::new(
            MessagesStreamErrorKind::IncompleteResponse,
            MessagesStreamErrorPhase::Complete,
            "Messages stream ended before message_stop",
        )
    }
    /// Returns the closed error kind.
    pub const fn kind(&self) -> MessagesStreamErrorKind {
        self.kind
    }
    /// Returns the closed failure phase.
    pub const fn phase(&self) -> MessagesStreamErrorPhase {
        self.phase
    }
    /// Returns the neutral safe diagnostic.
    pub fn message(&self) -> &str {
        &self.message
    }
    /// Returns bounded in-band error bytes for dialect classification.
    pub fn provider_payload(&self) -> Option<&[u8]> {
        self.provider_payload.as_deref()
    }
    /// Reports whether visible output preceded the error.
    pub const fn visible_output(&self) -> bool {
        self.visible_output
    }
    /// Reports whether provider-hosted effects may have occurred.
    pub const fn hosted_effects_possible(&self) -> bool {
        self.hosted_effects_possible
    }
}
impl fmt::Display for MessagesStreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for MessagesStreamError {}
fn invalid_response(message: impl Into<String>) -> MessagesStreamError {
    MessagesStreamError::new(
        MessagesStreamErrorKind::InvalidResponse,
        MessagesStreamErrorPhase::Stream,
        message,
    )
}

/// Exact dialect values supplied by an integration to neutral projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessagesStreamDialectHooks {
    programmatic_tool: Option<ProgrammaticToolHooks>,
    extensions: MessagesStreamExtensions,
}

/// Closed additive stream grammar selected by a Messages dialect.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MessagesStreamExtensions {
    /// Strict Messages grammar and lifecycle.
    #[default]
    Strict,
    /// Reviewed Claude OAuth additions from the pinned Claude Code envelope.
    ClaudeOauthV1,
    /// Reviewed OpenRouter Messages additions and terminal `[DONE]` framing.
    OpenRouterV1,
}
#[derive(Clone, Debug, Eq, PartialEq)]
struct ProgrammaticToolHooks {
    caller: String,
    namespace: NamespaceId,
}
impl MessagesStreamDialectHooks {
    /// Creates exact programmatic-tool lineage values.
    pub fn new(caller: impl Into<String>, namespace: NamespaceId) -> Self {
        Self {
            programmatic_tool: Some(ProgrammaticToolHooks {
                caller: caller.into(),
                namespace,
            }),
            extensions: MessagesStreamExtensions::Strict,
        }
    }

    /// Creates a stream policy admitting only ordinary client-tool calls.
    pub const fn client_tools_only() -> Self {
        Self {
            programmatic_tool: None,
            extensions: MessagesStreamExtensions::Strict,
        }
    }

    /// Selects one closed additive stream grammar.
    #[must_use]
    pub const fn with_extensions(mut self, extensions: MessagesStreamExtensions) -> Self {
        self.extensions = extensions;
        self
    }

    fn programmatic_tool_caller(&self) -> Option<&str> {
        self.programmatic_tool
            .as_ref()
            .map(|hooks| hooks.caller.as_str())
    }
    fn programmatic_tool_namespace(&self) -> Option<NamespaceId> {
        self.programmatic_tool
            .as_ref()
            .map(|hooks| hooks.namespace.clone())
    }

    const fn extensions(&self) -> MessagesStreamExtensions {
        self.extensions
    }
}

#[derive(Clone, Debug)]
/// Credential-free request, tool, and dialect values required for projection.
pub struct PreparedMessagesStreamBinding {
    request_policy: MessagesRequestPolicy,
    plan_digest: Digest,
    tool_ownership: ToolOwnership,
    dialect: MessagesStreamDialectHooks,
    artifact_plan: Option<ValidatedRequestPlan>,
    operation_digest: Option<Digest>,
    active_artifact_lineage: crate::artifact::MessagesArtifactLineage,
    expected_container: Option<String>,
}
impl PreparedMessagesStreamBinding {
    /// Creates an exact neutral stream binding.
    pub fn new(
        request_policy: MessagesRequestPolicy,
        plan_digest: Digest,
        tool_ownership: ToolOwnership,
        dialect: MessagesStreamDialectHooks,
    ) -> Self {
        Self {
            request_policy,
            plan_digest,
            tool_ownership,
            dialect,
            artifact_plan: None,
            operation_digest: None,
            active_artifact_lineage: crate::artifact::MessagesArtifactLineage::default(),
            expected_container: None,
        }
    }
    /// Enables exact hosted artifact production for one validated operation.
    #[must_use]
    pub fn with_artifact_production(
        mut self,
        plan: ValidatedRequestPlan,
        operation_digest: Digest,
    ) -> Self {
        self.artifact_plan = Some(plan);
        self.operation_digest = Some(operation_digest);
        self
    }
    pub(crate) fn with_active_artifact_lineage(
        mut self,
        container: Option<String>,
        lineage: Option<crate::artifact::MessagesArtifactLineage>,
    ) -> Self {
        self.expected_container = container;
        self.active_artifact_lineage = lineage.unwrap_or_default();
        self
    }
    /// Returns the exact protocol identity.
    pub const fn protocol(&self) -> &plakous_model::ProtocolId {
        self.request_policy.protocol()
    }
    /// Returns the required provider-reported model when pinned.
    pub const fn expected_reported_model(&self) -> Option<&ModelId> {
        self.request_policy.expected_reported_model()
    }
    /// Returns the canonical plan identity.
    pub const fn plan_digest(&self) -> Digest {
        self.plan_digest
    }
    /// Returns exact tool ownership.
    pub const fn tool_ownership(&self) -> &ToolOwnership {
        &self.tool_ownership
    }
    /// Reports whether this prepared request admits provider-hosted execution.
    pub const fn hosted_effect_capable(&self) -> bool {
        self.request_policy.hosted_effect_capable()
    }
    fn bind_replay(
        &self,
        actual_model: ModelId,
        kind: &'static str,
        payload: Box<RawValue>,
    ) -> Result<plakous_model::ReplayEnvelope, MessagesStreamError> {
        bind_replay_payload(&self.request_policy, actual_model, kind, payload)
            .map_err(|error| invalid_response(error.message()))
    }
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "dialect boundary matches SseError by value for exhaustiveness"
)]
fn map_sse_error(error: SseError) -> MessagesStreamError {
    match error {
        SseError::InvalidUtf8 => invalid_response("Messages SSE field was not valid UTF-8"),
        SseError::ResourceLimit("SSE line bytes") => {
            invalid_response("Messages SSE line exceeded the byte limit")
        }
        SseError::ResourceLimit("SSE event bytes") => {
            invalid_response("Messages SSE event exceeded the byte limit")
        }
        SseError::ResourceLimit(_) => invalid_response("Messages SSE resource limit exceeded"),
        SseError::AlreadyFailed => {
            invalid_response("Messages SSE decoder is terminal after an earlier error")
        }
        _ => invalid_response("Messages SSE framing error"),
    }
}

fn sse_limits_from_messages(limits: MessagesStreamLimits) -> SseLimits {
    SseLimits {
        max_line_bytes: limits.max_sse_line_bytes(),
        max_event_bytes: limits.max_sse_event_bytes(),
    }
}

/// A documented Messages terminal reason retained without collapsing cases.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MessagesStopReason {
    /// Natural assistant completion.
    EndTurn,
    /// A caller-provided stop sequence matched.
    StopSequence,
    /// One or more client tools were requested.
    ToolUse,
    /// The configured output-token ceiling was reached.
    MaxTokens,
    /// The model's context window was exhausted.
    ModelContextWindowExceeded,
    /// Provider-hosted work paused for continuation; excluded in A1.
    PauseTurn,
    /// The model refused the request.
    Refusal,
    /// The provider requested model fallback; excluded in A1.
    Fallback,
}

impl MessagesStopReason {
    fn parse(value: &str) -> Result<Self, MessagesStreamError> {
        match value {
            "end_turn" => Ok(Self::EndTurn),
            "stop_sequence" => Ok(Self::StopSequence),
            "tool_use" => Ok(Self::ToolUse),
            "max_tokens" => Ok(Self::MaxTokens),
            "model_context_window_exceeded" => Ok(Self::ModelContextWindowExceeded),
            "pause_turn" => Ok(Self::PauseTurn),
            "refusal" => Ok(Self::Refusal),
            "fallback" => Ok(Self::Fallback),
            _ => Err(MessagesStreamError::new(
                MessagesStreamErrorKind::UnsupportedSemantic,
                MessagesStreamErrorPhase::Stream,
                "unsupported Messages stop_reason",
            )),
        }
    }

    const fn wire_name(self) -> &'static str {
        match self {
            Self::EndTurn => "end_turn",
            Self::StopSequence => "stop_sequence",
            Self::ToolUse => "tool_use",
            Self::MaxTokens => "max_tokens",
            Self::ModelContextWindowExceeded => "model_context_window_exceeded",
            Self::PauseTurn => "pause_turn",
            Self::Refusal => "refusal",
            Self::Fallback => "fallback",
        }
    }
}

/// A known typed `stop_details.type` discriminator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MessagesStopDetailsKind {
    /// The terminal tool calls are client-executed.
    ClientTool,
    /// The model refused the request.
    Refusal,
    /// The model's context window was exhausted.
    ModelContextWindowExceeded,
    /// The provider requested fallback; excluded in A1.
    Fallback,
}

impl MessagesStopDetailsKind {
    fn parse(value: &str) -> Result<Self, MessagesStreamError> {
        match value {
            "client_tool" => Ok(Self::ClientTool),
            "refusal" => Ok(Self::Refusal),
            "model_context_window_exceeded" => Ok(Self::ModelContextWindowExceeded),
            "fallback" | "model_fallback" => Ok(Self::Fallback),
            _ => Err(MessagesStreamError::new(
                MessagesStreamErrorKind::UnsupportedSemantic,
                MessagesStreamErrorPhase::Stream,
                "unsupported Messages stop_details type",
            )),
        }
    }

    const fn wire_name(self) -> &'static str {
        match self {
            Self::ClientTool => "client_tool",
            Self::Refusal => "refusal",
            Self::ModelContextWindowExceeded => "model_context_window_exceeded",
            Self::Fallback => "fallback",
        }
    }
}

/// Exact redacted stop-details payload plus its safe discriminator.
#[derive(Clone)]
pub struct MessagesStopDetails {
    kind: MessagesStopDetailsKind,
    raw: Box<RawValue>,
}

impl MessagesStopDetails {
    /// Returns the parsed safe discriminator.
    #[must_use]
    pub const fn kind(&self) -> MessagesStopDetailsKind {
        self.kind
    }

    /// Returns the exact provider payload. Callers must treat it as sensitive.
    #[must_use]
    pub const fn raw(&self) -> &RawValue {
        &self.raw
    }
}

impl fmt::Debug for MessagesStopDetails {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MessagesStopDetails")
            .field("kind", &self.kind)
            .field("raw", &"<redacted>")
            .finish()
    }
}

impl PartialEq for MessagesStopDetails {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind && self.raw.get() == other.raw.get()
    }
}

/// Exact terminal stop metadata for one successfully normalized message.
#[derive(Clone, Debug, PartialEq)]
pub struct MessagesStopOutcome {
    reason: MessagesStopReason,
    stop_sequence: Option<String>,
    details: Option<MessagesStopDetails>,
}

impl MessagesStopOutcome {
    /// Returns the non-collapsed terminal reason.
    #[must_use]
    pub const fn reason(&self) -> MessagesStopReason {
        self.reason
    }

    /// Returns the exact matching stop sequence when applicable.
    #[must_use]
    pub fn stop_sequence(&self) -> Option<&str> {
        self.stop_sequence.as_deref()
    }

    /// Returns exact typed stop details when supplied.
    #[must_use]
    pub const fn details(&self) -> Option<&MessagesStopDetails> {
        self.details.as_ref()
    }
}

#[derive(Debug)]
enum StreamState {
    AwaitingStart,
    Open,
    AwaitingGatewayDone,
    Stopped,
    Failed,
}

#[derive(Debug)]
enum OpenBlock {
    Text {
        text: String,
    },
    Thinking {
        text: String,
        signature: Option<Box<RawValue>>,
        visibility: Option<ReasoningVisibility>,
    },
    Redacted {
        data: Box<RawValue>,
    },
    ToolUse(ToolUseInput),
    DeferredProgrammaticToolUse {
        call_id: CallId,
        remote_id: String,
        name: ToolName,
        owner: ToolExecutionOwner,
        execution_id: String,
        json: String,
    },
    Refusal {
        text: String,
    },
    Complete(ContentBlock),
}

#[derive(Debug)]
struct ToolUseInput {
    call_id: CallId,
    remote_id: String,
    name: ToolName,
    owner: ToolExecutionOwner,
    state: ToolCallState,
    correlation: ToolCorrelation,
    json: String,
}

impl OpenBlock {
    fn retained_bytes(&self) -> usize {
        match self {
            Self::Text { text } | Self::Refusal { text } => text.len(),
            Self::Thinking {
                text, signature, ..
            } => text
                .len()
                .saturating_add(signature.as_ref().map_or(0, |value| value.get().len())),
            Self::Redacted { data } => data.get().len(),
            Self::Complete(ContentBlock::Opaque(block)) => block.replay().payload().get().len(),
            Self::Complete(_) => 0,
            Self::ToolUse(ToolUseInput {
                remote_id,
                name,
                correlation,
                json,
                ..
            }) => remote_id
                .len()
                .saturating_add(name.as_str().len())
                .saturating_add(correlation.container_id().map_or(0, str::len))
                .saturating_add(
                    correlation
                        .caller_lineage()
                        .provider_programmatic_execution_id()
                        .map_or(0, str::len),
                )
                .saturating_add(json.len()),
            Self::DeferredProgrammaticToolUse {
                remote_id,
                name,
                execution_id,
                json,
                ..
            } => remote_id
                .len()
                .saturating_add(name.as_str().len())
                .saturating_add(execution_id.len())
                .saturating_add(json.len()),
        }
    }
}

/// Incremental Messages state machine producing Messages-normalized events.
pub struct MessagesStreamProjector {
    prepared: PreparedMessagesStreamBinding,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGenerator>,
    sse: SseDecoder,
    state: StreamState,
    item_id: Option<EntryId>,
    message_id: Option<String>,
    reported_model: Option<ModelId>,
    provider_request_id: Option<ModelProviderRequestId>,
    next_block_index: u32,
    remote_tool_ids: BTreeSet<String>,
    explicit_direct_tool_ids: BTreeSet<String>,
    server_tool_ids: BTreeSet<String>,
    code_execution_result_ids: BTreeSet<String>,
    server_lineage: crate::artifact::MessagesArtifactLineage,
    container_id: Option<String>,
    open_blocks: BTreeMap<u32, OpenBlock>,
    blocks: BTreeMap<u32, ContentBlock>,
    deferred_programmatic_blocks: Vec<(u32, OpenBlock)>,
    closed_client_blocks: Vec<(u32, ToolUseInput)>,
    usage: Option<Usage>,
    provider_usage: Option<ProviderUsage>,
    stop_reason: Option<MessagesStopReason>,
    stop_sequence: Option<String>,
    stop_details: Option<MessagesStopDetails>,
    stop_outcome: Option<MessagesStopOutcome>,
    terminal_response: Option<ModelResponse>,
    events_seen: usize,
    wire_bytes_seen: usize,
    visible_output: bool,
    hosted_effects_possible: bool,
    eof_sealed: bool,
    limits: MessagesStreamLimits,
}

impl fmt::Debug for MessagesStreamProjector {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MessagesStreamProjector")
            .field("prepared", &self.prepared)
            .field("state", &self.state)
            .field("message_id", &self.message_id)
            .field("reported_model", &self.reported_model)
            .field(
                "provider_request_id",
                &self.provider_request_id.as_ref().map(|_| "<redacted>"),
            )
            .field("next_block_index", &self.next_block_index)
            .field("events_seen", &self.events_seen)
            .field("wire_bytes_seen", &self.wire_bytes_seen)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

/// Complete neutral terminal projection; dialect wrappers add attempt authority.
pub struct MessagesTerminalProjection {
    response: ModelResponse,
    reported_model: ModelId,
    stop: MessagesStopOutcome,
}

impl MessagesTerminalProjection {
    /// Decomposes the terminal projection for exact integration binding.
    pub fn into_parts(self) -> (ModelResponse, ModelId, MessagesStopOutcome) {
        (self.response, self.reported_model, self.stop)
    }
}

impl MessagesStreamProjector {
    /// Creates a decoder with injected canonical identity and time sources.
    ///
    /// # Errors
    ///
    /// Returns an invalid-response error if the stable protocol identity cannot
    /// be constructed.
    pub fn new(
        prepared: PreparedMessagesStreamBinding,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
    ) -> Result<Self, MessagesStreamError> {
        Self::with_binding(prepared, clock, ids, MessagesStreamLimits::default())
    }

    /// Creates a decoder with injected identities and explicit resource limits.
    ///
    /// # Errors
    ///
    /// Returns an invalid-response error if the stable protocol identity cannot
    /// be constructed.
    pub fn with_limits(
        prepared: PreparedMessagesStreamBinding,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
        limits: MessagesStreamLimits,
    ) -> Result<Self, MessagesStreamError> {
        Self::with_binding(prepared, clock, ids, limits)
    }

    #[allow(
        clippy::unnecessary_wraps,
        reason = "constructor remains fail-closed as dialect hooks expand"
    )]
    fn with_binding(
        prepared: PreparedMessagesStreamBinding,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
        limits: MessagesStreamLimits,
    ) -> Result<Self, MessagesStreamError> {
        let server_tool_ids = prepared
            .active_artifact_lineage
            .server_ids()
            .map(str::to_owned)
            .collect();
        let code_execution_result_ids = prepared
            .active_artifact_lineage
            .completed_result_ids()
            .map(str::to_owned)
            .collect();
        let server_lineage = prepared.active_artifact_lineage.clone();
        Ok(Self {
            prepared,
            clock,
            ids,
            sse: SseDecoder::new(sse_limits_from_messages(limits)),
            state: StreamState::AwaitingStart,
            item_id: None,
            message_id: None,
            reported_model: None,
            provider_request_id: None,
            next_block_index: 0,
            remote_tool_ids: BTreeSet::new(),
            explicit_direct_tool_ids: BTreeSet::new(),
            server_tool_ids,
            code_execution_result_ids,
            server_lineage,
            container_id: None,
            open_blocks: BTreeMap::new(),
            blocks: BTreeMap::new(),
            deferred_programmatic_blocks: Vec::new(),
            closed_client_blocks: Vec::new(),
            usage: None,
            provider_usage: None,
            stop_reason: None,
            stop_sequence: None,
            stop_details: None,
            stop_outcome: None,
            terminal_response: None,
            events_seen: 0,
            wire_bytes_seen: 0,
            visible_output: false,
            hosted_effects_possible: false,
            eof_sealed: false,
            limits,
        })
    }

    /// Binds a validated provider request ID before any response bytes are decoded.
    ///
    /// The ID is projected into both response-start and terminal neutral
    /// metadata. Formatting the decoder never exposes it.
    #[must_use]
    pub fn with_provider_request_id(mut self, provider_request_id: ModelProviderRequestId) -> Self {
        self.provider_request_id = Some(provider_request_id);
        self
    }

    pub(crate) fn completed_items(&self) -> u64 {
        // Closed local tool inputs wait for the response terminal before projection.
        u64::from(self.next_block_index).saturating_sub(self.open_blocks.len() as u64)
    }

    /// Pushes arbitrarily fragmented SSE bytes and returns canonical events.
    ///
    /// # Errors
    ///
    /// Any framing, JSON, state, unsupported-block, or resource-limit error is terminal.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<ModelEvent>, MessagesStreamError> {
        if matches!(self.state, StreamState::Failed | StreamState::Stopped) {
            return Err(self.fail("bytes arrived after terminal Messages state"));
        }
        self.wire_bytes_seen = self.wire_bytes_seen.saturating_add(bytes.len());
        if self.wire_bytes_seen > self.limits.max_total_stream_bytes() {
            return Err(self.fail("Messages stream exceeded the total byte limit"));
        }
        let frames = match self.sse.push(bytes) {
            Ok(frames) => frames,
            Err(error) => {
                self.poison();
                return Err(map_sse_error(error));
            }
        };
        self.process_frames(frames)
    }

    /// Validates EOF. A successful stream must already contain `message_stop`.
    ///
    /// # Errors
    ///
    /// Returns [`MessagesStreamErrorKind::IncompleteResponse`] for any nonterminal stream.
    pub fn finish_eof(&mut self) -> Result<Vec<ModelEvent>, MessagesStreamError> {
        if self.eof_sealed || matches!(self.state, StreamState::Failed) {
            return Err(self.fail("Messages stream EOF was already consumed or failed"));
        }
        let frames = match self.sse.finish() {
            Ok(frames) => frames,
            Err(error) => {
                self.poison();
                return Err(map_sse_error(error));
            }
        };
        let events = self.process_frames(frames)?;
        if matches!(
            self.state,
            StreamState::Stopped | StreamState::AwaitingGatewayDone
        ) {
            self.state = StreamState::Stopped;
            self.eof_sealed = true;
            Ok(events)
        } else {
            self.poison();
            Err(MessagesStreamError::incomplete())
        }
    }

    /// Reports whether decoded wire evidence makes provider-hosted effects possible.
    #[must_use]
    pub const fn hosted_effects_possible(&self) -> bool {
        self.hosted_effects_possible
    }

    /// Returns the actual model identity reported in `message_start`.
    #[must_use]
    pub const fn reported_model(&self) -> Option<&ModelId> {
        self.reported_model.as_ref()
    }

    /// Returns the provider message identity from `message_start`.
    ///
    /// This is not the HTTP `request-id` response header.
    #[must_use]
    pub fn message_id(&self) -> Option<&str> {
        self.message_id.as_deref()
    }

    /// Returns HTTP-supplied or dialect-derived provider request attribution.
    #[must_use]
    pub const fn provider_request_id(&self) -> Option<&ModelProviderRequestId> {
        self.provider_request_id.as_ref()
    }

    /// Returns exact typed terminal stop metadata after successful completion.
    #[must_use]
    pub const fn stop_outcome(&self) -> Option<&MessagesStopOutcome> {
        self.stop_outcome.as_ref()
    }

    /// Consumes a completed projector into the neutral terminal projection.
    pub fn into_terminal_projection(
        self,
    ) -> Result<MessagesTerminalProjection, MessagesStreamError> {
        if !self.eof_sealed || !matches!(self.state, StreamState::Stopped) {
            return Err(MessagesStreamError::incomplete());
        }
        let response = self
            .terminal_response
            .ok_or_else(MessagesStreamError::incomplete)?;
        let reported_model = self.reported_model.ok_or_else(|| {
            MessagesStreamError::new(
                MessagesStreamErrorKind::InvalidResponse,
                MessagesStreamErrorPhase::Complete,
                "Messages terminal response omitted its provider-reported model",
            )
        })?;
        let stop = self.stop_outcome.ok_or_else(|| {
            MessagesStreamError::new(
                MessagesStreamErrorKind::InvalidResponse,
                MessagesStreamErrorPhase::Complete,
                "Messages terminal response omitted its typed stop outcome",
            )
        })?;
        Ok(MessagesTerminalProjection {
            response,
            reported_model,
            stop,
        })
    }

    fn process_frames(
        &mut self,
        frames: Vec<SseMessage>,
    ) -> Result<Vec<ModelEvent>, MessagesStreamError> {
        let mut output = Vec::new();
        for frame in frames {
            if matches!(self.state, StreamState::AwaitingGatewayDone) {
                self.process_gateway_done(&frame)?;
                continue;
            }
            if self.prepared.hosted_effect_capable()
                && raw_block_start_has_hosted_effect(frame.data())
            {
                self.hosted_effects_possible = true;
            }
            if matches!(self.state, StreamState::Stopped) {
                return Err(self.fail("Messages SSE event arrived after message_stop"));
            }
            self.events_seen = self.events_seen.saturating_add(1);
            if self.events_seen > self.limits.max_events() {
                return Err(self.fail("Messages stream exceeded the event limit"));
            }
            if let Err(error) = self.process_frame(&frame, &mut output) {
                self.poison();
                return Err(error);
            }
        }
        Ok(output)
    }

    fn process_gateway_done(&mut self, frame: &SseMessage) -> Result<(), MessagesStreamError> {
        if frame.event() != Some("data") || frame.data() != "[DONE]" {
            return Err(self.fail("OpenRouter Messages stream omitted its exact [DONE] trailer"));
        }
        self.state = StreamState::Stopped;
        Ok(())
    }

    fn process_frame(
        &mut self,
        frame: &SseMessage,
        output: &mut Vec<ModelEvent>,
    ) -> Result<(), MessagesStreamError> {
        let envelope: EventEnvelope = serde_json::from_str(frame.data())
            .map_err(|error| invalid_response(format!("invalid Messages event JSON: {error}")))?;
        validate_named_event(frame.event(), &envelope.kind)
            .map_err(|message| self.fail(message))?;
        match MessagesEventKind::classify(&envelope.kind) {
            MessagesEventKind::MessageStart => self.message_start(frame.data(), output),
            MessagesEventKind::ContentBlockStart => self.block_start(frame.data(), output),
            MessagesEventKind::ContentBlockDelta => self.block_delta(frame.data(), output),
            MessagesEventKind::ContentBlockStop => self.block_stop(frame.data(), output),
            MessagesEventKind::MessageDelta => self.message_delta(frame.data(), output),
            MessagesEventKind::MessageStop => self.message_stop(frame.data(), output),
            MessagesEventKind::Ping => {
                parse_event::<PingEvent>(frame.data())?;
                let payload = RawValue::from_string(frame.data().to_owned())
                    .map_err(|error| invalid_response(error.to_string()))?;
                output.push(ModelEvent::ProviderEvent(ProviderEvent::new(
                    "ping", payload,
                )));
                Ok(())
            }
            MessagesEventKind::Error => Err(MessagesStreamError::provider(
                frame.data().as_bytes(),
                self.visible_output,
                self.hosted_effects_possible,
            )),
            MessagesEventKind::Citation => Err(MessagesStreamError::new(
                MessagesStreamErrorKind::UnsupportedSemantic,
                MessagesStreamErrorPhase::Stream,
                "Messages citations are explicitly excluded by adapter revision 1",
            )),
            MessagesEventKind::Unknown => {
                if frame.data().len() > self.limits.max_unknown_event_bytes() {
                    return Err(
                        self.fail("unknown Messages event exceeded the retained byte limit")
                    );
                }
                let payload = RawValue::from_string(frame.data().to_owned())
                    .map_err(|error| invalid_response(error.to_string()))?;
                output.push(ModelEvent::ProviderEvent(ProviderEvent::new(
                    envelope.kind,
                    payload,
                )));
                Ok(())
            }
        }
    }

    fn message_start(
        &mut self,
        data: &str,
        output: &mut Vec<ModelEvent>,
    ) -> Result<(), MessagesStreamError> {
        if !matches!(self.state, StreamState::AwaitingStart) {
            return Err(self.fail("message_start was duplicated or out of order"));
        }
        let (event, cache_diagnosis) =
            parse_message_start(data, self.prepared.dialect.extensions())?;
        if event.message.kind != "message" || event.message.role != "assistant" {
            return Err(self.fail("message_start did not contain an assistant message"));
        }
        if !event.message.content.is_empty() {
            return Err(self.fail("streaming message_start unexpectedly contained content"));
        }
        if event.message.id.is_empty() {
            return Err(self.fail("message_start contained an empty message ID"));
        }
        if event.message.stop_reason.is_some()
            || event.message.stop_sequence.is_some()
            || event.message.stop_details.is_some()
        {
            return Err(self.fail("streaming message_start unexpectedly contained terminal state"));
        }
        if let Some(container) = event.message.container {
            self.bind_container(container, "message_start")?;
        }
        let model = ModelId::new(event.message.model)
            .map_err(|error| invalid_response(format!("invalid reported model ID: {error}")))?;
        if self
            .prepared
            .expected_reported_model()
            .is_some_and(|expected| expected != &model)
        {
            return Err(invalid_response(
                "provider-reported Messages model did not match the prepared descriptor",
            ));
        }
        let item_id =
            EntryId::generate(self.clock.as_ref(), self.ids.as_ref()).map_err(|error| {
                invalid_response(format!("failed to generate assistant item ID: {error}"))
            })?;
        let provider_usage = event.message.usage.into_provider();
        let usage = provider_usage
            .into_canonical()
            .map_err(|message| self.invalid_accounting(message))?;
        if self.provider_request_id.is_none()
            && self.prepared.dialect.extensions() == MessagesStreamExtensions::OpenRouterV1
        {
            self.provider_request_id = Some(
                ModelProviderRequestId::new(event.message.id.clone()).map_err(|_| {
                    invalid_response(
                        "OpenRouter message_start ID was unsafe for request attribution",
                    )
                })?,
            );
        }
        let metadata =
            ModelResponseMetadata::new(Some(model.clone()), self.provider_request_id.clone());
        self.message_id = Some(event.message.id);
        self.reported_model = Some(model);
        self.item_id = Some(item_id);
        self.usage = Some(usage);
        self.provider_usage = Some(provider_usage);
        self.state = StreamState::Open;
        if let Some(diagnosis) = cache_diagnosis {
            output.push(ModelEvent::PromptCacheDiagnosis(diagnosis));
        }
        output.push(ModelEvent::ResponseStarted(
            ResponseStarted::new(self.message_id.clone(), self.prepared.plan_digest())
                .with_metadata(metadata),
        ));
        output.push(ModelEvent::Usage(UsageUpdate::new(usage)));
        Ok(())
    }

    #[allow(
        clippy::too_many_lines,
        reason = "keeping every content-block start transition together makes the wire grammar auditable"
    )]
    fn block_start(
        &mut self,
        data: &str,
        output: &mut Vec<ModelEvent>,
    ) -> Result<(), MessagesStreamError> {
        if self.prepared.hosted_effect_capable() && raw_block_start_has_hosted_effect(data) {
            self.hosted_effects_possible = true;
        }
        self.require_open()?;
        if self.stop_reason.is_some() || self.stop_sequence.is_some() || self.stop_details.is_some()
        {
            return Err(self.fail("content block started after terminal message metadata"));
        }
        if self.prepared.dialect.extensions() != MessagesStreamExtensions::OpenRouterV1
            && !self.open_blocks.is_empty()
        {
            return Err(self.fail("content block started before the previous block stopped"));
        }
        if usize::try_from(self.next_block_index)
            .map_or(true, |count| count >= self.limits.max_blocks())
        {
            return Err(self.fail("Messages response exceeded the content-block limit"));
        }
        let event: ContentBlockStartEvent = parse_event(data)?;
        if event.index != self.next_block_index {
            return Err(self.fail("Messages content block indices are not contiguous"));
        }
        let kind: EventEnvelope = serde_json::from_str(event.content_block.get())
            .map_err(|error| invalid_response(format!("invalid content block: {error}")))?;
        let location = OutputLocation::new(0, event.index);
        let item_id = self.item_id()?;
        let block = match kind.kind.as_str() {
            "text" => {
                let value: TextStart = serde_json::from_str(event.content_block.get())
                    .map_err(|error| invalid_response(error.to_string()))?;
                reject_citations(value.citations.as_deref())?;
                output.push(ModelEvent::BlockStarted(BlockStarted::new(
                    item_id,
                    location,
                    BlockStart::Text { phase: None },
                )));
                if !value.text.is_empty() {
                    output.push(ModelEvent::TextDelta(TextDelta::new(
                        location,
                        value.text.as_bytes(),
                    )));
                }
                OpenBlock::Text { text: value.text }
            }
            "thinking" => {
                let value: ThinkingStart = serde_json::from_str(event.content_block.get())
                    .map_err(|error| invalid_response(error.to_string()))?;
                output.push(ModelEvent::BlockStarted(BlockStarted::new(
                    item_id,
                    location,
                    BlockStart::ReasoningPending { replay: None },
                )));
                let visibility = if value.thinking.is_empty() {
                    None
                } else {
                    output.push(ModelEvent::ReasoningVisibilityBound(
                        ReasoningVisibilityBound::new(location, ReasoningVisibility::Visible),
                    ));
                    Some(ReasoningVisibility::Visible)
                };
                if !value.thinking.is_empty() {
                    output.push(ModelEvent::ReasoningDelta(ReasoningDelta::new(
                        location,
                        value.thinking.as_bytes(),
                    )));
                }
                // `signature:""` on content_block_start is a placeholder, not a
                // bound envelope. Treating it as bound makes signature_delta a
                // no-op and later Messages replay of `signature:""` is 400.
                let signature = nonempty_opaque_signature(value.signature, "thinking signature")?;
                if let Some(signature) = &signature {
                    let reported_model = self
                        .reported_model
                        .clone()
                        .ok_or_else(|| self.fail("provider-reported model was not initialized"))?;
                    let replay = self.prepared.bind_replay(
                        reported_model,
                        SIGNATURE_REPLAY_KIND,
                        signature.clone(),
                    )?;
                    output.push(ModelEvent::ReasoningReplayBound(ReasoningReplayBound::new(
                        location, replay,
                    )));
                }
                OpenBlock::Thinking {
                    text: value.thinking,
                    signature,
                    visibility,
                }
            }
            "redacted_thinking" => {
                let value: RedactedStart = serde_json::from_str(event.content_block.get())
                    .map_err(|error| invalid_response(error.to_string()))?;
                validate_opaque_string(&value.data, "redacted thinking data")?;
                OpenBlock::Redacted { data: value.data }
            }
            "server_tool_use" => {
                if !self.prepared.hosted_effect_capable() {
                    return Err(self.fail(
                        "Messages code execution server tool is not admitted by the prepared request",
                    ));
                }
                let value: ToolUseStart = serde_json::from_str(event.content_block.get())
                    .map_err(|error| invalid_response(error.to_string()))?;
                if value.name != "code_execution"
                    || value.caller.is_some()
                    || value.container_id.is_some()
                    || !self.server_tool_ids.insert(value.id.clone())
                {
                    return Err(self.fail("invalid or duplicate Messages server_tool_use"));
                }
                self.server_lineage
                    .push_server(&value.id)
                    .map_err(|error| self.fail(error.to_string()))?;
                self.hosted_effects_possible = true;
                let provider_kind = self
                    .prepared
                    .dialect
                    .programmatic_tool_namespace()
                    .ok_or_else(|| {
                        self.fail("Messages stream policy omitted hosted-tool identity")
                    })?;
                let name = ToolName::new(value.name)
                    .map_err(|error| invalid_response(error.to_string()))?;
                let call_id = CallId::generate(self.clock.as_ref(), self.ids.as_ref())
                    .map_err(|error| invalid_response(error.to_string()))?;
                let correlation =
                    ToolCorrelation::new(Some(value.id.clone()), None, ToolCallerLineage::Direct)
                        .map_err(|error| invalid_response(error.to_string()))?;
                output.push(ModelEvent::BlockStarted(BlockStarted::new(
                    item_id,
                    location,
                    BlockStart::ToolCall {
                        call_id,
                        tool: name.clone(),
                        owner: ToolExecutionOwner::ProviderHosted {
                            provider_kind: provider_kind.clone(),
                        },
                        state: ToolCallState::Pending,
                        correlation: correlation.clone(),
                    },
                )));
                let mut json = String::new();
                if value.input.get() != "{}" {
                    json.push_str(value.input.get());
                    output.push(ModelEvent::ToolInputDelta(ToolInputDelta::new(
                        location,
                        value.input.get().as_bytes(),
                    )));
                }
                OpenBlock::ToolUse(ToolUseInput {
                    call_id,
                    remote_id: value.id,
                    name,
                    owner: ToolExecutionOwner::ProviderHosted { provider_kind },
                    state: ToolCallState::Pending,
                    correlation,
                    json,
                })
            }
            "code_execution_tool_result" => {
                if !self.prepared.hosted_effect_capable() {
                    return Err(self.fail(
                        "Messages code execution result is not admitted by the prepared request",
                    ));
                }
                let value: serde_json::Value = serde_json::from_str(event.content_block.get())
                    .map_err(|error| invalid_response(error.to_string()))?;
                let object = value
                    .as_object()
                    .ok_or_else(|| self.fail("code execution result must be an object"))?;
                let tool_use_id = object
                    .get("tool_use_id")
                    .and_then(serde_json::Value::as_str)
                    .filter(|id| self.server_tool_ids.contains(*id))
                    .ok_or_else(|| {
                        self.fail("code execution result did not match a preceding server tool")
                    })?;
                if !self
                    .code_execution_result_ids
                    .insert(tool_use_id.to_owned())
                {
                    return Err(self.fail(
                        "code execution result repeated a terminal result for one server tool",
                    ));
                }
                self.server_lineage
                    .complete_next(tool_use_id)
                    .map_err(|_| {
                        self.fail("code execution results did not follow exact server-tool order")
                    })?;
                if object.get("content").is_none()
                    || object.get("type").and_then(serde_json::Value::as_str)
                        != Some("code_execution_tool_result")
                {
                    return Err(self.fail("code execution result had an incompatible shape"));
                }
                self.hosted_effects_possible = true;
                let raw = RawValue::from_string(value.to_string())
                    .map_err(|error| invalid_response(error.to_string()))?;
                let reported_model = self
                    .reported_model
                    .clone()
                    .ok_or_else(|| self.fail("provider-reported model was not initialized"))?;
                let replay = self.prepared.bind_replay(
                    reported_model,
                    CODE_EXECUTION_RESULT_REPLAY_KIND,
                    raw,
                )?;
                let block = ContentBlock::Opaque(
                    OpaqueBlock::new(replay, OpaqueFallback::CompatibleReplay)
                        .map_err(|error| invalid_response(error.to_string()))?,
                );
                output.push(ModelEvent::BlockStarted(BlockStarted::new(
                    item_id,
                    location,
                    BlockStart::Complete(block.clone()),
                )));
                let _ = tool_use_id;
                OpenBlock::Complete(block)
            }
            "tool_use" => {
                let value: ToolUseStart = serde_json::from_str(event.content_block.get())
                    .map_err(|error| invalid_response(error.to_string()))?;
                if !self.remote_tool_ids.insert(value.id.clone()) {
                    return Err(self.fail("Messages repeated a tool_use id in one response"));
                }
                let name = ToolName::new(value.name).map_err(|error| {
                    invalid_response(format!("invalid Messages tool name: {error}"))
                })?;
                let caller = parse_tool_caller(value.caller.as_deref(), &self.prepared.dialect)?;
                let owner = self.prepared.tool_ownership().owner(&name);
                let call_id =
                    CallId::generate(self.clock.as_ref(), self.ids.as_ref()).map_err(|error| {
                        invalid_response(format!("failed to generate tool call ID: {error}"))
                    })?;
                if let ToolCaller::Programmatic { tool_id } = &caller {
                    if !self.prepared.hosted_effect_capable() {
                        return Err(self.fail(
                            "Messages programmatic caller is not admitted by the prepared request",
                        ));
                    }
                    self.hosted_effects_possible = true;
                    if !self.server_lineage.is_unresolved(tool_id)
                        || !matches!(
                            self.prepared.tool_ownership().caller(&name),
                            Some(
                                ToolCallerDisposition::ProviderProgrammaticOnly
                                    | ToolCallerDisposition::DirectAndProviderProgrammatic
                            )
                        )
                    {
                        return Err(self.fail("programmatic caller did not match an eligible tool and preceding server tool"));
                    }
                    let mut json = String::new();
                    if value.input.get() != "{}" {
                        json.push_str(value.input.get());
                    }
                    return self.open_deferred_programmatic_block(
                        event.index,
                        OpenBlock::DeferredProgrammaticToolUse {
                            call_id,
                            remote_id: value.id,
                            name,
                            owner,
                            execution_id: tool_id.clone(),
                            json,
                        },
                    );
                }
                if matches!(caller, ToolCaller::ExplicitDirect) {
                    self.explicit_direct_tool_ids.insert(value.id.clone());
                }
                // Unadvertised direct names stay PlakLocal Requested so admit_tool can
                // return a typed failure listing the presented catalog.
                let correlation =
                    ToolCorrelation::new(Some(value.id.clone()), None, ToolCallerLineage::Direct)
                        .map_err(|error| invalid_response(error.to_string()))?;
                output.push(ModelEvent::BlockStarted(BlockStarted::new(
                    item_id,
                    location,
                    BlockStart::ToolCall {
                        call_id,
                        tool: name.clone(),
                        owner: owner.clone(),
                        state: ToolCallState::Requested,
                        correlation: correlation.clone(),
                    },
                )));
                let mut json = String::new();
                if value.input.get() != "{}" {
                    json.push_str(value.input.get());
                    output.push(ModelEvent::ToolInputDelta(ToolInputDelta::new(
                        location,
                        value.input.get().as_bytes(),
                    )));
                }
                OpenBlock::ToolUse(ToolUseInput {
                    call_id,
                    remote_id: value.id,
                    name,
                    owner,
                    state: ToolCallState::Requested,
                    correlation,
                    json,
                })
            }
            "refusal" => {
                let value: RefusalStart = serde_json::from_str(event.content_block.get())
                    .map_err(|error| invalid_response(error.to_string()))?;
                output.push(ModelEvent::BlockStarted(BlockStarted::new(
                    item_id,
                    location,
                    BlockStart::Refusal,
                )));
                if !value.refusal.is_empty() {
                    output.push(ModelEvent::TextDelta(TextDelta::new(
                        location,
                        value.refusal.as_bytes(),
                    )));
                }
                OpenBlock::Refusal {
                    text: value.refusal,
                }
            }
            "fallback" => {
                return Err(MessagesStreamError::new(
                    MessagesStreamErrorKind::UnsupportedSemantic,
                    MessagesStreamErrorPhase::Stream,
                    "Messages fallback content is explicitly excluded by adapter revision 1",
                ));
            }
            other => {
                if matches!(other, "server_tool_use" | "web_search_tool_result") {
                    self.hosted_effects_possible = true;
                }
                return Err(MessagesStreamError::new(
                    MessagesStreamErrorKind::UnsupportedSemantic,
                    MessagesStreamErrorPhase::Stream,
                    "Messages content block type is outside the declared feature matrix",
                ));
            }
        };
        if block.retained_bytes() > self.limits.max_block_bytes() {
            return Err(self.fail("Messages content block exceeded the byte limit"));
        }
        self.visible_output = true;
        if self.open_blocks.insert(event.index, block).is_some() {
            return Err(self.fail("Messages repeated an open content block index"));
        }
        self.next_block_index = self.next_block_index.saturating_add(1);
        Ok(())
    }

    fn open_deferred_programmatic_block(
        &mut self,
        index: u32,
        block: OpenBlock,
    ) -> Result<(), MessagesStreamError> {
        if block.retained_bytes() > self.limits.max_block_bytes() {
            return Err(self.fail("Messages content block exceeded the byte limit"));
        }
        self.visible_output = true;
        if self.open_blocks.insert(index, block).is_some() {
            return Err(self.fail("Messages repeated an open content block index"));
        }
        self.next_block_index = self.next_block_index.saturating_add(1);
        Ok(())
    }

    #[allow(
        clippy::too_many_lines,
        reason = "all typed delta transitions stay together so ownership and replay mutations remain auditable"
    )]
    fn block_delta(
        &mut self,
        data: &str,
        output: &mut Vec<ModelEvent>,
    ) -> Result<(), MessagesStreamError> {
        self.require_open()?;
        let event: ContentBlockDeltaEvent = parse_event(data)?;
        let reported_model = self
            .reported_model
            .clone()
            .ok_or_else(|| invalid_response("provider-reported model was not initialized"))?;
        let Some(block) = self.open_blocks.get_mut(&event.index) else {
            return Err(self.fail("content delta arrived without an open block"));
        };
        let kind: EventEnvelope = serde_json::from_str(event.delta.get())
            .map_err(|error| invalid_response(error.to_string()))?;
        let location = OutputLocation::new(0, event.index);
        match (kind.kind.as_str(), block) {
            ("text_delta", OpenBlock::Text { text }) => {
                let delta: TextDeltaWire = serde_json::from_str(event.delta.get())
                    .map_err(|error| invalid_response(error.to_string()))?;
                if delta.text.is_empty() {
                    return Err(self.fail("empty text delta is not canonical"));
                }
                text.push_str(&delta.text);
                output.push(ModelEvent::TextDelta(TextDelta::new(
                    location,
                    delta.text.into_bytes(),
                )));
            }
            (
                "thinking_delta",
                OpenBlock::Thinking {
                    text,
                    signature,
                    visibility,
                },
            ) => {
                let delta =
                    parse_thinking_delta(event.delta.get(), self.prepared.dialect.extensions())?;
                // Claude Code 2.1.x emits empty thinking blocks (`thinking: ""`
                // plus a signature). Matching empty `thinking_delta` events are
                // no-ops; they must not fail closed or mark the block visible.
                if delta.thinking.is_empty() {
                    return Ok(());
                }
                if signature.is_some() {
                    return Err(self.fail("thinking mutated after its signature was bound"));
                }
                if visibility.is_none() {
                    output.push(ModelEvent::ReasoningVisibilityBound(
                        ReasoningVisibilityBound::new(location, ReasoningVisibility::Visible),
                    ));
                    *visibility = Some(ReasoningVisibility::Visible);
                }
                text.push_str(&delta.thinking);
                output.push(ModelEvent::ReasoningDelta(ReasoningDelta::new(
                    location,
                    delta.thinking.into_bytes(),
                )));
            }
            ("signature_delta", OpenBlock::Thinking { signature, .. }) => {
                let delta: SignatureDeltaWire = serde_json::from_str(event.delta.get())
                    .map_err(|error| invalid_response(error.to_string()))?;
                let Some(next) =
                    nonempty_opaque_signature(Some(delta.signature), "thinking signature")?
                else {
                    return Ok(());
                };
                // A provider may repeat the signature at block end. Compare
                // decoded JSON values so harmless encoding differences remain
                // valid while contradictory signed evidence fails closed.
                if let Some(current) = signature {
                    let current: String = serde_json::from_str(current.get())
                        .map_err(|error| invalid_response(error.to_string()))?;
                    let repeated: String = serde_json::from_str(next.get())
                        .map_err(|error| invalid_response(error.to_string()))?;
                    if current != repeated {
                        return Err(invalid_response(
                            "conflicting thinking signatures for one content block",
                        ));
                    }
                    return Ok(());
                }
                *signature = Some(next.clone());
                let replay =
                    self.prepared
                        .bind_replay(reported_model, SIGNATURE_REPLAY_KIND, next)?;
                output.push(ModelEvent::ReasoningReplayBound(ReasoningReplayBound::new(
                    location, replay,
                )));
            }
            ("input_json_delta", OpenBlock::ToolUse(ToolUseInput { json, .. })) => {
                let delta: InputJsonDeltaWire = serde_json::from_str(event.delta.get())
                    .map_err(|error| invalid_response(error.to_string()))?;
                // Messages documents an empty first input delta; it is a bounded no-op.
                if !delta.partial_json.is_empty() {
                    json.push_str(&delta.partial_json);
                    output.push(ModelEvent::ToolInputDelta(ToolInputDelta::new(
                        location,
                        delta.partial_json.into_bytes(),
                    )));
                }
            }
            ("input_json_delta", OpenBlock::DeferredProgrammaticToolUse { json, .. }) => {
                let delta: InputJsonDeltaWire = serde_json::from_str(event.delta.get())
                    .map_err(|error| invalid_response(error.to_string()))?;
                json.push_str(&delta.partial_json);
            }
            ("refusal_delta", OpenBlock::Refusal { text }) => {
                let delta: RefusalDeltaWire = serde_json::from_str(event.delta.get())
                    .map_err(|error| invalid_response(error.to_string()))?;
                text.push_str(&delta.refusal);
                output.push(ModelEvent::TextDelta(TextDelta::new(
                    location,
                    delta.refusal.into_bytes(),
                )));
            }
            (_, OpenBlock::Complete(_)) => {
                return Err(self.fail("complete hosted result received a delta"));
            }
            ("citations_delta", _) => {
                return Err(MessagesStreamError::new(
                    MessagesStreamErrorKind::UnsupportedSemantic,
                    MessagesStreamErrorPhase::Stream,
                    "Messages citations are explicitly excluded by adapter revision 1",
                ));
            }
            _ => return Err(self.fail("content delta type did not match its open block")),
        }
        if self
            .open_blocks
            .get(&event.index)
            .is_some_and(|block| block.retained_bytes() > self.limits.max_block_bytes())
        {
            return Err(self.fail("Messages content block exceeded the byte limit"));
        }
        Ok(())
    }

    #[allow(
        clippy::too_many_lines,
        reason = "keeping every content-block finalization transition together makes replay rules auditable"
    )]
    fn block_stop(
        &mut self,
        data: &str,
        output: &mut Vec<ModelEvent>,
    ) -> Result<(), MessagesStreamError> {
        self.require_open()?;
        let reported_model = self
            .reported_model
            .clone()
            .ok_or_else(|| self.fail("provider-reported model was not initialized"))?;
        let event: ContentBlockStopEvent = parse_event(data)?;
        let Some(block) = self.open_blocks.remove(&event.index) else {
            return Err(self.fail("content_block_stop arrived without an open block"));
        };
        let index = event.index;
        let location = OutputLocation::new(0, index);
        let item_id = self.item_id()?;
        let canonical = match block {
            OpenBlock::Text { text } => ContentBlock::Text(
                TextBlock::new(text, None).map_err(|error| invalid_response(error.to_string()))?,
            ),
            OpenBlock::Thinking {
                text,
                signature,
                visibility,
            } => {
                if signature.is_none()
                    && self.prepared.dialect.extensions() != MessagesStreamExtensions::OpenRouterV1
                {
                    return Err(self.fail("thinking block ended without a signature"));
                }
                let replay = signature
                    .map(|signature| {
                        self.prepared.bind_replay(
                            reported_model.clone(),
                            SIGNATURE_REPLAY_KIND,
                            signature,
                        )
                    })
                    .transpose()?;
                let (visibility, text) = if text.is_empty() {
                    if visibility.is_some() {
                        return Err(
                            self.fail("empty Messages thinking had already resolved visible")
                        );
                    }
                    output.push(ModelEvent::ReasoningVisibilityBound(
                        ReasoningVisibilityBound::new(location, ReasoningVisibility::Omitted),
                    ));
                    (ReasoningVisibility::Omitted, None)
                } else {
                    if visibility != Some(ReasoningVisibility::Visible) {
                        return Err(self.fail(
                            "nonempty Messages thinking did not resolve visible before its first delta",
                        ));
                    }
                    (ReasoningVisibility::Visible, Some(text))
                };
                let reasoning = ReasoningBlock::new(visibility, text, replay)
                    .map_err(|error| invalid_response(error.to_string()))?;
                ContentBlock::Reasoning(reasoning)
            }
            OpenBlock::Redacted { data } => {
                let replay =
                    self.prepared
                        .bind_replay(reported_model, REDACTED_REPLAY_KIND, data)?;
                let reasoning =
                    ReasoningBlock::new(ReasoningVisibility::Redacted, None, Some(replay))
                        .map_err(|error| invalid_response(error.to_string()))?;
                output.push(ModelEvent::BlockStarted(BlockStarted::new(
                    item_id,
                    location,
                    BlockStart::Complete(ContentBlock::Reasoning(reasoning.clone())),
                )));
                ContentBlock::Reasoning(reasoning)
            }
            OpenBlock::ToolUse(input) => {
                if matches!(
                    input.owner,
                    ToolExecutionOwner::PlakLocal | ToolExecutionOwner::PlakMcp { .. }
                ) {
                    // A closed wire block may still contain truncated JSON when a
                    // classifier revokes it. Only the terminal can authorize parsing.
                    self.closed_client_blocks.push((index, input));
                    return Ok(());
                }
                self.complete_tool_input(input, location, output)?
            }
            OpenBlock::DeferredProgrammaticToolUse {
                call_id,
                remote_id,
                name,
                owner,
                execution_id,
                mut json,
            } => {
                if json.is_empty() {
                    json.push_str("{}");
                }
                let parsed = serde_json::from_str::<serde_json::Value>(&json).map_err(|_| {
                    self.fail("terminal Messages tool input was not complete valid JSON")
                })?;
                if !parsed.is_object() {
                    return Err(self.fail("terminal Messages tool input must be a JSON object"));
                }
                self.deferred_programmatic_blocks.push((
                    event.index,
                    OpenBlock::DeferredProgrammaticToolUse {
                        call_id,
                        remote_id,
                        name,
                        owner,
                        execution_id,
                        json,
                    },
                ));
                return Ok(());
            }
            OpenBlock::Refusal { text } => ContentBlock::Refusal(
                RefusalBlock::new(text).map_err(|error| invalid_response(error.to_string()))?,
            ),
            OpenBlock::Complete(block) => block,
        };
        let needs_completion_event = !matches!(
            &canonical,
            ContentBlock::Reasoning(reasoning)
                if reasoning.visibility() == ReasoningVisibility::Redacted
        );
        if self.blocks.insert(index, canonical).is_some() {
            return Err(self.fail("Messages completed one content block more than once"));
        }
        if needs_completion_event {
            output.push(ModelEvent::BlockCompleted(BlockCompleted::new(location)));
        }
        Ok(())
    }

    fn complete_tool_input(
        &mut self,
        input: ToolUseInput,
        location: OutputLocation,
        output: &mut Vec<ModelEvent>,
    ) -> Result<ContentBlock, MessagesStreamError> {
        let ToolUseInput {
            call_id,
            remote_id,
            name,
            owner,
            state,
            correlation,
            mut json,
        } = input;
        if json.is_empty() {
            json.push_str("{}");
            output.push(ModelEvent::ToolInputDelta(ToolInputDelta::new(
                location, b"{}",
            )));
        }
        let parsed = serde_json::from_str::<serde_json::Value>(&json)
            .map_err(|_| self.fail("terminal Messages tool input was not complete valid JSON"))?;
        if !parsed.is_object() {
            return Err(self.fail("terminal Messages tool input must be a JSON object"));
        }
        if correlation.remote_call_id() != Some(remote_id.as_str()) {
            return Err(self.fail("Messages tool correlation changed during assembly"));
        }
        Ok(ContentBlock::ToolCall(
            ToolCallBlock::new(call_id, name, owner, state, correlation, json)
                .map_err(|error| invalid_response(error.to_string()))?,
        ))
    }

    fn message_delta(
        &mut self,
        data: &str,
        output: &mut Vec<ModelEvent>,
    ) -> Result<(), MessagesStreamError> {
        self.require_open()?;
        if !self.open_blocks.is_empty() {
            return Err(self.fail("message_delta arrived before content block stop"));
        }
        if self.stop_reason.is_some() || self.stop_sequence.is_some() || self.stop_details.is_some()
        {
            return Err(self.fail("message_delta arrived after terminal message metadata"));
        }
        let event = parse_message_delta(data, self.prepared.dialect.extensions())?;
        if let Some(container) = event.delta.container {
            self.bind_container(container, "message_delta")?;
        }
        self.flush_deferred_programmatic_blocks(output)?;
        if let Some(stop_reason) = event.delta.stop_reason {
            if self.stop_reason.is_some() {
                return Err(self.fail("message stop reason was supplied more than once"));
            }
            self.stop_reason = Some(MessagesStopReason::parse(&stop_reason)?);
        }
        if self
            .stop_reason
            .is_some_and(|reason| reason != MessagesStopReason::Refusal)
        {
            for (index, input) in std::mem::take(&mut self.closed_client_blocks) {
                let location = OutputLocation::new(0, index);
                let block = self.complete_tool_input(input, location, output)?;
                self.blocks.insert(index, block);
                output.push(ModelEvent::BlockCompleted(BlockCompleted::new(location)));
            }
        }
        match (self.stop_sequence.is_some(), event.delta.stop_sequence) {
            (true, Some(_)) => {
                return Err(self.fail("message stop sequence was supplied more than once"));
            }
            (false, Some(stop_sequence)) => self.stop_sequence = Some(stop_sequence),
            (_, None) => {}
        }
        if let Some(stop_details) = event.delta.stop_details {
            if self.stop_details.is_some() {
                return Err(self.fail("message stop details were supplied more than once"));
            }
            self.stop_details = Some(parse_stop_details(stop_details)?);
        }
        let previous = self
            .provider_usage
            .ok_or_else(|| self.fail("message usage was not initialized"))?;
        let provider_usage = previous
            .merge(event.usage.into_family())
            .map_err(|message| self.invalid_accounting(message))?;
        let usage = provider_usage
            .into_canonical()
            .map_err(|message| self.invalid_accounting(message))?;
        self.provider_usage = Some(provider_usage);
        self.usage = Some(usage);
        output.push(ModelEvent::Usage(UsageUpdate::new(usage)));
        Ok(())
    }

    fn flush_deferred_programmatic_blocks(
        &mut self,
        output: &mut Vec<ModelEvent>,
    ) -> Result<(), MessagesStreamError> {
        if self.deferred_programmatic_blocks.is_empty() {
            return Ok(());
        }
        let container = self
            .container_id
            .clone()
            .ok_or_else(|| self.fail("programmatic tool use requires a message_delta container"))?;
        let item_id = self.item_id()?;
        let deferred = std::mem::take(&mut self.deferred_programmatic_blocks);
        let provider_kind = self
            .prepared
            .dialect
            .programmatic_tool_namespace()
            .ok_or_else(|| self.fail("Messages stream policy omitted hosted-tool identity"))?;
        for (index, block) in deferred {
            let OpenBlock::DeferredProgrammaticToolUse {
                call_id,
                remote_id,
                name,
                owner,
                execution_id,
                json,
            } = block
            else {
                return Err(self.fail("invalid deferred Messages block"));
            };
            let location = OutputLocation::new(0, index);
            let lineage =
                ToolCallerLineage::provider_programmatic(provider_kind.clone(), execution_id)
                    .map_err(|error| invalid_response(error.to_string()))?;
            let correlation =
                ToolCorrelation::new(Some(remote_id.clone()), Some(container.clone()), lineage)
                    .map_err(|error| invalid_response(error.to_string()))?;
            output.push(ModelEvent::BlockStarted(BlockStarted::new(
                item_id,
                location,
                BlockStart::ToolCall {
                    call_id,
                    tool: name.clone(),
                    owner: owner.clone(),
                    state: ToolCallState::Requested,
                    correlation: correlation.clone(),
                },
            )));
            output.push(ModelEvent::ToolInputDelta(ToolInputDelta::new(
                location,
                json.as_bytes(),
            )));
            let canonical = ContentBlock::ToolCall(
                ToolCallBlock::new(
                    call_id,
                    name,
                    owner,
                    ToolCallState::Requested,
                    correlation,
                    json,
                )
                .map_err(|error| invalid_response(error.to_string()))?,
            );
            if self.blocks.insert(index, canonical).is_some() {
                return Err(self.fail("deferred Messages block completed more than once"));
            }
            output.push(ModelEvent::BlockCompleted(BlockCompleted::new(location)));
        }
        Ok(())
    }

    fn ordered_blocks(&mut self) -> Result<Vec<ContentBlock>, MessagesStreamError> {
        if self
            .blocks
            .keys()
            .any(|index| *index >= self.next_block_index)
        {
            return Err(self.fail("Messages completed a content block outside the started range"));
        }
        Ok(self.blocks.values().cloned().collect())
    }

    #[allow(
        clippy::too_many_lines,
        reason = "terminal validation and exact artifact sealing remain one ordered boundary"
    )]
    fn message_stop(
        &mut self,
        data: &str,
        output: &mut Vec<ModelEvent>,
    ) -> Result<(), MessagesStreamError> {
        self.require_open()?;
        parse_event::<MessageStopEvent>(data)?;
        if !self.open_blocks.is_empty() {
            return Err(self.fail("message_stop arrived with an open content block"));
        }
        let blocks = self.ordered_blocks()?;
        let stop_reason = self
            .stop_reason
            .ok_or_else(|| self.fail("message_stop arrived without a terminal stop_reason"))?;
        if stop_reason == MessagesStopReason::Refusal && self.hosted_effects_possible {
            return Err(MessagesStreamError::new(
                MessagesStreamErrorKind::UnsupportedSemantic,
                MessagesStreamErrorPhase::Complete,
                "Messages refusal after possible hosted effects requires reconciliation",
            ));
        }
        validate_stop_blocks(
            stop_reason,
            &blocks,
            self.prepared.hosted_effect_capable(),
            self.container_id.as_deref(),
        )?;
        let (finish, stop_outcome) = map_finish(
            stop_reason,
            self.stop_sequence.clone(),
            self.stop_details.clone(),
        )?;
        let item_id = self.item_id()?;
        // Streaming refusal revokes the entire provisional response, including tool
        // proposals and signed thinking. Nothing from it is eligible for replay.
        let items = if stop_reason == MessagesStopReason::Refusal {
            Vec::new()
        } else {
            vec![
                ConversationItem::new(
                    item_id,
                    Actor::Assistant,
                    blocks.clone(),
                    Provenance::ModelOutput,
                )
                .map_err(|error| invalid_response(error.to_string()))?,
            ]
        };
        let metadata = ModelResponseMetadata::new(
            self.reported_model.clone(),
            self.provider_request_id.clone(),
        );
        let mut response = ModelResponse::new(
            items,
            finish,
            self.usage,
            self.message_id.clone(),
            self.prepared.plan_digest(),
        )
        .with_metadata(metadata);
        if self.hosted_effects_possible && self.prepared.artifact_plan.is_some() {
            let plan = self.prepared.artifact_plan.clone().ok_or_else(|| {
                self.fail("hosted Messages response omitted artifact production authority")
            })?;
            let operation = self
                .prepared
                .operation_digest
                .ok_or_else(|| self.fail("hosted Messages response omitted operation identity"))?;
            let container = self
                .container_id
                .clone()
                .ok_or_else(|| self.fail("hosted Messages response omitted its exact container"))?;
            let mut native_container = None;
            let mut artifact_lineage = self.prepared.active_artifact_lineage.clone();
            let artifact_policy = self.prepared.request_policy.clone();
            let artifact_ownership = self.prepared.tool_ownership.clone();
            let (mut native_blocks, _) = crate::request::translate_assistant_bound(
                &blocks,
                &artifact_policy,
                &artifact_ownership,
                true,
                true,
                &mut native_container,
                &mut artifact_lineage,
                &mut Vec::new(),
            )
            .map_err(|error| self.fail(error.to_string()))?;
            for block in &mut native_blocks {
                if let crate::request::MessageBlock::ToolUse { id, caller, .. } = block
                    && self.explicit_direct_tool_ids.contains(id)
                {
                    *caller = Some(crate::request::ToolCallerParam {
                        kind: "direct".to_owned(),
                        tool_id: None,
                    });
                }
            }
            if native_container
                .as_deref()
                .is_some_and(|value| value != container)
            {
                return Err(self.fail("hosted Messages native blocks changed container"));
            }
            let tool_set =
                messages_tool_set_identity(plan.request(), &artifact_policy, &artifact_ownership)
                    .map_err(|error| self.fail(error.to_string()))?;
            let configuration =
                messages_artifact_configuration_identity(&artifact_policy, plan.digest())
                    .map_err(|error| self.fail(error.to_string()))?;
            let binding = messages_artifact_binding(&artifact_policy, tool_set, configuration)
                .map_err(|error| self.fail(error.to_string()))?;
            let covered = plan
                .request()
                .input()
                .staged_canonical_item_prefix_identity(response.output())
                .map_err(|error| self.fail(error.to_string()))?;
            let artifact = crate::artifact::MessagesFamilyArtifact::new(
                binding,
                plan.digest(),
                tool_set,
                configuration,
                covered,
                container,
                self.prepared.active_artifact_lineage.clone(),
                native_blocks,
                crate::artifact::MessagesArtifactStop {
                    reason: stop_reason.wire_name().to_owned(),
                    sequence: self.stop_sequence.clone(),
                    details: self
                        .stop_details
                        .as_ref()
                        .map(|details| details.raw.clone()),
                },
            )
            .map_err(|error| self.fail(error.to_string()))?;
            let request = plan.request();
            let source = request
                .input()
                .canonical_item_prefix_identity(request.input().items().len())
                .map_err(|error| self.fail(error.to_string()))?;
            let physical = Digest::sha256(
                DigestDomain::ProviderOperation,
                1,
                &[
                    b"plakous-messages/physical-operation-v1",
                    &operation.identity_bytes(),
                ],
            )
            .map_err(|error| self.fail(error.to_string()))?;
            let terminal = Digest::sha256(
                DigestDomain::ProviderOperation,
                1,
                &[
                    b"plakous-messages/terminal-operation-v1",
                    &operation.identity_bytes(),
                    self.message_id.as_deref().unwrap_or_default().as_bytes(),
                ],
            )
            .map_err(|error| self.fail(error.to_string()))?;
            let producer = FamilyArtifactProducerFacts::new(
                request.identity(),
                request.model_input_identity(),
                plan.digest(),
                None,
                request.metadata().run_id(),
                request.metadata().root(),
                request.metadata().attempt_id(),
                source.identity(),
                u32::try_from(source.item_count())
                    .map_err(|_| self.fail("Messages source item count overflow"))?,
                1,
                covered.identity(),
                u32::try_from(covered.item_count())
                    .map_err(|_| self.fail("Messages covered item count overflow"))?,
                physical,
                terminal,
            )
            .map_err(|error| self.fail(error.to_string()))?;
            let candidate = artifact
                .seal_candidate(producer)
                .map_err(|error| self.fail(error.to_string()))?;
            response = response
                .with_family_artifact_candidate(candidate)
                .map_err(|error| self.fail(error.to_string()))?;
        }
        self.stop_outcome = Some(stop_outcome);
        self.terminal_response = Some(response.clone());
        output.push(ModelEvent::ResponseCompleted(response));
        self.state = match self.prepared.dialect.extensions() {
            MessagesStreamExtensions::Strict | MessagesStreamExtensions::ClaudeOauthV1 => {
                StreamState::Stopped
            }
            MessagesStreamExtensions::OpenRouterV1 => StreamState::AwaitingGatewayDone,
        };
        Ok(())
    }

    /// Admits and binds the code-execution container reported by a
    /// message-family event, rejecting an unadmitted, malformed, or
    /// lineage-changing identity.
    fn bind_container(
        &mut self,
        container: MessageContainer,
        event: &str,
    ) -> Result<(), MessagesStreamError> {
        if !self.prepared.hosted_effect_capable()
            || container.id.is_empty()
            || container.id.len() > 512
        {
            return Err(self.fail(format!(
                "Messages {event} container is not admitted or valid"
            )));
        }
        if self
            .prepared
            .expected_container
            .as_deref()
            .is_some_and(|expected| expected != container.id)
        {
            return Err(self.fail(format!(
                "Messages {event} container changed active artifact lineage"
            )));
        }
        match self.container_id.as_deref() {
            Some(bound) if bound != container.id => {
                Err(self.fail(format!("Messages {event} repeated a conflicting container")))
            }
            Some(_) => Ok(()),
            None => {
                self.container_id = Some(container.id);
                Ok(())
            }
        }
    }

    fn require_open(&mut self) -> Result<(), MessagesStreamError> {
        if matches!(self.state, StreamState::Open) {
            Ok(())
        } else {
            Err(self.fail("Messages event arrived outside an open message"))
        }
    }

    fn item_id(&mut self) -> Result<EntryId, MessagesStreamError> {
        self.item_id
            .ok_or_else(|| self.fail("assistant item identity was not initialized"))
    }

    fn fail(&mut self, message: impl Into<String>) -> MessagesStreamError {
        self.poison();
        MessagesStreamError::new(
            MessagesStreamErrorKind::InvalidEvent,
            MessagesStreamErrorPhase::Stream,
            message,
        )
    }

    fn invalid_accounting(&mut self, message: impl Into<String>) -> MessagesStreamError {
        self.poison();
        MessagesStreamError::new(
            MessagesStreamErrorKind::InvalidAccounting,
            MessagesStreamErrorPhase::Stream,
            message,
        )
    }

    fn poison(&mut self) {
        self.state = StreamState::Failed;
        self.eof_sealed = false;
        self.terminal_response = None;
        self.stop_outcome = None;
    }
}

#[derive(Deserialize)]
struct EventEnvelope {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageStartEvent {
    #[serde(rename = "type")]
    _kind: String,
    message: MessageStart,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageStart {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    role: String,
    model: String,
    content: Vec<Box<RawValue>>,
    /// Code-execution container reused by this message (`null` on every
    /// request that did not allocate one).
    #[serde(default)]
    container: Option<MessageContainer>,
    #[serde(default)]
    stop_reason: Option<Box<RawValue>>,
    #[serde(default)]
    stop_sequence: Option<Box<RawValue>>,
    #[serde(default)]
    stop_details: Option<Box<RawValue>>,
    usage: UsageWire,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageContainer {
    id: String,
    #[serde(rename = "expires_at")]
    _expires_at: String,
}

/// Messages `usage` receipt.
///
/// Accounting reads only these scalars. Usage is additive provider metadata
/// with no authority, so unknown keys are ignored rather than failing a turn.
#[derive(Debug, Deserialize)]
struct OutputTokensDetailsWire {
    #[serde(default)]
    thinking_tokens: u64,
}

#[allow(
    clippy::struct_field_names,
    reason = "field names intentionally mirror Messages usage JSON"
)]
#[derive(Debug, Deserialize)]
struct UsageWire {
    input_tokens: u64,
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    output_tokens_details: Option<OutputTokensDetailsWire>,
}

impl UsageWire {
    fn into_provider(self) -> ProviderUsage {
        let thinking_tokens = self
            .output_tokens_details
            .map_or(0, |details| details.thinking_tokens);
        ProviderUsage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            thinking_tokens,
            cache_creation_input_tokens: self.cache_creation_input_tokens,
            cache_read_input_tokens: self.cache_read_input_tokens,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContentBlockStartEvent {
    #[serde(rename = "type")]
    _kind: String,
    index: u32,
    content_block: Box<RawValue>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TextStart {
    #[serde(rename = "type")]
    _kind: String,
    text: String,
    #[serde(default)]
    citations: Option<Box<RawValue>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ThinkingStart {
    #[serde(rename = "type")]
    _kind: String,
    #[serde(default)]
    thinking: String,
    #[serde(default)]
    signature: Option<Box<RawValue>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RedactedStart {
    #[serde(rename = "type")]
    _kind: String,
    data: Box<RawValue>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolUseStart {
    #[serde(rename = "type")]
    _kind: String,
    id: String,
    name: String,
    input: Box<RawValue>,
    #[serde(default)]
    caller: Option<Box<RawValue>>,
    #[serde(default)]
    container_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RefusalStart {
    #[serde(rename = "type")]
    _kind: String,
    refusal: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContentBlockDeltaEvent {
    #[serde(rename = "type")]
    _kind: String,
    index: u32,
    delta: Box<RawValue>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TextDeltaWire {
    #[serde(rename = "type")]
    _kind: String,
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ThinkingDeltaWire {
    #[serde(rename = "type")]
    _kind: String,
    thinking: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SignatureDeltaWire {
    #[serde(rename = "type")]
    _kind: String,
    signature: Box<RawValue>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InputJsonDeltaWire {
    #[serde(rename = "type")]
    _kind: String,
    partial_json: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RefusalDeltaWire {
    #[serde(rename = "type")]
    _kind: String,
    refusal: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContentBlockStopEvent {
    #[serde(rename = "type")]
    _kind: String,
    index: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageDeltaEvent {
    #[serde(rename = "type")]
    _kind: String,
    delta: MessageDelta,
    usage: MessageDeltaUsage,
}

#[allow(
    clippy::struct_field_names,
    reason = "field names intentionally mirror Messages message_delta JSON"
)]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageDelta {
    #[serde(default)]
    container: Option<MessageContainer>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    stop_sequence: Option<String>,
    #[serde(default)]
    stop_details: Option<Box<RawValue>>,
}

#[allow(
    clippy::struct_field_names,
    reason = "field names intentionally mirror Messages cumulative usage JSON"
)]
#[derive(Debug, Deserialize)]
struct MessageDeltaUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens_details: Option<OutputTokensDetailsWire>,
}

impl MessageDeltaUsage {
    fn into_family(self) -> MessagesUsageUpdate {
        MessagesUsageUpdate {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            thinking_tokens: self
                .output_tokens_details
                .map(|details| details.thinking_tokens),
            cache_creation_input_tokens: self.cache_creation_input_tokens,
            cache_read_input_tokens: self.cache_read_input_tokens,
        }
    }
}

fn parse_event<T: for<'de> Deserialize<'de>>(data: &str) -> Result<T, MessagesStreamError> {
    serde_json::from_str(data)
        .map_err(|error| invalid_response(format!("invalid Messages event payload: {error}")))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PingEvent {
    #[serde(rename = "type")]
    _kind: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageStopEvent {
    #[serde(rename = "type")]
    _kind: String,
}

fn parse_message_start(
    data: &str,
    extensions: MessagesStreamExtensions,
) -> Result<
    (
        MessageStartEvent,
        Option<plakous_model::PromptCacheDiagnosis>,
    ),
    MessagesStreamError,
> {
    match extensions {
        MessagesStreamExtensions::Strict => {
            // API-key and Claude OAuth both admit `message.diagnostics` as
            // null or advisory cache telemetry. Malformed diagnostics record
            // nothing and do not fail the turn. Unknown keys inside
            // `diagnostics` and inside `cache_miss_reason` are ignored.
            // Every other unknown field still fails closed.
            let mut value = parse_event::<serde_json::Value>(data)?;
            let diagnosis = match value
                .get_mut("message")
                .and_then(serde_json::Value::as_object_mut)
            {
                Some(message) => admit_message_diagnostics(message),
                None => None,
            };
            let event = serde_json::from_value(value).map_err(|error| {
                invalid_response(format!("invalid Messages event payload: {error}"))
            })?;
            Ok((event, diagnosis))
        }
        MessagesStreamExtensions::ClaudeOauthV1 => {
            let mut value = parse_event::<serde_json::Value>(data)?;
            let message = value
                .get_mut("message")
                .and_then(serde_json::Value::as_object_mut)
                .ok_or_else(|| invalid_response("Claude OAuth message_start omitted message"))?;
            if let Some(context_management) = message.remove("context_management") {
                validate_claude_context_management(&context_management)?;
            }
            let diagnosis = admit_message_diagnostics(message);
            if let Some(transformations) = message.remove("input_transformations") {
                validate_claude_input_transformations(&transformations)?;
            }
            let event = serde_json::from_value(value).map_err(|error| {
                invalid_response(format!(
                    "invalid Claude OAuth message_start payload: {error}"
                ))
            })?;
            Ok((event, diagnosis))
        }
        MessagesStreamExtensions::OpenRouterV1 => {
            let mut value = parse_event::<serde_json::Value>(data)?;
            let message = value
                .get_mut("message")
                .and_then(serde_json::Value::as_object_mut)
                .ok_or_else(|| invalid_response("OpenRouter message_start omitted message"))?;
            if let Some(provider) = message.remove("provider") {
                let valid = provider
                    .as_str()
                    .is_some_and(|provider| !provider.is_empty() && provider.len() <= 256);
                if !valid {
                    return Err(invalid_response(
                        "OpenRouter message_start provider was not a bounded string",
                    ));
                }
            }
            if let Some(container) = message.remove("container")
                && !container.is_null()
            {
                return Err(MessagesStreamError::new(
                    MessagesStreamErrorKind::UnsupportedSemantic,
                    MessagesStreamErrorPhase::Stream,
                    "OpenRouter message_start non-null context container is not admitted",
                ));
            }
            let usage = message
                .get_mut("usage")
                .and_then(serde_json::Value::as_object_mut)
                .ok_or_else(|| invalid_response("OpenRouter message_start omitted usage"))?;
            normalize_openrouter_usage(usage)?;
            let event = serde_json::from_value(value).map_err(|error| {
                invalid_response(format!("invalid OpenRouter message_start payload: {error}"))
            })?;
            Ok((event, None))
        }
    }
}

fn admit_message_diagnostics(
    message: &mut serde_json::Map<String, serde_json::Value>,
) -> Option<plakous_model::PromptCacheDiagnosis> {
    let diagnostics = message.remove("diagnostics")?;
    parse_message_diagnostics(&diagnostics)
}

/// Advisory cache telemetry. A malformed value records nothing and never fails the turn.
///
/// Unknown keys are ignored. Only `type` and `cache_missed_input_tokens` are
/// read, and the raw type string is not retained.
fn parse_message_diagnostics(
    diagnostics: &serde_json::Value,
) -> Option<plakous_model::PromptCacheDiagnosis> {
    let reason = diagnostics
        .as_object()?
        .get("cache_miss_reason")?
        .as_object()?;
    let kind = reason.get("type")?.as_str()?;
    let missed_input_tokens = match reason.get("cache_missed_input_tokens") {
        None => None,
        Some(value) => Some(non_negative_cache_miss_tokens(value)?),
    };
    Some(plakous_model::PromptCacheDiagnosis {
        reason: classify_cache_miss_reason(kind),
        missed_input_tokens,
    })
}

fn classify_cache_miss_reason(kind: &str) -> plakous_model::PromptCacheMissReason {
    match kind {
        "model_changed" => plakous_model::PromptCacheMissReason::ModelChanged,
        "system_changed" => plakous_model::PromptCacheMissReason::SystemChanged,
        "tools_changed" => plakous_model::PromptCacheMissReason::ToolsChanged,
        "messages_changed" => plakous_model::PromptCacheMissReason::MessagesChanged,
        "previous_message_not_found" => {
            plakous_model::PromptCacheMissReason::PreviousMessageNotFound
        }
        "unavailable" => plakous_model::PromptCacheMissReason::Unavailable,
        _ => plakous_model::PromptCacheMissReason::Other,
    }
}

fn non_negative_cache_miss_tokens(value: &serde_json::Value) -> Option<u64> {
    // `as_u64` rejects negatives, floats, and integers above `u64::MAX`.
    value.as_number().and_then(serde_json::Number::as_u64)
}

fn validate_claude_input_transformations(
    value: &serde_json::Value,
) -> Result<(), MessagesStreamError> {
    // The pinned native client continues and retains the original signed
    // thinking after this server-side input notification. It is not authority
    // for client-side history surgery or a context-window change.
    let valid = value.as_array().is_some_and(|items| {
        items.iter().all(|item| {
            let Some(object) = item.as_object() else {
                return false;
            };
            if object.len() != 3
                || object.get("type").and_then(serde_json::Value::as_str)
                    != Some("thinking_dropped")
                || object.get("reason").and_then(serde_json::Value::as_str)
                    != Some("prefix_binding_mismatch")
            {
                return false;
            }
            let Some((message, block)) = object
                .get("path")
                .and_then(serde_json::Value::as_str)
                .and_then(|path| path.strip_prefix("messages."))
                .and_then(|path| path.split_once(".content."))
            else {
                return false;
            };
            [message, block].into_iter().all(|index| {
                !index.is_empty()
                    && index.len() <= 6
                    && index.bytes().all(|byte| byte.is_ascii_digit())
            })
        })
    });
    if !valid {
        return Err(invalid_response(
            "Claude OAuth input transformation had an unsupported shape",
        ));
    }
    Ok(())
}

fn parse_thinking_delta(
    data: &str,
    extensions: MessagesStreamExtensions,
) -> Result<ThinkingDeltaWire, MessagesStreamError> {
    if extensions != MessagesStreamExtensions::ClaudeOauthV1 {
        return parse_event(data);
    }
    let mut value = parse_event::<serde_json::Value>(data)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| invalid_response("Claude OAuth thinking delta was not an object"))?;
    if let Some(estimated_tokens) = object.remove("estimated_tokens")
        && !estimated_tokens.is_null()
        && estimated_tokens.as_u64().is_none()
    {
        let shape = match estimated_tokens {
            serde_json::Value::Number(_) => "non-unsigned number",
            serde_json::Value::String(_) => "string",
            serde_json::Value::Bool(_) => "boolean",
            serde_json::Value::Array(_) => "array",
            serde_json::Value::Object(_) => "object",
            serde_json::Value::Null => "null",
        };
        return Err(invalid_response(format!(
            "Claude OAuth thinking token estimate had unsupported {shape} shape"
        )));
    }
    serde_json::from_value(value)
        .map_err(|error| invalid_response(format!("invalid Claude OAuth thinking delta: {error}")))
}

fn parse_message_delta(
    data: &str,
    extensions: MessagesStreamExtensions,
) -> Result<MessageDeltaEvent, MessagesStreamError> {
    match extensions {
        MessagesStreamExtensions::Strict => parse_event(data),
        MessagesStreamExtensions::ClaudeOauthV1 => {
            let mut value = parse_event::<serde_json::Value>(data)?;
            let event = value
                .as_object_mut()
                .ok_or_else(|| invalid_response("Claude OAuth message_delta was not an object"))?;
            if let Some(context_management) = event.remove("context_management") {
                validate_claude_context_management(&context_management)?;
            }
            serde_json::from_value(value).map_err(|error| {
                invalid_response(format!(
                    "invalid Claude OAuth message_delta payload: {error}"
                ))
            })
        }
        MessagesStreamExtensions::OpenRouterV1 => {
            let mut value = parse_event::<serde_json::Value>(data)?;
            let event = value
                .as_object_mut()
                .ok_or_else(|| invalid_response("OpenRouter message_delta was not an object"))?;
            if let Some(context) = event.remove("context_management")
                && !context.is_null()
            {
                return Err(MessagesStreamError::new(
                    MessagesStreamErrorKind::UnsupportedSemantic,
                    MessagesStreamErrorPhase::Stream,
                    "OpenRouter non-null context_management is not admitted",
                ));
            }
            let usage = event
                .get_mut("usage")
                .and_then(serde_json::Value::as_object_mut)
                .ok_or_else(|| invalid_response("OpenRouter message_delta omitted usage"))?;
            normalize_openrouter_usage(usage)?;
            serde_json::from_value(value).map_err(|error| {
                invalid_response(format!("invalid OpenRouter message_delta payload: {error}"))
            })
        }
    }
}

fn validate_claude_context_management(
    value: &serde_json::Value,
) -> Result<(), MessagesStreamError> {
    if value.is_null() {
        return Ok(());
    }
    let object = value.as_object().ok_or_else(|| {
        invalid_response("Claude OAuth context_management was not null or an object")
    })?;
    if object.len() != 1 || !object.contains_key("applied_edits") {
        return Err(invalid_response(
            "Claude OAuth context_management contained an unknown field",
        ));
    }
    let edits = object["applied_edits"].as_array().ok_or_else(|| {
        invalid_response("Claude OAuth context_management applied_edits was not an array")
    })?;
    if edits.len() > 64 {
        return Err(invalid_response(
            "Claude OAuth context_management exceeded the applied edit bound",
        ));
    }
    for edit in edits {
        if !edit.is_object() {
            return Err(invalid_response(
                "Claude OAuth context_management applied edit was not an object",
            ));
        }
        let bytes = serde_json::to_vec(edit)
            .map_err(|_| invalid_response("Claude OAuth context_management edit was invalid"))?;
        if bytes.len() > 16 * 1024 {
            return Err(invalid_response(
                "Claude OAuth context_management edit exceeded the byte bound",
            ));
        }
    }
    // Applied edits describe provider-local input processing for this call. The
    // canonical request remains complete and the route has no continuation
    // locator, so this metadata grants no replay or recovery authority.
    Ok(())
}

fn normalize_openrouter_usage(
    usage: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<(), MessagesStreamError> {
    for field in ["cache_creation_input_tokens", "cache_read_input_tokens"] {
        if usage.get(field).is_some_and(serde_json::Value::is_null) {
            usage.remove(field);
        }
    }
    if let Some(cost) = usage.remove("cost")
        && !cost.is_number()
    {
        return Err(invalid_response("OpenRouter usage cost was not a number"));
    }
    if let Some(is_byok) = usage.remove("is_byok")
        && !is_byok.is_boolean()
    {
        return Err(invalid_response(
            "OpenRouter usage is_byok was not a boolean",
        ));
    }
    if let Some(details) = usage.remove("cost_details") {
        validate_openrouter_cost_details(&details)?;
    }
    Ok(())
}

fn validate_openrouter_cost_details(
    details: &serde_json::Value,
) -> Result<(), MessagesStreamError> {
    const FIELDS: [&str; 3] = [
        "upstream_inference_cost",
        "upstream_inference_prompt_cost",
        "upstream_inference_completions_cost",
    ];
    let Some(details) = details.as_object() else {
        return Err(invalid_response(
            "OpenRouter usage cost_details was not an object",
        ));
    };
    if details
        .iter()
        .any(|(name, value)| FIELDS.contains(&name.as_str()) && !value.is_number())
    {
        return Err(invalid_response(
            "OpenRouter usage cost_details had a non-numeric cost",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ToolCaller {
    AbsentDirect,
    ExplicitDirect,
    Programmatic { tool_id: String },
}

fn parse_tool_caller(
    caller: Option<&RawValue>,
    dialect: &MessagesStreamDialectHooks,
) -> Result<ToolCaller, MessagesStreamError> {
    let Some(caller) = caller else {
        return Ok(ToolCaller::AbsentDirect);
    };
    let value: serde_json::Value = serde_json::from_str(caller.get())
        .map_err(|error| invalid_response(format!("invalid Messages tool caller: {error}")))?;
    let object = value
        .as_object()
        .ok_or_else(|| invalid_response("Messages tool caller must be a typed JSON object"))?;
    let kind = object
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid_response("Messages tool caller requires a string type"))?;
    match kind {
        "direct" if object.len() == 1 => Ok(ToolCaller::ExplicitDirect),
        "direct" => Err(invalid_response(
            "Messages direct tool caller contained unsupported lineage fields",
        )),
        kind if dialect.programmatic_tool_caller() == Some(kind) => {
            if object.len() != 2 {
                return Err(invalid_response(
                    "Messages programmatic tool caller contained unsupported fields",
                ));
            }
            let tool_id = object
                .get("tool_id")
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    invalid_response(
                        "Messages programmatic tool caller requires a nonempty tool_id",
                    )
                })?;
            Ok(ToolCaller::Programmatic {
                tool_id: tool_id.to_owned(),
            })
        }
        _ => Err(MessagesStreamError::new(
            MessagesStreamErrorKind::UnsupportedSemantic,
            MessagesStreamErrorPhase::Stream,
            "unsupported Messages tool caller type",
        )),
    }
}

fn raw_block_start_has_hosted_effect(data: &str) -> bool {
    let Ok(serde_json::Value::Object(event)) = serde_json::from_str(data) else {
        return false;
    };
    if event.get("type").and_then(serde_json::Value::as_str) != Some("content_block_start") {
        return false;
    }
    let Some(serde_json::Value::Object(block)) = event.get("content_block") else {
        return false;
    };
    if block
        .get("container_id")
        .is_some_and(|value| !value.is_null())
    {
        return true;
    }
    if let Some(caller) = block.get("caller").filter(|caller| !caller.is_null()) {
        let proven_direct = matches!(caller,
            serde_json::Value::Object(caller)
                if caller.len() == 1
                    && caller.get("type").and_then(serde_json::Value::as_str) == Some("direct")
        );
        if !proven_direct {
            return true;
        }
    }
    block
        .get("type")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|kind| kind.starts_with("server_tool") || kind == "web_search_tool_result")
}

fn reject_citations(citations: Option<&RawValue>) -> Result<(), MessagesStreamError> {
    let Some(citations) = citations else {
        return Ok(());
    };
    let value: serde_json::Value = serde_json::from_str(citations.get())
        .map_err(|error| invalid_response(format!("invalid Messages citations: {error}")))?;
    if value.is_null() || value.as_array().is_some_and(Vec::is_empty) {
        Ok(())
    } else {
        Err(MessagesStreamError::new(
            MessagesStreamErrorKind::UnsupportedSemantic,
            MessagesStreamErrorPhase::Stream,
            "Messages citations are explicitly excluded by adapter revision 1",
        ))
    }
}

fn validate_opaque_string(value: &RawValue, field: &str) -> Result<(), MessagesStreamError> {
    serde_json::from_str::<String>(value.get())
        .map(|_| ())
        .map_err(|_| {
            invalid_response(format!(
                "Messages {field} must be an exact JSON string payload"
            ))
        })
}

fn nonempty_opaque_signature(
    value: Option<Box<RawValue>>,
    field: &str,
) -> Result<Option<Box<RawValue>>, MessagesStreamError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let parsed = serde_json::from_str::<String>(value.get()).map_err(|_| {
        invalid_response(format!(
            "Messages {field} must be an exact JSON string payload"
        ))
    })?;
    Ok((!parsed.is_empty()).then_some(value))
}

fn parse_stop_details(raw: Box<RawValue>) -> Result<MessagesStopDetails, MessagesStreamError> {
    #[derive(Deserialize)]
    struct StopDetailsEnvelope {
        #[serde(rename = "type")]
        kind: String,
    }

    let envelope: StopDetailsEnvelope = serde_json::from_str(raw.get())
        .map_err(|error| invalid_response(format!("invalid Messages stop_details: {error}")))?;
    Ok(MessagesStopDetails {
        kind: MessagesStopDetailsKind::parse(&envelope.kind)?,
        raw,
    })
}

fn validate_stop_blocks(
    reason: MessagesStopReason,
    blocks: &[ContentBlock],
    hosted_effect_capable: bool,
    container_id: Option<&str>,
) -> Result<(), MessagesStreamError> {
    let client_proposals = blocks
        .iter()
        .filter(|block| {
            matches!(
                block, ContentBlock::ToolCall(call)
                    if !matches!(call.owner(), ToolExecutionOwner::ProviderHosted { .. })
            )
        })
        .count();
    let provider_proposals = blocks
        .iter()
        .filter(|block| {
            matches!(
                block, ContentBlock::ToolCall(call)
                    if matches!(call.owner(), ToolExecutionOwner::ProviderHosted { .. })
            )
        })
        .count();
    let has_client_tool = client_proposals != 0;
    let has_refusal = blocks
        .iter()
        .any(|block| matches!(block, ContentBlock::Refusal(_)));
    if reason != MessagesStopReason::Refusal
        && matches!(reason, MessagesStopReason::ToolUse) != has_client_tool
    {
        return Err(invalid_response(format!(
            "Messages terminal tool proposal mismatch: stop_reason={} client_proposals={client_proposals} provider_proposals={provider_proposals}; tool_use requires a client proposal and other non-refusal stop reasons cannot complete client proposals",
            reason.wire_name(),
        )));
    }
    if matches!(reason, MessagesStopReason::PauseTurn)
        && (!hosted_effect_capable || container_id.is_none() || has_client_tool)
    {
        return Err(invalid_response(
            "Messages pause_turn requires an admitted hosted request, an exact container, and no client tool calls",
        ));
    }
    if has_refusal && !matches!(reason, MessagesStopReason::Refusal) {
        return Err(invalid_response(
            "Messages refusal stop_reason did not match terminal refusal blocks",
        ));
    }
    Ok(())
}

fn map_finish(
    stop_reason: MessagesStopReason,
    stop_sequence: Option<String>,
    stop_details: Option<MessagesStopDetails>,
) -> Result<(FinishDetails, MessagesStopOutcome), MessagesStreamError> {
    if matches!(stop_reason, MessagesStopReason::Fallback)
        || stop_details
            .as_ref()
            .is_some_and(|details| details.kind == MessagesStopDetailsKind::Fallback)
    {
        return Err(MessagesStreamError::new(
            MessagesStreamErrorKind::UnsupportedSemantic,
            MessagesStreamErrorPhase::Stream,
            "Messages model fallback is explicitly excluded by adapter revision 1",
        ));
    }
    if matches!(stop_reason, MessagesStopReason::StopSequence) != stop_sequence.is_some() {
        return Err(invalid_response(
            "Messages stop_sequence reason and value must be supplied together",
        ));
    }
    if let Some(details) = &stop_details {
        let compatible = matches!(
            (stop_reason, details.kind),
            (
                MessagesStopReason::ToolUse,
                MessagesStopDetailsKind::ClientTool
            ) | (
                MessagesStopReason::Refusal,
                MessagesStopDetailsKind::Refusal
            ) | (
                MessagesStopReason::ModelContextWindowExceeded,
                MessagesStopDetailsKind::ModelContextWindowExceeded
            )
        );
        if !compatible {
            return Err(invalid_response(
                "Messages stop_reason and stop_details type disagree",
            ));
        }
    }
    let reason = match stop_reason {
        MessagesStopReason::ToolUse => FinishReason::ToolCalls,
        MessagesStopReason::MaxTokens | MessagesStopReason::ModelContextWindowExceeded => {
            FinishReason::Length
        }
        MessagesStopReason::EndTurn | MessagesStopReason::StopSequence => FinishReason::Stop,
        MessagesStopReason::PauseTurn => FinishReason::ProviderContinuation,
        MessagesStopReason::Refusal => FinishReason::Refusal,
        MessagesStopReason::Fallback => {
            unreachable!("unsupported Messages stop reasons returned before canonical mapping")
        }
    };
    let mut detail = format!("stop_reason={}", stop_reason.wire_name());
    if let Some(value) = &stop_sequence {
        detail.push_str(";stop_sequence=");
        detail.push_str(value);
    }
    if let Some(value) = &stop_details {
        detail.push_str(";stop_details.type=");
        detail.push_str(value.kind.wire_name());
    }
    if reason == FinishReason::Refusal {
        append_refusal_diagnostic(&mut detail, stop_details.as_ref())?;
    }
    Ok((
        FinishDetails::new(reason, Some(detail)),
        MessagesStopOutcome {
            reason: stop_reason,
            stop_sequence,
            details: stop_details,
        },
    ))
}

fn append_refusal_diagnostic(
    detail: &mut String,
    stop_details: Option<&MessagesStopDetails>,
) -> Result<(), MessagesStreamError> {
    if let Some(stop_details) = stop_details {
        #[derive(Deserialize)]
        struct RefusalDiagnostic {
            category: Option<String>,
            explanation: Option<String>,
        }
        let diagnostic: RefusalDiagnostic = serde_json::from_str(stop_details.raw.get())
            .map_err(|_| invalid_response("invalid Messages refusal diagnostic fields"))?;
        for (label, value) in [
            ("category", diagnostic.category),
            ("explanation", diagnostic.explanation),
        ] {
            if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
                detail.push(';');
                detail.push_str(label);
                detail.push('=');
                detail.extend(value.chars().map(|character| {
                    if character.is_control() {
                        ' '
                    } else {
                        character
                    }
                }));
            }
        }
    }
    plakous_model::SafeDiagnostic::new(&*detail)
        .as_str()
        .clone_into(detail);
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "usage pins fail fast on fixture identity"
)]
mod usage_wire_tests {
    use super::{
        MessageDeltaUsage, MessagesStreamExtensions, ThinkingDeltaWire, UsageWire,
        parse_thinking_delta,
    };

    const LIVE_SUBSCRIPTION_USAGE: &str = r#"{
        "input_tokens": 2,
        "cache_creation_input_tokens": 51132,
        "cache_read_input_tokens": 0,
        "output_tokens": 4,
        "output_tokens_details": { "thinking_tokens": 0 },
        "server_tool_use": { "web_search_requests": 0, "web_fetch_requests": 0 },
        "service_tier": "standard",
        "cache_creation": {
            "ephemeral_1h_input_tokens": 51132,
            "ephemeral_5m_input_tokens": 0
        },
        "inference_geo": "not_available",
        "iterations": [{
            "input_tokens": 2,
            "output_tokens": 4,
            "cache_read_input_tokens": 0,
            "cache_creation_input_tokens": 51132,
            "cache_creation": {
                "ephemeral_5m_input_tokens": 0,
                "ephemeral_1h_input_tokens": 51132
            },
            "type": "message"
        }],
        "speed": "standard"
    }"#;

    #[test]
    fn live_claude_oauth_usage_admits_subscription_receipt() {
        let usage: UsageWire = serde_json::from_str(LIVE_SUBSCRIPTION_USAGE).expect("pinned usage");
        assert_eq!(usage.input_tokens, 2);
        assert_eq!(usage.cache_creation_input_tokens, 51132);
        assert_eq!(usage.output_tokens, 4);
        let provider = usage.into_provider();
        assert_eq!(provider.cache_creation_input_tokens, 51132);
        assert_eq!(provider.cache_read_input_tokens, 0);
    }

    #[test]
    fn messages_usage_preserves_cumulative_thinking_breakdown() {
        let initial: UsageWire = serde_json::from_str(
            r#"{"input_tokens":7,"output_tokens":10,"output_tokens_details":{"thinking_tokens":4}}"#,
        )
        .expect("initial usage");
        let provider = initial.into_provider();
        let canonical = provider.into_canonical().expect("canonical initial usage");
        assert_eq!(canonical.output_tokens(), 10);
        assert_eq!(canonical.reasoning_tokens(), 4);

        let update: MessageDeltaUsage = serde_json::from_str(
            r#"{"output_tokens":12,"output_tokens_details":{"thinking_tokens":5}}"#,
        )
        .expect("terminal usage");
        let provider = provider.merge(update.into_family()).expect("usage update");
        let canonical = provider.into_canonical().expect("canonical terminal usage");
        assert_eq!(canonical.output_tokens(), 12);
        assert_eq!(canonical.reasoning_tokens(), 5);
    }

    #[test]
    fn messages_usage_rejects_inconsistent_thinking_accounting() {
        let usage: UsageWire = serde_json::from_str(
            r#"{"input_tokens":1,"output_tokens":2,"output_tokens_details":{"thinking_tokens":3}}"#,
        )
        .expect("typed usage");
        assert_eq!(
            usage.into_provider().into_canonical(),
            Err("Messages thinking usage exceeded output usage")
        );

        let initial: UsageWire = serde_json::from_str(
            r#"{"input_tokens":1,"output_tokens":4,"output_tokens_details":{"thinking_tokens":3}}"#,
        )
        .expect("initial usage");
        let update: MessageDeltaUsage = serde_json::from_str(
            r#"{"output_tokens":5,"output_tokens_details":{"thinking_tokens":2}}"#,
        )
        .expect("decreasing thinking update");
        assert_eq!(
            initial.into_provider().merge(update.into_family()),
            Err("Messages cumulative usage decreased")
        );
    }

    #[test]
    fn unknown_usage_keys_are_ignored_and_accounting_is_unchanged() {
        let usage = serde_json::from_str::<UsageWire>(
            r#"{"input_tokens":1,"output_tokens":2,"future_counter":{"x":1},"speed":"fast"}"#,
        )
        .expect("additive usage keys");
        let provider = usage.into_provider();
        assert_eq!((provider.input_tokens, provider.output_tokens), (1, 2));
        serde_json::from_str::<UsageWire>(r#"{"input_tokens":"1","output_tokens":1}"#)
            .expect_err("a known counter keeps its type");
    }

    #[test]
    fn live_claude_oauth_message_delta_usage_admits_subscription_receipt() {
        let usage: MessageDeltaUsage = serde_json::from_str(
            r#"{
                "output_tokens": 8,
                "cache_creation": {
                    "ephemeral_5m_input_tokens": 0,
                    "ephemeral_1h_input_tokens": 0
                },
                "service_tier": "standard",
                "inference_geo": "not_available",
                "speed": "standard"
            }"#,
        )
        .expect("pinned message_delta usage");
        assert_eq!(usage.output_tokens, 8);
    }

    #[test]
    fn claude_code_empty_thinking_delta_is_observed() {
        let delta: ThinkingDeltaWire =
            serde_json::from_str(r#"{"type":"thinking_delta","thinking":""}"#)
                .expect("empty thinking delta is observed on Claude Code");
        assert!(delta.thinking.is_empty());
    }

    #[test]
    fn claude_opus_thinking_estimate_is_a_nonsemantic_unsigned_hint() {
        let witnessed = r#"{"type":"thinking_delta","thinking":"","estimated_tokens":null}"#;
        assert!(parse_thinking_delta(witnessed, MessagesStreamExtensions::ClaudeOauthV1).is_ok());
        for extension in [
            MessagesStreamExtensions::Strict,
            MessagesStreamExtensions::OpenRouterV1,
        ] {
            assert!(parse_thinking_delta(witnessed, extension).is_err());
        }
        let estimate = r#"{"type":"thinking_delta","thinking":"step","estimated_tokens":42}"#;
        assert_eq!(
            parse_thinking_delta(estimate, MessagesStreamExtensions::ClaudeOauthV1)
                .expect("unsigned estimate is an ignored hint")
                .thinking,
            "step"
        );
        assert!(parse_thinking_delta(estimate, MessagesStreamExtensions::Strict).is_err());
        for (invalid, shape) in [
            ("-1", "non-unsigned number"),
            ("1.5", "non-unsigned number"),
            ("\"private-sentinel\"", "string"),
            ("true", "boolean"),
            ("{}", "object"),
        ] {
            let delta = format!(
                "{{\"type\":\"thinking_delta\",\"thinking\":\"\",\"estimated_tokens\":{invalid}}}"
            );
            let error = parse_thinking_delta(&delta, MessagesStreamExtensions::ClaudeOauthV1)
                .err()
                .expect("unsupported shape must be rejected");
            assert!(error.message().contains(shape));
            assert!(!error.message().contains("private-sentinel"));
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "projector fixtures fail fast on identity construction"
)]
mod projector_tests {
    use std::sync::Arc;

    use plakous_model::{
        ContentBlock, Digest, DigestDomain, EffectiveProviderConfigurationId, FinishReason,
        ModelEvent, ModelId, NamespaceId, ProtocolId, SelectedProviderConfigurationId, SystemClock,
        SystemIdGenerator, ToolName,
    };

    use super::{
        MessagesStreamDialectHooks, MessagesStreamExtensions, MessagesStreamProjector,
        PreparedMessagesStreamBinding, ToolCaller, parse_tool_caller,
    };
    use crate::request::ToolOwnership;
    use crate::{MessagesRequestPolicy, MessagesStreamErrorKind};

    fn projector_with_hosted(hosted: bool) -> MessagesStreamProjector {
        let selected: SelectedProviderConfigurationId = serde_json::from_value(
            serde_json::to_value(
                Digest::sha256(
                    DigestDomain::SelectedProviderConfiguration,
                    5,
                    &[b"messages-projector-selected"],
                )
                .expect("selected digest"),
            )
            .expect("selected json"),
        )
        .expect("selected");
        let effective: EffectiveProviderConfigurationId = serde_json::from_value(
            serde_json::to_value(
                Digest::sha256(
                    DigestDomain::EffectiveProviderConfiguration,
                    5,
                    &[b"messages-projector-effective"],
                )
                .expect("effective digest"),
            )
            .expect("effective json"),
        )
        .expect("effective");
        let digest = Digest::sha256(DigestDomain::RequestPlan, 1, &[b"messages-projector-test"])
            .expect("digest");
        let mut policy = MessagesRequestPolicy::new(
            ProtocolId::new("anthropic.messages").expect("protocol"),
            ModelId::new("claude-sonnet-5").expect("model"),
            None,
            digest,
            digest,
            digest,
            selected,
            effective,
            128,
            None,
            "2023-06-01",
            "oauth-2025-04-20,claude-code-20250219",
        );
        if hosted {
            policy = policy.with_programmatic_tool_calling(
                vec![
                    "code_execution_20260120".to_owned(),
                    "code_execution_20260521".to_owned(),
                ],
                "code_execution_20260120",
                NamespaceId::new("anthropic.programmatic_tool").expect("namespace"),
            );
        }
        let prepared = PreparedMessagesStreamBinding::new(
            policy,
            digest,
            ToolOwnership::default(),
            MessagesStreamDialectHooks::new(
                "code_execution",
                NamespaceId::new("anthropic.programmatic_tool").expect("namespace"),
            ),
        );
        MessagesStreamProjector::new(prepared, Arc::new(SystemClock), Arc::new(SystemIdGenerator))
            .expect("projector")
    }

    fn projector() -> MessagesStreamProjector {
        projector_with_hosted(false)
    }

    fn openrouter_projector() -> MessagesStreamProjector {
        let mut projector = projector();
        projector.prepared.dialect = MessagesStreamDialectHooks::client_tools_only()
            .with_extensions(MessagesStreamExtensions::OpenRouterV1);
        projector
    }

    fn claude_oauth_projector() -> MessagesStreamProjector {
        let mut projector = projector();
        projector.prepared.dialect = MessagesStreamDialectHooks::client_tools_only()
            .with_extensions(MessagesStreamExtensions::ClaudeOauthV1);
        projector
    }

    const OPENROUTER_STREAM: &str = include_str!("../tests/fixtures/stream_openrouter_v1.sse");
    const OPENROUTER_INTERLEAVED_TOOL_STREAM: &str =
        include_str!("../tests/fixtures/stream_openrouter_interleaved_tool.sse");

    const CLAUDE_OAUTH_CONTEXT_MANAGEMENT_STREAM: &str = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_context_management\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-5\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"CLAUDE_FIXED_OK\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":1},\"context_management\":{\"applied_edits\":[]}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );

    fn refusal_stream(block: Option<&str>, reason: &str, details: Option<&str>) -> String {
        use std::fmt::Write as _;
        let start = message_start_stream("");
        let mut stream = format!("{}\n\n", start.split("\n\n").next().expect("start"));
        if let Some(block) = block {
            write!(stream,
                "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{block}}}\n\n\
                 event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n"
            ).expect("write fixture");
        }
        let details =
            details.map_or_else(String::new, |value| format!(",\"stop_details\":{value}"));
        write!(stream,
            "event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"{reason}\",\"stop_sequence\":null{details}}},\"usage\":{{\"output_tokens\":1}}}}\n\n\
             event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
        ).expect("write fixture");
        stream
    }

    #[test]
    fn documented_refusal_shapes_discard_provisional_content_and_preserve_terminal() {
        let details = r#"{"type":"refusal","category":"synthetic-policy","explanation":"synthetic explanation"}"#;
        for block in [
            None,
            Some(r#"{"type":"text","text":"partial answer"}"#),
            Some(r#"{"type":"refusal","refusal":"cannot comply"}"#),
            Some(
                r#"{"type":"thinking","thinking":"provisional reasoning","signature":"synthetic-signature"}"#,
            ),
            Some(r#"{"type":"redacted_thinking","data":"synthetic-redacted"}"#),
            Some(
                r#"{"type":"tool_use","id":"refused-call","name":"read_file","input":{"path":"synthetic.txt"}}"#,
            ),
        ] {
            for details in [
                None,
                Some(details),
                Some(r#"{"type":"refusal","category":null,"explanation":null}"#),
            ] {
                for mut projector in [
                    projector(),
                    claude_oauth_projector(),
                    openrouter_projector(),
                ] {
                    let mut stream = refusal_stream(block, "refusal", details);
                    if projector.prepared.dialect.extensions()
                        == MessagesStreamExtensions::OpenRouterV1
                    {
                        stream.push_str("event: data\ndata: [DONE]\n\n");
                    }
                    let mut assembler = plakous_model::ModelResponseAssembler::new(
                        projector.prepared.plan_digest(),
                    )
                    .expect("assembler");
                    let mut response = None;
                    for chunk in stream.as_bytes().chunks(7) {
                        for event in projector.push(chunk).expect("documented refusal SSE") {
                            if let Some(terminal) =
                                assembler.push(event).expect("canonical refusal events")
                            {
                                response = Some(terminal);
                            }
                        }
                    }
                    projector.finish_eof().expect("terminal EOF");
                    let (terminal, _, stop) = projector
                        .into_terminal_projection()
                        .expect("refusal terminal")
                        .into_parts();
                    assert_eq!(response.as_ref(), Some(&terminal));
                    assert_eq!(terminal.finish().reason(), FinishReason::Refusal);
                    assert!(terminal.output().is_empty());
                    assert!(terminal.usage().is_some());
                    assert!(terminal.response_id().is_some());
                    assert_eq!(
                        stop.details()
                            .map(|value| serde_json::from_str::<serde_json::Value>(
                                value.raw().get()
                            )
                            .expect("details JSON")),
                        details.map(|value| serde_json::from_str::<serde_json::Value>(value)
                            .expect("fixture details"))
                    );
                }
            }
        }
    }

    #[test]
    fn closed_client_input_is_validated_only_after_non_refusal_terminal() {
        let block = r#"{"type":"tool_use","id":"provisional-call","name":"read_file","input":{}}"#;
        for input in [
            r#"{"path":"#,
            r#"{"path":"unfinished"#,
            "not-json",
            "[1]",
            "{]",
        ] {
            for reason in ["refusal", "tool_use"] {
                let delta = serde_json::json!({
                    "type": "content_block_delta", "index": 0,
                    "delta": {"type": "input_json_delta", "partial_json": input},
                });
                let stream = refusal_stream(Some(block), reason, None).replace(
                    "event: content_block_stop\n",
                    &format!(
                        "event: content_block_delta\ndata: {delta}\n\nevent: content_block_stop\n"
                    ),
                );
                let stream = stream.replace("\"index\":0", "\"index\":1").replacen(
                    "event: content_block_start\n",
                    concat!(
                        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"partial response\"}}\n\n",
                        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                        "event: content_block_start\n",
                    ), 1,
                );
                let mut projector = projector();
                let mut assembler =
                    plakous_model::ModelResponseAssembler::new(projector.prepared.plan_digest())
                        .expect("assembler");
                let mut terminal = None;
                let mut rejected = false;
                for chunk in stream.as_bytes().chunks(7) {
                    let Ok(events) = projector.push(chunk) else {
                        rejected = true;
                        break;
                    };
                    for event in events {
                        if let Some(response) =
                            assembler.push(event).expect("revoked provisional events")
                        {
                            terminal = Some(response);
                        }
                    }
                }
                if reason == "tool_use" {
                    assert!(rejected, "invalid tool input must not dispatch");
                    continue;
                }
                assert!(!rejected, "revoked input needs no JSON interpretation");
                projector.finish_eof().expect("closed refusal EOF");
                assert!(terminal.expect("refusal terminal").output().is_empty());
            }
        }
        let stream = refusal_stream(Some(block), "refusal", None).replace(
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "",
        );
        assert!(
            projector().push(stream.as_bytes()).is_err(),
            "wire closure remains required"
        );
    }

    #[test]
    fn refusal_details_are_optional_bounded_and_sanitized() {
        for details in [
            None,
            Some(r#"{"type":"refusal"}"#),
            Some(r#"{"type":"refusal","category":null,"explanation":null}"#),
        ] {
            let mut projector = projector();
            projector
                .push(refusal_stream(None, "refusal", details).as_bytes())
                .expect("optional details");
            projector.finish_eof().expect("EOF");
            let (response, _, _) = projector
                .into_terminal_projection()
                .expect("terminal")
                .into_parts();
            let detail = response.finish().detail().expect("terminal reason");
            assert!(!detail.contains("category="));
            assert!(!detail.contains("explanation="));
        }
        let raw = serde_json::json!({
            "type": "refusal", "category": "synthetic-policy",
            "explanation": format!("\u{1b}[2J\nBearer sk-private-secret {}", "x".repeat(4096)),
        })
        .to_string();
        let mut projector = projector();
        projector
            .push(refusal_stream(None, "refusal", Some(&raw)).as_bytes())
            .expect("diagnostic");
        projector.finish_eof().expect("EOF");
        let (response, _, _) = projector
            .into_terminal_projection()
            .expect("terminal")
            .into_parts();
        let detail = response.finish().detail().expect("diagnostic");
        assert!(detail.contains("category=synthetic-policy"));
        assert!(detail.contains("explanation="));
        assert!(!detail.contains("sk-private-secret"));
        assert!(!detail.chars().any(char::is_control));
        assert!(detail.len() <= plakous_model::MAX_SAFE_DIAGNOSTIC_BYTES);
    }

    #[test]
    fn refusal_cannot_discard_possible_hosted_effects() {
        let stream = refusal_stream(
            Some(
                r#"{"type":"server_tool_use","id":"server-1","name":"code_execution","input":{}}"#,
            ),
            "refusal",
            None,
        );
        let mut projector = projector_with_hosted(true);
        let error = projector
            .push(stream.as_bytes())
            .expect_err("hosted refusal needs reconciliation");
        assert_eq!(error.kind(), MessagesStreamErrorKind::UnsupportedSemantic);
        assert!(projector.hosted_effects_possible());
        assert!(projector.into_terminal_projection().is_err());
    }

    #[test]
    fn terminal_tool_mismatch_reports_only_fixed_shape() {
        for (block, reason, counts) in [
            (
                Some(
                    r#"{"type":"tool_use","id":"private-id","name":"private-tool","input":{"secret":"private-argument"}}"#,
                ),
                "end_turn",
                "client_proposals=1 provider_proposals=0",
            ),
            (None, "tool_use", "client_proposals=0 provider_proposals=0"),
        ] {
            let error = projector()
                .push(refusal_stream(block, reason, None).as_bytes())
                .expect_err("unsupported terminal proposal shape");
            assert_eq!(error.kind(), MessagesStreamErrorKind::InvalidResponse);
            let message = error.to_string();
            assert!(message.contains(&format!("stop_reason={reason}")));
            assert!(message.contains(counts));
            assert!(!message.contains("private"));
        }
    }

    #[test]
    fn refusal_does_not_relax_terminal_contradictions_or_tool_guards() {
        for stream in [
            refusal_stream(
                Some(r#"{"type":"refusal","refusal":"cannot comply"}"#),
                "end_turn",
                None,
            ),
            refusal_stream(None, "refusal", Some(r#"{"type":"client_tool"}"#)),
            refusal_stream(None, "end_turn", Some(r#"{"type":"refusal"}"#)),
        ] {
            assert_eq!(
                projector()
                    .push(stream.as_bytes())
                    .expect_err("contradictory terminal")
                    .kind(),
                MessagesStreamErrorKind::InvalidResponse
            );
        }
        let fallback = refusal_stream(None, "refusal", Some(r#"{"type":"fallback"}"#));
        assert_eq!(
            projector()
                .push(fallback.as_bytes())
                .expect_err("no automatic fallback")
                .kind(),
            MessagesStreamErrorKind::UnsupportedSemantic
        );
    }

    #[test]
    fn claude_oauth_accepts_bounded_context_management_without_weakening_strict_messages() {
        let mut oauth = claude_oauth_projector();
        let events = oauth
            .push(CLAUDE_OAUTH_CONTEXT_MANAGEMENT_STREAM.as_bytes())
            .expect("pinned Claude OAuth context management delta");
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ModelEvent::ResponseCompleted(_)))
        );
        oauth.finish_eof().expect("complete Claude OAuth EOF");
        oauth
            .into_terminal_projection()
            .expect("Claude OAuth terminal projection");

        let error = projector()
            .push(CLAUDE_OAUTH_CONTEXT_MANAGEMENT_STREAM.as_bytes())
            .expect_err("strict Messages must reject the OAuth-only field");
        assert_eq!(error.kind(), MessagesStreamErrorKind::InvalidResponse);

        let mut unknown = claude_oauth_projector();
        let stream = CLAUDE_OAUTH_CONTEXT_MANAGEMENT_STREAM.replacen(
            "{\"applied_edits\":[]}",
            "{\"future_edits\":[]}",
            1,
        );
        assert_eq!(
            unknown
                .push(stream.as_bytes())
                .expect_err("unknown context management shape")
                .kind(),
            MessagesStreamErrorKind::InvalidResponse
        );
    }

    fn message_start_stream(message_extra: &str) -> String {
        format!(
            concat!(
                "event: message_start\n",
                "data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_container\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-5\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null{extra},\"usage\":{{\"input_tokens\":1,\"output_tokens\":0}}}}}}\n\n",
                "event: content_block_start\n",
                "data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n",
                "event: content_block_delta\n",
                "data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"READY\"}}}}\n\n",
                "event: content_block_stop\n",
                "data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n",
                "event: message_delta\n",
                "data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\",\"stop_sequence\":null}},\"usage\":{{\"output_tokens\":1}}}}\n\n",
                "event: message_stop\n",
                "data: {{\"type\":\"message_stop\"}}\n\n",
            ),
            extra = message_extra
        )
    }

    #[test]
    fn message_start_admits_the_container_receipt_without_widening_the_message_object() {
        // The Messages API reports the code-execution container on every
        // `message_start`; it is null whenever the turn allocated none.
        for extra in ["", ",\"container\":null"] {
            let mut projector = projector();
            let events = projector
                .push(message_start_stream(extra).as_bytes())
                .expect("message_start with an absent or null container");
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, ModelEvent::ResponseCompleted(_)))
            );
            projector.finish_eof().expect("EOF");
            projector
                .into_terminal_projection()
                .expect("terminal projection");
        }

        // A reported container is the hosted artifact identity, so it binds
        // exactly like the `message_delta` container.
        let hosted_container =
            ",\"container\":{\"id\":\"container_1\",\"expires_at\":\"2026-09-08T00:00:00Z\"}";
        let mut hosted = projector_with_hosted(true);
        hosted
            .push(message_start_stream(hosted_container).as_bytes())
            .expect("hosted message_start container");
        assert_eq!(hosted.container_id.as_deref(), Some("container_1"));

        // Unadmitted or malformed containers still fail closed.
        assert_eq!(
            projector()
                .push(message_start_stream(hosted_container).as_bytes())
                .expect_err("client-only requests may not receive a container")
                .kind(),
            MessagesStreamErrorKind::InvalidEvent
        );
        assert_eq!(
            projector_with_hosted(true)
                .push(
                    message_start_stream(
                        ",\"container\":{\"id\":\"\",\"expires_at\":\"2026-09-08T00:00:00Z\"}"
                    )
                    .as_bytes()
                )
                .expect_err("empty container ID")
                .kind(),
            MessagesStreamErrorKind::InvalidEvent
        );
        assert_eq!(
            projector_with_hosted(true)
                .push(message_start_stream(",\"container\":{\"id\":\"container_1\"}").as_bytes())
                .expect_err("container missing its documented expiry")
                .kind(),
            MessagesStreamErrorKind::InvalidResponse
        );

        // A truly unknown top-level message field is still rejected.
        assert_eq!(
            projector()
                .push(message_start_stream(",\"future_field\":1").as_bytes())
                .expect_err("unknown message_start field")
                .kind(),
            MessagesStreamErrorKind::InvalidResponse
        );
    }

    #[test]
    fn claude_oauth_message_start_admits_bounded_context_management_only_for_the_costume() {
        let stream = message_start_stream(",\"context_management\":{\"applied_edits\":[]}");
        let mut oauth = claude_oauth_projector();
        let events = oauth
            .push(stream.as_bytes())
            .expect("pinned Claude OAuth message_start context management");
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ModelEvent::ResponseCompleted(_)))
        );
        oauth.finish_eof().expect("complete Claude OAuth EOF");
        oauth
            .into_terminal_projection()
            .expect("Claude OAuth terminal projection");

        assert_eq!(
            projector()
                .push(stream.as_bytes())
                .expect_err("strict Messages must reject the OAuth-only field")
                .kind(),
            MessagesStreamErrorKind::InvalidResponse
        );
        assert_eq!(
            claude_oauth_projector()
                .push(
                    message_start_stream(",\"context_management\":{\"future_edits\":[]}")
                        .as_bytes()
                )
                .expect_err("unknown context management shape")
                .kind(),
            MessagesStreamErrorKind::InvalidResponse
        );
        assert_eq!(
            claude_oauth_projector()
                .push(message_start_stream(",\"future_field\":1").as_bytes())
                .expect_err("unknown message_start field under the costume")
                .kind(),
            MessagesStreamErrorKind::InvalidResponse
        );
    }

    #[test]
    fn claude_prefix_binding_notification_does_not_fail_or_rewrite_the_response() {
        // Body-only native 2.1.284 / Sonnet 5.5 OAuth observation: changing
        // an earlier input prefix while retaining signed thinking produces
        // this notification. Native continuation retains the thinking.
        let notification = serde_json::json!([
            {"type":"thinking_dropped", "path":"messages.2.content.0",
             "reason":"prefix_binding_mismatch"},
            {"type":"thinking_dropped", "path":"messages.17.content.3",
             "reason":"prefix_binding_mismatch"}
        ]);
        let mut notified = claude_oauth_projector();
        notified
            .push(
                message_start_stream(&format!(",\"input_transformations\":{notification}"))
                    .as_bytes(),
            )
            .expect("witnessed prefix-binding notification must complete");
        notified.finish_eof().expect("notified complete EOF");
        let mut baseline = claude_oauth_projector();
        baseline
            .push(message_start_stream("").as_bytes())
            .expect("baseline response");
        baseline.finish_eof().expect("baseline complete EOF");
        let (notified, model, stop) = notified
            .into_terminal_projection()
            .expect("notified projection")
            .into_parts();
        let (baseline, baseline_model, baseline_stop) = baseline
            .into_terminal_projection()
            .expect("baseline projection")
            .into_parts();
        assert_eq!(model, baseline_model);
        assert_eq!(stop, baseline_stop);
        assert_eq!(notified.output().len(), 1);
        assert_eq!(notified.output()[0].blocks(), baseline.output()[0].blocks());
        assert_eq!(notified.finish(), baseline.finish());
        assert_eq!(notified.usage(), baseline.usage());
        assert!(
            projector()
                .push(
                    message_start_stream(&format!(",\"input_transformations\":{notification}"))
                        .as_bytes()
                )
                .is_err()
        );

        for invalid in [
            serde_json::json!({"type":"clear","path":"messages.2.content.0","reason":"prefix_binding_mismatch"}),
            serde_json::json!({"type":"thinking_dropped","path":"messages.2.content.0","reason":"unknown"}),
            serde_json::json!({"type":"thinking_dropped","path":"messages.2","reason":"prefix_binding_mismatch"}),
            serde_json::json!({"type":"thinking_dropped","path":"messages.-2.content.0","reason":"prefix_binding_mismatch"}),
            serde_json::json!({"type":"thinking_dropped","path":"messages.2.content.0.text","reason":"prefix_binding_mismatch"}),
            serde_json::json!({"type":"thinking_dropped","path":"messages.2.content.0","reason":"prefix_binding_mismatch","future":true}),
            serde_json::json!({"type":"thinking_dropped","path":2,"reason":"prefix_binding_mismatch"}),
        ] {
            let stream = message_start_stream(&format!(",\"input_transformations\":[{invalid}]"));
            let error = claude_oauth_projector()
                .push(stream.as_bytes())
                .expect_err("unqualified transformation must fail closed");
            assert_eq!(error.kind(), MessagesStreamErrorKind::InvalidResponse);
            assert!(
                !error.to_string().contains("messages.2"),
                "no raw path in diagnostics"
            );
        }
    }

    #[test]
    fn claude_opus_message_start_admits_null_diagnostics_and_empty_transformations() {
        let witnessed = message_start_stream(",\"diagnostics\":null,\"input_transformations\":[]");
        let mut oauth = claude_oauth_projector();
        let events = oauth
            .push(witnessed.as_bytes())
            .expect("observed Opus envelope");
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ModelEvent::ResponseCompleted(_)))
        );
        oauth.finish_eof().expect("complete OAuth EOF");
        assert!(oauth.into_terminal_projection().is_ok());

        for extension in [
            ",\"input_transformations\":[{\"type\":\"clear\"}]",
            ",\"input_transformations\":null",
            ",\"future_field\":1",
        ] {
            let error = claude_oauth_projector()
                .push(message_start_stream(extension).as_bytes())
                .expect_err("unwitnessed envelope must fail closed");
            assert_eq!(error.kind(), MessagesStreamErrorKind::InvalidResponse);
        }
        // The strict route admits the same diagnostics object as Claude OAuth.
        // Input transformations and any other message field stay refused.
        projector()
            .push(message_start_stream(",\"diagnostics\":null").as_bytes())
            .expect("the API's null diagnostics");
        for extension in [",\"input_transformations\":[]", ",\"future_field\":1"] {
            assert_eq!(
                projector()
                    .push(message_start_stream(extension).as_bytes())
                    .expect_err("other Messages routes stay strict")
                    .kind(),
                MessagesStreamErrorKind::InvalidResponse,
                "{extension}"
            );
        }
    }

    #[test]
    fn message_start_admits_cache_miss_diagnostics_without_changing_the_turn() {
        let known = [
            (
                "model_changed",
                plakous_model::PromptCacheMissReason::ModelChanged,
            ),
            (
                "system_changed",
                plakous_model::PromptCacheMissReason::SystemChanged,
            ),
            (
                "tools_changed",
                plakous_model::PromptCacheMissReason::ToolsChanged,
            ),
            (
                "messages_changed",
                plakous_model::PromptCacheMissReason::MessagesChanged,
            ),
            (
                "previous_message_not_found",
                plakous_model::PromptCacheMissReason::PreviousMessageNotFound,
            ),
            (
                "unavailable",
                plakous_model::PromptCacheMissReason::Unavailable,
            ),
            (
                "private-sentinel",
                plakous_model::PromptCacheMissReason::Other,
            ),
            ("", plakous_model::PromptCacheMissReason::Other),
        ];
        for (kind, reason) in known {
            let extra = format!(
                r#","diagnostics":{{"ignored":true,"cache_miss_reason":{{"type":"{kind}","cache_missed_input_tokens":587,"private-sentinel":true}}}}"#
            );
            for mut stream in [projector(), claude_oauth_projector()] {
                let events = stream
                    .push(message_start_stream(&extra).as_bytes())
                    .expect("known cache-miss diagnostics");
                let diagnosis = events.iter().find_map(|event| match event {
                    ModelEvent::PromptCacheDiagnosis(diagnosis) => Some(*diagnosis),
                    _ => None,
                });
                let diagnosis = diagnosis.expect("diagnosis event");
                assert_eq!(diagnosis.reason, reason);
                assert_eq!(diagnosis.missed_input_tokens, Some(587));
                let rendered = format!("{diagnosis:?}");
                assert!(!rendered.contains("private-sentinel"));
                assert!(!rendered.contains("msg_"));
                stream.finish_eof().expect("EOF");
                let (response, _, _) = stream
                    .into_terminal_projection()
                    .expect("terminal")
                    .into_parts();
                assert_eq!(response.usage(), baseline_usage());
            }
        }
        for extra in [
            r#","diagnostics":null"#,
            r#","diagnostics":{"cache_miss_reason":null}"#,
            r#","diagnostics":{}"#,
            r#","diagnostics":{"advisory":1}"#,
        ] {
            for mut stream in [projector(), claude_oauth_projector()] {
                let events = stream
                    .push(message_start_stream(extra).as_bytes())
                    .expect("diagnostics with nothing to record");
                assert!(
                    !events
                        .iter()
                        .any(|event| matches!(event, ModelEvent::PromptCacheDiagnosis(_)))
                );
                stream.finish_eof().expect("EOF");
                let (response, _, _) = stream
                    .into_terminal_projection()
                    .expect("terminal")
                    .into_parts();
                assert_eq!(response.usage(), baseline_usage());
            }
        }
        let zero = r#","diagnostics":{"cache_miss_reason":{"type":"messages_changed","cache_missed_input_tokens":0}}"#;
        let events = projector()
            .push(message_start_stream(zero).as_bytes())
            .expect("zero missed tokens");
        let diagnosis = events.iter().find_map(|event| match event {
            ModelEvent::PromptCacheDiagnosis(diagnosis) => Some(*diagnosis),
            _ => None,
        });
        assert_eq!(
            diagnosis.expect("zero diagnosis").missed_input_tokens,
            Some(0)
        );
    }

    #[test]
    fn missing_cache_miss_count_is_admitted_and_u64_max_is_kept() {
        let omitted = r#","diagnostics":{"cache_miss_reason":{"type":"system_changed","ignored":"private-sentinel"}}"#;
        for mut stream in [projector(), claude_oauth_projector()] {
            let events = stream
                .push(message_start_stream(omitted).as_bytes())
                .expect("missing count is admitted");
            let diagnosis = events.iter().find_map(|event| match event {
                ModelEvent::PromptCacheDiagnosis(diagnosis) => Some(*diagnosis),
                _ => None,
            });
            let diagnosis = diagnosis.expect("omitted count");
            assert_eq!(
                diagnosis.reason,
                plakous_model::PromptCacheMissReason::SystemChanged
            );
            assert_eq!(diagnosis.missed_input_tokens, None);
            let rendered = format!("{diagnosis:?}");
            assert!(!rendered.contains("private-sentinel"));
            assert!(!rendered.contains("msg_"));
            stream.finish_eof().expect("EOF");
            let (response, _, _) = stream
                .into_terminal_projection()
                .expect("terminal")
                .into_parts();
            assert_eq!(response.usage(), baseline_usage());
        }
        let max_tokens = format!(
            r#","diagnostics":{{"cache_miss_reason":{{"type":"unavailable","cache_missed_input_tokens":{}}}}}"#,
            u64::MAX
        );
        let events = projector()
            .push(message_start_stream(&max_tokens).as_bytes())
            .expect("u64 max is a non-negative integer");
        let diagnosis = events.iter().find_map(|event| match event {
            ModelEvent::PromptCacheDiagnosis(diagnosis) => Some(*diagnosis),
            _ => None,
        });
        assert_eq!(
            diagnosis.expect("max diagnosis").missed_input_tokens,
            Some(u64::MAX)
        );
    }

    #[test]
    fn malformed_cache_miss_diagnostics_record_nothing_and_the_turn_proceeds() {
        let cases = [
            r#","diagnostics":"private-sentinel""#,
            r#","diagnostics":["private-sentinel"]"#,
            r#","diagnostics":{"cache_miss_reason":"private-sentinel"}"#,
            r#","diagnostics":{"cache_miss_reason":{"cache_missed_input_tokens":1}}"#,
            r#","diagnostics":{"cache_miss_reason":{"type":null,"cache_missed_input_tokens":1}}"#,
            r#","diagnostics":{"cache_miss_reason":{"type":1,"cache_missed_input_tokens":1}}"#,
            r#","diagnostics":{"cache_miss_reason":{"type":true,"cache_missed_input_tokens":1}}"#,
            r#","diagnostics":{"cache_miss_reason":{"type":"messages_changed","cache_missed_input_tokens":-1}}"#,
            r#","diagnostics":{"cache_miss_reason":{"type":"messages_changed","cache_missed_input_tokens":1.5}}"#,
            r#","diagnostics":{"cache_miss_reason":{"type":"messages_changed","cache_missed_input_tokens":"587"}}"#,
            r#","diagnostics":{"cache_miss_reason":{"type":"messages_changed","cache_missed_input_tokens":18446744073709551616}}"#,
            r#","diagnostics":{"cache_miss_reason":{"type":"messages_changed","cache_missed_input_tokens":null,"private-sentinel":true}}"#,
        ];
        for extra in cases {
            for mut stream in [projector(), claude_oauth_projector()] {
                let events = stream
                    .push(message_start_stream(extra).as_bytes())
                    .unwrap_or_else(|error| panic!("turn must proceed for {extra}: {error}"));
                assert!(
                    !events
                        .iter()
                        .any(|event| matches!(event, ModelEvent::PromptCacheDiagnosis(_))),
                    "{extra}"
                );
                assert!(
                    !format!("{events:?}").contains("private-sentinel"),
                    "{extra}"
                );
                stream.finish_eof().expect("EOF");
                let (response, _, _) = stream
                    .into_terminal_projection()
                    .expect("terminal")
                    .into_parts();
                assert_eq!(response.usage(), baseline_usage());
            }
        }
    }

    #[test]
    fn unknown_message_start_fields_still_fail_closed() {
        let cases = [
            r#","future_field":1"#,
            r#","diagnostics":{"cache_miss_reason":{"type":"messages_changed","cache_missed_input_tokens":1,"private-sentinel":true}},"future_field":1"#,
            r#","diagnostics":"private-sentinel","future_field":1"#,
        ];
        for extra in cases {
            for mut stream in [projector(), claude_oauth_projector()] {
                let error = stream
                    .push(message_start_stream(extra).as_bytes())
                    .expect_err("unknown message_start field must fail closed");
                assert_eq!(error.kind(), MessagesStreamErrorKind::InvalidResponse);
                assert!(
                    error.message().contains("invalid"),
                    "missing invalid in {}",
                    error.message()
                );
                assert!(!error.message().contains("private-sentinel"));
                assert!(!error.message().contains("msg_"));
            }
        }
    }

    #[test]
    fn openrouter_rejects_message_diagnostics() {
        let extra = r#","diagnostics":{"cache_miss_reason":{"type":"messages_changed","cache_missed_input_tokens":1,"private-sentinel":true}}"#;
        let error = openrouter_projector()
            .push(message_start_stream(extra).as_bytes())
            .expect_err("OpenRouter rejects diagnostics");
        assert_eq!(error.kind(), MessagesStreamErrorKind::InvalidResponse);
        assert!(!error.message().contains("private-sentinel"));
        assert!(!error.message().contains("msg_"));
    }

    fn baseline_usage() -> Option<plakous_model::Usage> {
        let mut stream = projector();
        stream
            .push(message_start_stream("").as_bytes())
            .expect("baseline");
        stream.finish_eof().expect("baseline EOF");
        let (response, _, _) = stream
            .into_terminal_projection()
            .expect("baseline terminal")
            .into_parts();
        response.usage()
    }

    #[test]
    fn claude_opus_observed_thinking_stream_projects_to_terminal() {
        // Synthetic values preserve the 2.1.280 Opus message and event shapes.
        // No native prompt, message identity, signature, or session data enters
        // the fixture.
        let stream = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_synthetic\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-opus-5-5\",\"content\":[],\"container\":null,\"context_management\":null,\"diagnostics\":null,\"input_transformations\":[],\"stop_details\":null,\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0,\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"\",\"estimated_tokens\":null}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig_synthetic\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null,\"stop_details\":null,\"container\":null},\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0,\"iterations\":[]},\"context_management\":{\"applied_edits\":[]}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let mut oauth = claude_oauth_projector();
        let events = oauth.push(stream.as_bytes()).expect("observed OAuth shape");
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ModelEvent::ResponseCompleted(_)))
        );
        oauth.finish_eof().expect("complete OAuth EOF");
        assert!(oauth.into_terminal_projection().is_ok());
    }

    #[test]
    fn parser_preserves_absent_and_explicit_direct_native_forms() {
        let dialect = MessagesStreamDialectHooks::new(
            "code_execution_20260120",
            NamespaceId::new("anthropic.programmatic_tool").expect("namespace"),
        );
        assert_eq!(
            parse_tool_caller(None, &dialect).expect("absent direct"),
            ToolCaller::AbsentDirect
        );
        let explicit = serde_json::value::RawValue::from_string(r#"{"type":"direct"}"#.to_owned())
            .expect("caller");
        assert_eq!(
            parse_tool_caller(Some(explicit.as_ref()), &dialect).expect("explicit direct"),
            ToolCaller::ExplicitDirect
        );
    }

    #[test]
    fn client_tools_only_hook_has_no_hosted_program_identity() {
        let dialect = MessagesStreamDialectHooks::client_tools_only();
        assert_eq!(
            parse_tool_caller(None, &dialect).expect("absent direct"),
            ToolCaller::AbsentDirect
        );
        let explicit = serde_json::value::RawValue::from_string(r#"{"type":"direct"}"#.to_owned())
            .expect("caller");
        assert_eq!(
            parse_tool_caller(Some(explicit.as_ref()), &dialect).expect("explicit direct"),
            ToolCaller::ExplicitDirect
        );
        let hosted = serde_json::value::RawValue::from_string(
            r#"{"type":"code_execution_20260120","tool_id":"srv_1"}"#.to_owned(),
        )
        .expect("caller");
        let error = parse_tool_caller(Some(hosted.as_ref()), &dialect)
            .expect_err("client-only hook must reject hosted callers");
        assert_eq!(error.kind(), MessagesStreamErrorKind::UnsupportedSemantic);
    }

    #[test]
    fn openrouter_v1_projects_real_shaped_lifecycle_and_derives_request_id() {
        let mut projector = openrouter_projector();
        let events = projector
            .push(OPENROUTER_STREAM.as_bytes())
            .expect("reviewed OpenRouter stream");
        projector.finish_eof().expect("complete OpenRouter EOF");
        assert_eq!(
            projector
                .provider_request_id()
                .map(plakous_model::ModelProviderRequestId::as_str),
            Some("gen-sanitized-request-01")
        );
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::ResponseStarted(started)
                if started.provider_request_id() == Some("gen-sanitized-request-01")
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::ResponseCompleted(response)
                if response.provider_request_id() == Some("gen-sanitized-request-01")
                    && response.finish().reason() == FinishReason::Length
        )));
        projector
            .into_terminal_projection()
            .expect("terminal projection remains message_stop-authoritative");
    }

    #[test]
    fn messages_dialects_project_thinking_usage_canonically() {
        let openrouter_stream =
            OPENROUTER_STREAM.replacen("\"thinking_tokens\":0", "\"thinking_tokens\":1", 1);
        let mut openrouter = openrouter_projector();
        let openrouter_events = openrouter
            .push(openrouter_stream.as_bytes())
            .expect("OpenRouter thinking usage");
        assert!(openrouter_events.iter().any(|event| matches!(
            event,
            ModelEvent::Usage(update) if update.usage().reasoning_tokens() == 1
        )));

        let claude_stream = CLAUDE_OAUTH_CONTEXT_MANAGEMENT_STREAM.replacen(
            "\"output_tokens\":1}",
            "\"output_tokens\":1,\"output_tokens_details\":{\"thinking_tokens\":1}}",
            1,
        );
        let mut claude = claude_oauth_projector();
        let claude_events = claude
            .push(claude_stream.as_bytes())
            .expect("Claude OAuth thinking usage");
        assert!(claude_events.iter().any(|event| matches!(
            event,
            ModelEvent::Usage(update) if update.usage().reasoning_tokens() == 1
        )));
    }

    #[test]
    fn openrouter_v1_projects_interleaved_blocks_in_provider_index_order() {
        let mut openrouter = openrouter_projector();
        openrouter
            .push(OPENROUTER_INTERLEAVED_TOOL_STREAM.as_bytes())
            .expect("reviewed interleaved OpenRouter stream");
        openrouter.finish_eof().expect("complete OpenRouter EOF");
        let (response, _, _) = openrouter
            .into_terminal_projection()
            .expect("terminal projection")
            .into_parts();
        assert!(matches!(
            response.output()[0].blocks()[0],
            ContentBlock::Text(_)
        ));
        assert!(matches!(
            response.output()[0].blocks()[1],
            ContentBlock::ToolCall(_)
        ));
        assert!(matches!(
            response.output()[0].blocks()[2],
            ContentBlock::Text(_)
        ));

        let error = projector()
            .push(OPENROUTER_INTERLEAVED_TOOL_STREAM.as_bytes())
            .expect_err("strict Messages must retain its single-open-block grammar");
        assert_eq!(error.kind(), MessagesStreamErrorKind::InvalidResponse);
    }

    #[test]
    fn openrouter_empty_unsigned_thinking_preserves_following_block_indexes() {
        let stream = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_empty_thinking\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-5\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"ok\",\"citations\":[]}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":1}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let mut projector = openrouter_projector();
        projector.push(stream.as_bytes()).expect("valid stream");
        projector.finish_eof().expect("terminal EOF");
        let (response, _, _) = projector
            .into_terminal_projection()
            .expect("terminal projection")
            .into_parts();
        let blocks = response.output()[0].blocks();
        assert!(matches!(
            &blocks[0],
            ContentBlock::Reasoning(block)
                if block.visibility() == plakous_model::ReasoningVisibility::Omitted
        ));
        assert!(matches!(&blocks[1], ContentBlock::Text(block) if block.text() == "ok"));
    }

    #[test]
    fn strict_messages_rejects_openrouter_additions() {
        let error = projector()
            .push(OPENROUTER_STREAM.as_bytes())
            .expect_err("strict grammar must remain unchanged");
        assert_eq!(error.kind(), MessagesStreamErrorKind::InvalidResponse);
    }

    #[test]
    fn openrouter_v1_accepts_optional_exact_done_and_rejects_unreviewed_additions() {
        let without_done = OPENROUTER_STREAM
            .split("event: data\ndata: [DONE]")
            .next()
            .expect("prefix");
        let mut missing = openrouter_projector();
        missing
            .push(without_done.as_bytes())
            .expect("message_stop remains terminal authority");
        missing
            .finish_eof()
            .expect("message_stop is sufficient at EOF");
        missing
            .into_terminal_projection()
            .expect("message_stop remains terminal authority");

        let mut non_null_context = openrouter_projector();
        let stream = OPENROUTER_STREAM.replacen(
            "\"context_management\":null",
            "\"context_management\":{}",
            1,
        );
        assert_eq!(
            non_null_context
                .push(stream.as_bytes())
                .expect_err("unreviewed context semantic")
                .kind(),
            MessagesStreamErrorKind::UnsupportedSemantic
        );

        let mut unknown_usage = openrouter_projector();
        let stream = OPENROUTER_STREAM.replacen("\"cost\":0", "\"future_cost\":0", 1);
        unknown_usage
            .push(stream.as_bytes())
            .expect("an additive usage key is ignored");

        let mut unknown_stop = openrouter_projector();
        let stream = OPENROUTER_STREAM.replacen("\"max_tokens\"", "\"future_stop\"", 1);
        assert_eq!(
            unknown_stop
                .push(stream.as_bytes())
                .expect_err("unknown terminal enum")
                .kind(),
            MessagesStreamErrorKind::UnsupportedSemantic
        );

        let mut decreasing_usage = openrouter_projector();
        let stream = OPENROUTER_STREAM.replacen("\"input_tokens\":0", "\"input_tokens\":20", 1);
        assert_eq!(
            decreasing_usage
                .push(stream.as_bytes())
                .expect_err("decreasing cumulative usage")
                .kind(),
            MessagesStreamErrorKind::InvalidAccounting
        );
    }

    fn push(sse: &str) -> Vec<ModelEvent> {
        let mut projector = projector();
        projector.push(sse.as_bytes()).expect("stream projects")
    }

    fn pause_stream(container: bool, client_tool: bool) -> String {
        let container = if container {
            "\"container\":{\"id\":\"container_1\",\"expires_at\":\"2026-08-23T12:00:00Z\"},"
        } else {
            ""
        };
        let client = if client_tool {
            concat!(
                "event: content_block_start\n",
                "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tool_1\",\"name\":\"weather\",\"input\":{}}}\n\n",
                "event: content_block_stop\n",
                "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            )
        } else {
            ""
        };
        format!(
            concat!(
                "event: message_start\n",
                "data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_pause\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-5\",\"content\":[],\"stop_reason\":null,\"usage\":{{\"input_tokens\":1,\"output_tokens\":0}}}}}}\n\n",
                "event: content_block_start\n",
                "data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"server_tool_use\",\"id\":\"srv_1\",\"name\":\"code_execution\",\"input\":{{}}}}}}\n\n",
                "event: content_block_stop\n",
                "data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n",
                "{}",
                "event: message_delta\n",
                "data: {{\"type\":\"message_delta\",\"delta\":{{{}\"stop_reason\":\"pause_turn\",\"stop_sequence\":null}},\"usage\":{{\"output_tokens\":1}}}}\n\n",
                "event: message_stop\n",
                "data: {{\"type\":\"message_stop\"}}\n\n",
            ),
            client, container
        )
    }

    #[test]
    fn hosted_pause_maps_to_provider_continuation() {
        let mut projector = projector_with_hosted(true);
        let events = projector
            .push(pause_stream(true, false).as_bytes())
            .expect("admitted hosted pause");
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::ResponseCompleted(response)
                if response.finish().reason() == FinishReason::ProviderContinuation
        )));
    }

    #[test]
    fn hosted_pause_rejects_missing_container_and_client_tool_contradictions() {
        let mut missing_container = projector_with_hosted(true);
        let error = missing_container
            .push(pause_stream(false, false).as_bytes())
            .expect_err("pause without container");
        assert!(error.to_string().contains("pause_turn"));

        let mut client_tool = projector_with_hosted(true);
        let error = client_tool
            .push(pause_stream(true, true).as_bytes())
            .expect_err("pause with client tool");
        assert!(
            error
                .to_string()
                .contains("stop_reason=pause_turn client_proposals=1 provider_proposals=1")
        );
    }

    #[test]
    fn hosted_container_binds_on_message_delta_only_and_rejects_drift() {
        let mut wrong_start = projector_with_hosted(true);
        let start_container = pause_stream(true, false).replacen(
            "\"content\":[]",
            "\"content\":[],\"container\":{\"id\":\"wrong\",\"expires_at\":\"2026-08-23T12:00:00Z\"}",
            1,
        );
        assert!(wrong_start.push(start_container.as_bytes()).is_err());

        let mut wrong_active = projector_with_hosted(true);
        wrong_active.prepared.expected_container = Some("container_prior".to_owned());
        let error = wrong_active
            .push(pause_stream(true, false).as_bytes())
            .expect_err("active container drift");
        assert!(error.to_string().contains("active artifact lineage"));

        let mut conflicting = projector_with_hosted(true);
        let stream = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_conflict\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-5\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"container\":{\"id\":\"container_1\",\"expires_at\":\"2026-08-23T12:00:00Z\"}},\"usage\":{\"output_tokens\":1}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"container\":{\"id\":\"container_2\",\"expires_at\":\"2026-08-23T12:00:00Z\"},\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
        );
        let error = conflicting
            .push(stream.as_bytes())
            .expect_err("conflicting repeated container");
        assert!(error.to_string().contains("conflicting container"));
    }

    #[test]
    fn empty_placeholder_signature_is_unbound_until_signature_delta() {
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_think\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-5\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig_real\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":1}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let events = push(sse);
        assert!(
            events.iter().any(|event| matches!(
                event,
                ModelEvent::ReasoningReplayBound(bound)
                    if bound.replay().payload().get().contains("sig_real")
            )),
            "empty start signature must not hide the later signature_delta: {events:?}"
        );
    }

    #[test]
    fn empty_thinking_delta_and_repeat_signature_are_noops() {
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_think\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-5\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"sig_one\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig_\\u006fne\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":1}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let events = push(sse);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ModelEvent::BlockStarted(_))),
            "empty thinking must still open a reasoning block"
        );
    }

    #[test]
    fn conflicting_repeat_signature_fails_closed() {
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_think\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-5\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"sig_one\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig_two\"}}\n\n",
        );
        let error = projector()
            .push(sse.as_bytes())
            .expect_err("conflicting signature must fail");
        assert_eq!(error.kind(), MessagesStreamErrorKind::InvalidResponse);
    }

    #[test]
    fn unadvertised_tool_use_projects_as_local_requested() {
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_hidden\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-5\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_hidden\",\"name\":\"Write\",\"input\":{}}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":1}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let events = push(sse);
        assert!(
            events.iter().any(|event| matches!(
                event,
                ModelEvent::BlockStarted(started)
                    if matches!(
                        started.block(),
                        plakous_model::BlockStart::ToolCall { tool, owner, state, .. }
                            if tool == &ToolName::new("Write").expect("name")
                                && *owner == plakous_model::ToolExecutionOwner::PlakLocal
                                && *state == plakous_model::ToolCallState::Requested
                    )
            )),
            "hidden Write must materialize as a local requested tool call"
        );
    }
}
