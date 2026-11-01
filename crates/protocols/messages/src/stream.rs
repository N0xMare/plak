//! Messages-family incremental stream state machine and canonical projection.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use plakous_http::SseError;
pub use plakous_http::{SseDecoder, SseLimits, SseMessage};
use plakous_model::{
    CallId, Clock, ContentBlock, Digest, EntryId, IdGenerator, ModelEvent, ModelId,
    ModelProviderRequestId, ModelResponse, NamespaceId, ProviderEvent, ReasoningVisibility,
    ToolCallState, ToolCorrelation, ToolExecutionOwner, ToolName, Usage, ValidatedRequestPlan,
};
use serde::Deserialize;
use serde_json::value::RawValue;

use crate::request::{MessagesRequestPolicy, ToolOwnership, bind_replay_payload};
use crate::{
    MessagesEventKind, MessagesProviderUsage as ProviderUsage, MessagesStreamLimits,
    MessagesUsageUpdate, validate_named_event,
};

mod blocks;
mod citations;
mod message;
mod parse;

use parse::{
    ToolCaller, map_finish, nonempty_opaque_signature, parse_event, parse_message_delta,
    parse_message_start, parse_stop_details, parse_thinking_delta, parse_tool_caller,
    raw_block_start_has_hosted_effect, validate_opaque_string, validate_stop_blocks,
};

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

    /// An output-limit stop may cut a client `tool_use` block anywhere in its
    /// input, so its client proposals are revoked rather than parsed.
    const fn is_output_limit(self) -> bool {
        matches!(self, Self::MaxTokens | Self::ModelContextWindowExceeded)
    }

    /// Whether closed client tool input is revoked unparsed at this terminal.
    const fn revokes_client_input(self) -> bool {
        matches!(self, Self::Refusal) || self.is_output_limit()
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
        citations: Vec<Box<RawValue>>,
        native: Box<RawValue>,
    },
    Skipped {
        bytes: usize,
    },
    Native {
        block: Box<RawValue>,
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

struct CitedText {
    target: u32,
    native: Box<RawValue>,
    citations: Vec<Box<RawValue>>,
}

impl OpenBlock {
    fn retained_bytes(&self) -> usize {
        match self {
            Self::Text {
                text,
                citations,
                native,
            } => text
                .len()
                .saturating_add(native.get().len())
                .saturating_add(citations.iter().map(|v| v.get().len()).sum::<usize>()),
            Self::Refusal { text } => text.len(),
            Self::Skipped { bytes } => *bytes,
            Self::Native { block } => block.get().len(),
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
    next_output_index: u32,
    output_indices: BTreeMap<u32, u32>,
    cited_text: Vec<CitedText>,
    skipped_blocks: u32,
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
            next_output_index: 0,
            output_indices: BTreeMap::new(),
            cited_text: Vec::new(),
            skipped_blocks: 0,
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
}

impl MessagesStreamProjector {
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

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "projector fixtures fail fast on identity construction"
)]
mod tests;
