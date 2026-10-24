//! Bounded one-choice Chat SSE lifecycle.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::Arc;

use plakous_http::{SseDecoder, SseLimits};
use plakous_model::{
    Actor, BlockCompleted, BlockStart, BlockStarted, CallId, Clock, ContentBlock, ConversationItem,
    DataClassification, Digest, EntryId, FinishDetails, FinishReason, IdGenerator, ModelEvent,
    ModelId, ModelProviderRequestId, ModelResponse, ModelResponseMetadata, OutputLocation,
    ProtocolId, Provenance, ReasoningBlock, ReasoningDelta, ReasoningPolicy, ReasoningVisibility,
    ReplayEnvelope, ReplayKindId, ReplayScope, SafeDiagnostic, SemanticJsonV1, ServedBy, TextBlock,
    TextDelta, ToolCallBlock, ToolCallState, ToolCallerLineage, ToolCorrelation,
    ToolExecutionOwner, ToolInputDelta, ToolName, Usage, UsageUpdate,
};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::request::{
    CHAT_REPLAY_SCHEMA_REVISION, ChatReasoningDetailReplayV1, ChatReasoningReplay,
    ChatReplayTargetV1,
};

mod in_band;
mod wire;

use in_band::{in_band_error_status, in_band_error_summary};
use wire::{capture_served_provider, parse_finish};

const REASONING_DETAIL_REPLAY_KIND: &str = "chat.reasoning_detail";
const MAX_TOOL_CALL_SLOT_INDEX: u32 = 4096;

/// Closed wire spelling policy for visible reasoning deltas.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningAliasPolicy {
    /// Accept only `delta.reasoning`.
    Reasoning,
    /// Accept only `delta.reasoning_content`.
    ReasoningContent,
    /// Bind the first nonempty spelling and require it for the whole response.
    EitherBindFirst,
}

/// Closed set of admitted dialect additions to the neutral Chat stream.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ChatStreamExtensions {
    /// Exact provider-neutral Chat V1 grammar only.
    #[default]
    None,
    /// OpenRouter gateway metadata and repeated assistant-role spelling.
    OpenRouterV1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BoundReasoningAlias {
    Reasoning,
    ReasoningContent,
}

/// Closed stream failure class.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChatStreamErrorKind {
    /// Malformed framing, JSON, ordering, identity, or correlation.
    InvalidResponse,
    /// A known excluded semantic was observed.
    UnsupportedSemantic,
    /// An explicit resource bound was exceeded.
    ResourceLimit,
    /// An in-band provider error object was observed.
    ProviderError,
    /// EOF or `[DONE]` occurred before the complete terminal grammar.
    Incomplete,
}

/// Redaction-safe terminal stream error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatStreamError {
    kind: ChatStreamErrorKind,
    message: String,
    http_status: Option<u16>,
    usage_limit: bool,
    in_band_detail: Option<String>,
}
impl ChatStreamError {
    /// Returns the closed class.
    pub const fn kind(&self) -> ChatStreamErrorKind {
        self.kind
    }
    /// Returns the bounded, credential-redacted diagnostic.
    pub fn message(&self) -> &str {
        &self.message
    }
    /// Whether the in-band error names exhausted quota or entitlement.
    #[must_use]
    pub const fn usage_limit(&self) -> bool {
        self.usage_limit
    }

    /// Returns the HTTP-shaped status an in-band provider error named.
    ///
    /// A gateway that fronts other providers reports an upstream failure inside
    /// the stream it already committed to, carrying the status it would have
    /// sent had it failed earlier. Keeping it typed is what lets a caller
    /// classify the failure without parsing the diagnostic.
    pub const fn http_status(&self) -> Option<u16> {
        self.http_status
    }

    /// Returns the provider's own account of an in-band error, without the
    /// framing of the sentence it sits in.
    ///
    /// `HTTP 504 (timeout) from <upstream>: <message>` when the provider named
    /// a status, and its message (and the upstream it named) otherwise; bounded
    /// and credential-redacted like the diagnostic. Present exactly for
    /// [`ChatStreamErrorKind::ProviderError`], so a caller can say who failed
    /// without calling a well-formed provider error a protocol violation.
    pub fn in_band_detail(&self) -> Option<&str> {
        self.in_band_detail.as_deref()
    }
}
impl fmt::Display for ChatStreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for ChatStreamError {}
fn err(kind: ChatStreamErrorKind, message: impl Into<String>) -> ChatStreamError {
    ChatStreamError {
        kind,
        message: message.into(),
        http_status: None,
        usage_limit: false,
        in_band_detail: None,
    }
}

/// Exact wire finish spelling plus canonical meaning.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatFinishReason {
    wire: String,
    canonical: FinishReason,
}
impl ChatFinishReason {
    /// Returns the wire value.
    pub fn wire(&self) -> &str {
        &self.wire
    }
    /// Returns its canonical meaning.
    pub const fn canonical(&self) -> FinishReason {
        self.canonical
    }
}

/// Explicit nonzero limits for one stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChatStreamLimits {
    line: NonZeroUsize,
    event: NonZeroUsize,
    events: NonZeroUsize,
    output: NonZeroUsize,
    calls: NonZeroUsize,
    arguments: NonZeroUsize,
    reasoning_details: NonZeroUsize,
    reasoning_detail_bytes: NonZeroUsize,
    total_stream_bytes: NonZeroUsize,
}
impl ChatStreamLimits {
    /// Creates one exact limit set. `total_stream_bytes` bounds every wire byte,
    /// SSE comments included, so it also bounds the aggregate retained output and
    /// tool arguments across all calls.
    #[allow(
        clippy::too_many_arguments,
        reason = "each bound is an independent exact limit"
    )]
    pub const fn new(
        line: NonZeroUsize,
        event: NonZeroUsize,
        events: NonZeroUsize,
        output: NonZeroUsize,
        calls: NonZeroUsize,
        arguments: NonZeroUsize,
        total_stream_bytes: NonZeroUsize,
    ) -> Self {
        Self {
            line,
            event,
            events,
            output,
            calls,
            arguments,
            total_stream_bytes,
            reasoning_details: match NonZeroUsize::new(128) {
                Some(value) => value,
                None => NonZeroUsize::MIN,
            },
            reasoning_detail_bytes: match NonZeroUsize::new(4 * 1024 * 1024) {
                Some(value) => value,
                None => NonZeroUsize::MIN,
            },
        }
    }

    /// Replaces the independent retained reasoning-detail count and byte bounds.
    #[must_use]
    pub const fn with_reasoning_detail_limits(
        mut self,
        count: NonZeroUsize,
        bytes: NonZeroUsize,
    ) -> Self {
        self.reasoning_details = count;
        self.reasoning_detail_bytes = bytes;
        self
    }
}
impl Default for ChatStreamLimits {
    fn default() -> Self {
        Self::new(
            NonZeroUsize::new(1024 * 1024).unwrap_or(NonZeroUsize::MIN),
            NonZeroUsize::new(4 * 1024 * 1024).unwrap_or(NonZeroUsize::MIN),
            NonZeroUsize::new(100_000).unwrap_or(NonZeroUsize::MIN),
            NonZeroUsize::new(16 * 1024 * 1024).unwrap_or(NonZeroUsize::MIN),
            NonZeroUsize::new(128).unwrap_or(NonZeroUsize::MIN),
            NonZeroUsize::new(16 * 1024 * 1024).unwrap_or(NonZeroUsize::MIN),
            NonZeroUsize::new(64 * 1024 * 1024).unwrap_or(NonZeroUsize::MIN),
        )
    }
}

/// Complete authority for one stream interpretation.
#[derive(Clone)]
pub struct PreparedChatStreamBinding {
    plan: Digest,
    protocol: ProtocolId,
    expected_model: ModelId,
    replay_target: ChatReplayTargetV1,
    reasoning_policy: ReasoningPolicy,
    reasoning_replay: ChatReasoningReplay,
    reasoning_alias_policy: ReasoningAliasPolicy,
    extensions: ChatStreamExtensions,
    limits: ChatStreamLimits,
}
impl PreparedChatStreamBinding {
    /// Creates an exact credential-free binding.
    pub fn new(
        plan: Digest,
        protocol: ProtocolId,
        expected_model: ModelId,
        replay_target: ChatReplayTargetV1,
        reasoning_policy: ReasoningPolicy,
        reasoning_replay: ChatReasoningReplay,
        reasoning_alias_policy: ReasoningAliasPolicy,
        limits: ChatStreamLimits,
    ) -> Self {
        Self {
            plan,
            protocol,
            expected_model,
            replay_target,
            reasoning_policy,
            reasoning_replay,
            reasoning_alias_policy,
            extensions: ChatStreamExtensions::None,
            limits,
        }
    }
    /// Returns the plan identity.
    pub const fn plan(&self) -> Digest {
        self.plan
    }
    /// Returns the protocol identity used for replay binding.
    pub const fn protocol(&self) -> &ProtocolId {
        &self.protocol
    }

    /// Admits one reviewed dialect extension profile.
    #[must_use]
    pub const fn with_extensions(mut self, extensions: ChatStreamExtensions) -> Self {
        self.extensions = extensions;
        self
    }
}
impl fmt::Debug for PreparedChatStreamBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreparedChatStreamBinding")
            .field("plan", &self.plan)
            .field("protocol", &self.protocol)
            .field("expected_model", &self.expected_model)
            .field("replay_target", &self.replay_target)
            .field("reasoning_policy", &self.reasoning_policy)
            .field("reasoning_replay", &self.reasoning_replay)
            .field("reasoning_alias_policy", &self.reasoning_alias_policy)
            .field("extensions", &self.extensions)
            .field("limits", &self.limits)
            .finish()
    }
}

/// Complete terminal projection; Chat V1 intentionally has no aggregate artifact.
#[derive(Clone, Debug, PartialEq)]
pub struct ChatTerminalProjection {
    response: ModelResponse,
    finish: ChatFinishReason,
}
impl ChatTerminalProjection {
    /// Returns the canonical response.
    pub const fn response(&self) -> &ModelResponse {
        &self.response
    }
    /// Returns exact finish facts.
    pub const fn finish(&self) -> &ChatFinishReason {
        &self.finish
    }
    /// Splits the terminal value.
    pub fn into_parts(self) -> (ModelResponse, ChatFinishReason) {
        (self.response, self.finish)
    }
}

#[derive(Default)]
struct ToolAssembly {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

/// Fragmentation-independent projector for data-only Chat SSE.
pub struct ChatStreamProjector {
    binding: PreparedChatStreamBinding,
    request_id: Option<ModelProviderRequestId>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGenerator>,
    sse: SseDecoder,
    wire_bytes: usize,
    event_count: usize,
    response_id: Option<String>,
    model: Option<ModelId>,
    created: Option<u64>,
    role_seen: bool,
    done: bool,
    failed: bool,
    text: String,
    reasoning: String,
    reasoning_alias: Option<BoundReasoningAlias>,
    reasoning_details: Vec<Box<RawValue>>,
    reasoning_detail_bytes: usize,
    slot_ordinals: BTreeMap<u32, usize>,
    tools: Vec<ToolAssembly>,
    remote_ids: BTreeSet<String>,
    finish: Option<ChatFinishReason>,
    usage: Option<Usage>,
    served_by: Option<ServedBy>,
    terminal: Option<ChatTerminalProjection>,
    item_id: Option<EntryId>,
    text_location: Option<OutputLocation>,
    reasoning_location: Option<OutputLocation>,
    next_block: u32,
}
impl ChatStreamProjector {
    /// Creates a fresh projector.
    pub fn new(
        binding: PreparedChatStreamBinding,
        request_id: Option<ModelProviderRequestId>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
    ) -> Self {
        let limits = binding.limits;
        Self {
            binding,
            request_id,
            clock,
            ids,
            sse: SseDecoder::new(SseLimits {
                max_line_bytes: limits.line.get(),
                max_event_bytes: limits.event.get(),
            }),
            wire_bytes: 0,
            event_count: 0,
            response_id: None,
            model: None,
            created: None,
            role_seen: false,
            done: false,
            failed: false,
            text: String::new(),
            reasoning: String::new(),
            reasoning_alias: None,
            reasoning_details: Vec::new(),
            reasoning_detail_bytes: 0,
            slot_ordinals: BTreeMap::new(),
            tools: Vec::new(),
            remote_ids: BTreeSet::new(),
            finish: None,
            usage: None,
            served_by: None,
            terminal: None,
            item_id: None,
            text_location: None,
            reasoning_location: None,
            next_block: 0,
        }
    }

    /// Feeds arbitrary transport fragments.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<ModelEvent>, ChatStreamError> {
        if self.failed {
            return Err(err(
                ChatStreamErrorKind::InvalidResponse,
                "Chat projector already failed",
            ));
        }
        let result = self.push_unpoisoned(bytes);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn push_unpoisoned(&mut self, bytes: &[u8]) -> Result<Vec<ModelEvent>, ChatStreamError> {
        if self.done && !bytes.is_empty() {
            return self.fail(
                ChatStreamErrorKind::InvalidResponse,
                "bytes arrived after [DONE]",
            );
        }
        self.wire_bytes = self.wire_bytes.saturating_add(bytes.len());
        if self.wire_bytes > self.binding.limits.total_stream_bytes.get() {
            return self.fail(
                ChatStreamErrorKind::ResourceLimit,
                "Chat stream exceeded its total byte bound",
            );
        }
        let frames = self.sse.push(bytes).map_err(|error| {
            let kind = match error {
                plakous_http::SseError::ResourceLimit(_) => ChatStreamErrorKind::ResourceLimit,
                _ => ChatStreamErrorKind::InvalidResponse,
            };
            err(kind, "invalid Chat SSE framing")
        })?;
        let mut output = Vec::new();
        for frame in frames {
            if frame.event().is_some() {
                return self.fail(
                    ChatStreamErrorKind::InvalidResponse,
                    "Chat V1 accepts data-only SSE",
                );
            }
            self.event_count = self.event_count.checked_add(1).ok_or_else(|| {
                err(
                    ChatStreamErrorKind::ResourceLimit,
                    "Chat event count overflow",
                )
            })?;
            if self.event_count > self.binding.limits.events.get() {
                return self.fail(
                    ChatStreamErrorKind::ResourceLimit,
                    "Chat event count exceeded its bound",
                );
            }
            if frame.data() == "[DONE]" {
                self.complete(&mut output)?;
            } else {
                self.chunk(frame.data(), &mut output)?;
            }
        }
        Ok(output)
    }

    /// Requires a completed `[DONE]` lifecycle at transport EOF.
    pub fn finish(&mut self) -> Result<ChatTerminalProjection, ChatStreamError> {
        if self.failed {
            return Err(err(
                ChatStreamErrorKind::InvalidResponse,
                "Chat projector already failed",
            ));
        }
        let result = self.finish_unpoisoned();
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn finish_unpoisoned(&mut self) -> Result<ChatTerminalProjection, ChatStreamError> {
        if !self.sse.is_idle() {
            return self.fail(
                ChatStreamErrorKind::Incomplete,
                "unterminated SSE event at EOF",
            );
        }
        if !self.done {
            return self.fail(
                ChatStreamErrorKind::Incomplete,
                "Chat stream ended without [DONE]",
            );
        }
        self.terminal.take().ok_or_else(|| {
            err(
                ChatStreamErrorKind::InvalidResponse,
                "Chat terminal aggregate was already taken or never formed",
            )
        })
    }

    #[allow(clippy::too_many_lines, reason = "closed Chat chunk grammar is linear")]
    fn chunk(&mut self, data: &str, output: &mut Vec<ModelEvent>) -> Result<(), ChatStreamError> {
        if self.done {
            return self.fail(
                ChatStreamErrorKind::InvalidResponse,
                "JSON arrived after [DONE]",
            );
        }
        let mut chunk: Chunk = serde_json::from_str(data).map_err(|error| {
            err(
                ChatStreamErrorKind::InvalidResponse,
                format!("Chat data is not exact JSON: {error}"),
            )
        })?;
        if !chunk.extensions_allowed(self.binding.extensions) {
            let detail = chunk
                .first_unreviewed_extension(self.binding.extensions)
                .map_or_else(
                    || "Chat chunk contains dialect fields outside the prepared grammar".to_owned(),
                    |path| format!("Chat chunk contains an unreviewed dialect field at {path}"),
                );
            return self.fail(ChatStreamErrorKind::InvalidResponse, detail);
        }
        if let Some(error) = chunk.error.as_ref() {
            let status = in_band_error_status(error);
            let usage_limit = plakous_attempt::value_reports_usage_limit(error);
            let detail = SafeDiagnostic::new(in_band_error_summary(
                error,
                status,
                usage_limit,
                chunk.extra.get("provider"),
            ))
            .to_string();
            self.failed = true;
            return Err(ChatStreamError {
                kind: ChatStreamErrorKind::ProviderError,
                message: format!(
                    "Chat provider returned an in-band error after accepting the request: {detail}"
                ),
                http_status: status,
                usage_limit,
                in_band_detail: Some(detail),
            });
        }
        if chunk.object.as_deref() != Some("chat.completion.chunk") {
            return self.fail(
                ChatStreamErrorKind::InvalidResponse,
                "Chat chunk object is absent or invalid",
            );
        }
        let id = chunk.id.ok_or_else(|| {
            err(
                ChatStreamErrorKind::InvalidResponse,
                "Chat chunk omitted response id",
            )
        })?;
        let created = chunk.created.ok_or_else(|| {
            err(
                ChatStreamErrorKind::InvalidResponse,
                "Chat chunk omitted creation time",
            )
        })?;
        let model = ModelId::new(chunk.model.ok_or_else(|| {
            err(
                ChatStreamErrorKind::InvalidResponse,
                "Chat chunk omitted model",
            )
        })?)
        .map_err(|_| {
            err(
                ChatStreamErrorKind::InvalidResponse,
                "Chat model is invalid",
            )
        })?;
        if model != self.binding.expected_model {
            return self.fail(
                ChatStreamErrorKind::InvalidResponse,
                "reported model differs from the exact target",
            );
        }
        if self.response_id.as_ref().is_some_and(|old| old != &id)
            || self.model.as_ref().is_some_and(|old| old != &model)
            || self.created.is_some_and(|old| old != created)
        {
            return self.fail(
                ChatStreamErrorKind::InvalidResponse,
                "Chat identity changed mid-stream",
            );
        }
        let first = self.response_id.is_none();
        if first && self.request_id.is_none() {
            self.request_id = Some(ModelProviderRequestId::new(&id).map_err(|_| {
                err(
                    ChatStreamErrorKind::InvalidResponse,
                    "Chat response ID is not a valid provider request identity",
                )
            })?);
        }
        let request_id = self.request_id.clone().ok_or_else(|| {
            err(
                ChatStreamErrorKind::InvalidResponse,
                "Chat provider request identity is unavailable",
            )
        })?;
        self.response_id.get_or_insert(id.clone());
        self.model.get_or_insert(model.clone());
        self.created.get_or_insert(created);
        capture_served_provider(&chunk.extra, self.binding.extensions, &mut self.served_by)?;
        if first {
            output.push(ModelEvent::ResponseStarted(
                plakous_model::ResponseStarted::new(Some(id), self.binding.plan)
                    .with_metadata(ModelResponseMetadata::new(Some(model), Some(request_id))),
            ));
        }
        if let Some(usage) = chunk.usage {
            let repeated_finish = match chunk.choices.as_slice() {
                [] => true,
                [choice]
                    if self.binding.extensions == ChatStreamExtensions::OpenRouterV1
                        && choice.index == 0
                        && !choice.delta.semantic()
                        && choice
                            .finish_reason
                            .as_deref()
                            .zip(self.finish.as_ref().map(ChatFinishReason::wire))
                            .is_some_and(|(current, prior)| current == prior) =>
                {
                    true
                }
                _ => false,
            };
            if !repeated_finish || self.finish.is_none() || self.usage.is_some() {
                return self.fail(
                    ChatStreamErrorKind::InvalidResponse,
                    "usage must be one post-finish terminal-metadata chunk",
                );
            }
            let usage = usage.canonical(self.binding.extensions)?;
            self.usage = Some(usage);
            output.push(ModelEvent::Usage(UsageUpdate::new(usage)));
            return Ok(());
        }
        let [choice] = chunk.choices.as_mut_slice() else {
            return self.fail(
                ChatStreamErrorKind::InvalidResponse,
                "Chat chunk must contain exactly one choice",
            );
        };
        if choice.index != 0 || self.finish.is_some() {
            return self.fail(
                ChatStreamErrorKind::InvalidResponse,
                "choice index or lifecycle is invalid",
            );
        }
        if let Some(role) = &choice.delta.role {
            if role != "assistant"
                || (self.role_seen && self.binding.extensions != ChatStreamExtensions::OpenRouterV1)
            {
                return self.fail(
                    ChatStreamErrorKind::InvalidResponse,
                    "assistant role must bind exactly once",
                );
            }
            self.role_seen = true;
        }
        if let Some(value) = &choice.delta.content {
            if value.is_empty() {
                if self.binding.extensions == ChatStreamExtensions::None {
                    return self.fail(ChatStreamErrorKind::InvalidResponse, "empty text delta");
                }
            } else {
                let location = self.open_text(output)?;
                self.text.push_str(value);
                output.push(ModelEvent::TextDelta(TextDelta::new(
                    location,
                    value.as_bytes(),
                )));
            }
        }
        if choice.delta.reasoning.is_some() && choice.delta.reasoning_content.is_some() {
            return self.fail(
                ChatStreamErrorKind::InvalidResponse,
                "reasoning aliases are mutually exclusive",
            );
        }
        let reasoning_delta = choice
            .delta
            .reasoning
            .as_ref()
            .map(|value| (BoundReasoningAlias::Reasoning, value))
            .or_else(|| {
                choice
                    .delta
                    .reasoning_content
                    .as_ref()
                    .map(|value| (BoundReasoningAlias::ReasoningContent, value))
            });
        if let Some((alias, value)) = reasoning_delta {
            self.bind_reasoning_alias(alias, value)?;
            let location = self.open_reasoning(output)?;
            self.reasoning.push_str(value);
            output.push(ModelEvent::ReasoningDelta(ReasoningDelta::new(
                location,
                value.as_bytes(),
            )));
        }
        if let Some(details) = choice.delta.reasoning_details.take() {
            if self.binding.reasoning_replay.includes_exact_details() {
                self.retain_reasoning_details(details)?;
            } else if self.binding.extensions == ChatStreamExtensions::None {
                return self.fail(
                    ChatStreamErrorKind::UnsupportedSemantic,
                    "reasoning details are excluded by the prepared replay policy",
                );
            }
        }
        for part in &choice.delta.tool_calls {
            self.tool_delta(part)?;
        }
        self.check_output_bound()?;
        if let Some(reason) = &choice.finish_reason {
            if choice.delta.semantic() {
                return self.fail(
                    ChatStreamErrorKind::InvalidResponse,
                    "finish reason shared a semantic delta",
                );
            }
            self.finish = Some(parse_finish(reason)?);
        }
        Ok(())
    }

    /// Applies one streamed tool-call delta.
    ///
    /// Wire `index` values are treated as opaque slot keys, not dense ordinals:
    /// some upstreams number slots by assistant content part (reasoning, text,
    /// then tool calls), so the first call may arrive as index 2 or higher.
    /// Each unseen slot key is mapped to the next dense local ordinal on first
    /// sight; downstream projection still emits tool calls in first-appearance
    /// order. A slot is admitted once both remote id and function name are
    /// bound; earlier deltas for that slot may carry arguments only.
    fn tool_delta(&mut self, delta: &ToolDelta) -> Result<(), ChatStreamError> {
        if delta.index >= MAX_TOOL_CALL_SLOT_INDEX {
            return self.fail(
                ChatStreamErrorKind::ResourceLimit,
                "tool-call index exceeded its bound",
            );
        }
        if delta.kind.as_deref().is_some_and(|kind| kind != "function") {
            return self.fail(
                ChatStreamErrorKind::UnsupportedSemantic,
                "only function calls are admitted",
            );
        }
        let ordinal = if let Some(&ordinal) = self.slot_ordinals.get(&delta.index) {
            ordinal
        } else if self.tools.len() >= self.binding.limits.calls.get() {
            return self.fail(
                ChatStreamErrorKind::ResourceLimit,
                "tool-call count exceeded its bound",
            );
        } else {
            let ordinal = self.tools.len();
            self.slot_ordinals.insert(delta.index, ordinal);
            self.tools.push(ToolAssembly::default());
            ordinal
        };
        let call = &mut self.tools[ordinal];
        if let Some(id) = &delta.id {
            if id.is_empty() {
                return self.fail(
                    ChatStreamErrorKind::InvalidResponse,
                    "tool-call id is empty",
                );
            }
            if call.id.as_ref().is_some_and(|old| old != id) {
                return self.fail(ChatStreamErrorKind::InvalidResponse, "tool-call id changed");
            }
            if call.id.is_none() && !self.remote_ids.insert(id.clone()) {
                return self.fail(
                    ChatStreamErrorKind::InvalidResponse,
                    "tool-call id repeated",
                );
            }
            call.id.get_or_insert_with(|| id.clone());
        }
        if let Some(function) = &delta.function {
            if let Some(name) = &function.name {
                if name.is_empty() {
                    return self.fail(
                        ChatStreamErrorKind::InvalidResponse,
                        "tool-call name is empty",
                    );
                }
                if call.name.as_ref().is_some_and(|old| old != name) {
                    return self.fail(
                        ChatStreamErrorKind::InvalidResponse,
                        "tool-call name changed",
                    );
                }
                call.name.get_or_insert_with(|| name.clone());
            }
            if let Some(arguments) = &function.arguments {
                call.arguments.push_str(arguments);
            }
        }
        if call.arguments.len() > self.binding.limits.arguments.get() {
            return self.fail(
                ChatStreamErrorKind::ResourceLimit,
                "tool arguments exceeded their bound",
            );
        }
        Ok(())
    }

    fn bind_reasoning_alias(
        &mut self,
        alias: BoundReasoningAlias,
        value: &str,
    ) -> Result<(), ChatStreamError> {
        if value.is_empty() {
            return self.fail(
                ChatStreamErrorKind::InvalidResponse,
                "empty reasoning delta",
            );
        }
        let admitted = match self.binding.reasoning_alias_policy {
            ReasoningAliasPolicy::Reasoning => alias == BoundReasoningAlias::Reasoning,
            ReasoningAliasPolicy::ReasoningContent => {
                alias == BoundReasoningAlias::ReasoningContent
            }
            ReasoningAliasPolicy::EitherBindFirst => {
                self.reasoning_alias.is_none_or(|bound| bound == alias)
            }
        };
        if !admitted {
            return self.fail(
                ChatStreamErrorKind::InvalidResponse,
                "reasoning alias differs from the prepared response policy",
            );
        }
        self.reasoning_alias.get_or_insert(alias);
        Ok(())
    }

    fn retain_reasoning_details(
        &mut self,
        details: Vec<Box<RawValue>>,
    ) -> Result<(), ChatStreamError> {
        if !self.binding.reasoning_replay.includes_exact_details() {
            return self.fail(
                ChatStreamErrorKind::UnsupportedSemantic,
                "reasoning details are excluded by the prepared replay policy",
            );
        }
        if details.is_empty() {
            return self.fail(
                ChatStreamErrorKind::InvalidResponse,
                "reasoning details delta is empty",
            );
        }
        let count = self
            .reasoning_details
            .len()
            .checked_add(details.len())
            .ok_or_else(|| {
                err(
                    ChatStreamErrorKind::ResourceLimit,
                    "reasoning detail count overflow",
                )
            })?;
        let bytes = details
            .iter()
            .try_fold(self.reasoning_detail_bytes, |retained, detail| {
                if !serde_json::from_str::<serde_json::Value>(detail.get())
                    .is_ok_and(|value| value.is_object())
                {
                    return Err(err(
                        ChatStreamErrorKind::InvalidResponse,
                        "reasoning detail must be one JSON object",
                    ));
                }
                retained.checked_add(detail.get().len()).ok_or_else(|| {
                    err(
                        ChatStreamErrorKind::ResourceLimit,
                        "reasoning detail byte count overflow",
                    )
                })
            })?;
        if count > self.binding.limits.reasoning_details.get()
            || bytes > self.binding.limits.reasoning_detail_bytes.get()
        {
            return self.fail(
                ChatStreamErrorKind::ResourceLimit,
                "reasoning details exceeded their independent bound",
            );
        }
        self.reasoning_details.extend(details);
        self.reasoning_detail_bytes = bytes;
        Ok(())
    }

    #[allow(
        clippy::too_many_lines,
        reason = "terminal validation and projection stay atomic"
    )]
    fn complete(&mut self, output: &mut Vec<ModelEvent>) -> Result<(), ChatStreamError> {
        if self.done || self.response_id.is_none() || !self.role_seen {
            return self.fail(
                ChatStreamErrorKind::InvalidResponse,
                "[DONE] arrived outside one assistant response",
            );
        }
        let finish = self.finish.clone().ok_or_else(|| {
            err(
                ChatStreamErrorKind::Incomplete,
                "[DONE] preceded finish reason",
            )
        })?;
        let usage = self.usage.ok_or_else(|| {
            err(
                ChatStreamErrorKind::Incomplete,
                "[DONE] preceded exact usage",
            )
        })?;
        if (finish.canonical == FinishReason::ToolCalls) == self.tools.is_empty() {
            return self.fail(
                ChatStreamErrorKind::InvalidResponse,
                "finish reason disagrees with tool calls",
            );
        }

        let item_id = self.item_id()?;
        let mut terminal_events = Vec::new();
        let mut blocks = Vec::new();
        let mut next_block = self.next_block;
        if let Some(location) = self.text_location {
            terminal_events.push(ModelEvent::BlockCompleted(BlockCompleted::new(location)));
            blocks.push((
                location,
                ContentBlock::Text(TextBlock::new(self.text.clone(), None).map_err(|_| {
                    err(
                        ChatStreamErrorKind::InvalidResponse,
                        "invalid canonical text",
                    )
                })?),
            ));
        }
        if let Some(location) = self.reasoning_location {
            terminal_events.push(ModelEvent::BlockCompleted(BlockCompleted::new(location)));
            blocks.push((
                location,
                ContentBlock::Reasoning(
                    ReasoningBlock::new(
                        ReasoningVisibility::Visible,
                        Some(self.reasoning.clone()),
                        None,
                    )
                    .map_err(|_| {
                        err(
                            ChatStreamErrorKind::InvalidResponse,
                            "invalid canonical reasoning",
                        )
                    })?,
                ),
            ));
        }

        let detail_bound_alias = match self.binding.reasoning_alias_policy {
            ReasoningAliasPolicy::Reasoning => ReasoningAliasPolicy::Reasoning,
            ReasoningAliasPolicy::ReasoningContent => ReasoningAliasPolicy::ReasoningContent,
            ReasoningAliasPolicy::EitherBindFirst => match self.reasoning_alias {
                Some(BoundReasoningAlias::Reasoning) => ReasoningAliasPolicy::Reasoning,
                Some(BoundReasoningAlias::ReasoningContent) => {
                    ReasoningAliasPolicy::ReasoningContent
                }
                None if self.reasoning_details.is_empty() => ReasoningAliasPolicy::Reasoning,
                None => {
                    return self.fail(
                        ChatStreamErrorKind::InvalidResponse,
                        "reasoning details lack a bound response alias",
                    );
                }
            },
        };
        for (ordinal, detail) in self.reasoning_details.iter().enumerate() {
            let source_ordinal = u32::try_from(ordinal).map_err(|_| {
                err(
                    ChatStreamErrorKind::ResourceLimit,
                    "reasoning detail ordinal overflow",
                )
            })?;
            let payload = ChatReasoningDetailReplayV1::new(
                self.binding.replay_target.clone(),
                self.binding.reasoning_policy,
                self.binding.reasoning_alias_policy,
                detail_bound_alias,
                detail.get().to_owned(),
                source_ordinal,
            )
            .map_err(|_| {
                err(
                    ChatStreamErrorKind::InvalidResponse,
                    "reasoning detail replay binding failed",
                )
            })?;
            let payload = serde_json::value::to_raw_value(&payload).map_err(|_| {
                err(
                    ChatStreamErrorKind::InvalidResponse,
                    "reasoning detail replay serialization failed",
                )
            })?;
            let envelope = ReplayEnvelope::new(
                self.binding.replay_target.protocol().clone(),
                None,
                ReplayScope::ExactModel(self.binding.replay_target.requested_model().clone()),
                ReplayKindId::new(REASONING_DETAIL_REPLAY_KIND).map_err(|_| {
                    err(
                        ChatStreamErrorKind::InvalidResponse,
                        "reasoning detail replay kind is invalid",
                    )
                })?,
                CHAT_REPLAY_SCHEMA_REVISION,
                DataClassification::Sensitive,
                payload,
            )
            .map_err(|_| {
                err(
                    ChatStreamErrorKind::InvalidResponse,
                    "reasoning detail replay envelope failed",
                )
            })?;
            let block = ContentBlock::Reasoning(
                ReasoningBlock::new(ReasoningVisibility::OpaqueReplay, None, Some(envelope))
                    .map_err(|_| {
                        err(
                            ChatStreamErrorKind::InvalidResponse,
                            "reasoning detail block failed",
                        )
                    })?,
            );
            let location = OutputLocation::new(0, next_block);
            next_block = next_block.checked_add(1).ok_or_else(|| {
                err(
                    ChatStreamErrorKind::ResourceLimit,
                    "Chat block index overflow",
                )
            })?;
            terminal_events.push(ModelEvent::BlockStarted(BlockStarted::new(
                item_id,
                location,
                BlockStart::Complete(block.clone()),
            )));
            blocks.push((location, block));
        }

        for call in &self.tools {
            let Some(remote) = call.id.as_deref().filter(|id| !id.is_empty()) else {
                return self.fail(
                    ChatStreamErrorKind::InvalidResponse,
                    "tool-call slot never bound id and name",
                );
            };
            let Some(name_raw) = call.name.as_deref().filter(|name| !name.is_empty()) else {
                return self.fail(
                    ChatStreamErrorKind::InvalidResponse,
                    "tool-call slot never bound id and name",
                );
            };
            // Bounded by the binding's argument cap, not the tool-schema node
            // and string limits: an oversized write is the tool runtime's
            // recoverable input error, as on Messages and Responses.
            let arguments = SemanticJsonV1::parse_document(
                &call.arguments,
                self.binding.limits.arguments.get(),
            )
            .map_err(|_| {
                err(
                    ChatStreamErrorKind::InvalidResponse,
                    "tool arguments are malformed or ambiguous",
                )
            })?;
            if !arguments.is_object() {
                return self.fail(
                    ChatStreamErrorKind::InvalidResponse,
                    "tool arguments must be one JSON object",
                );
            }
            // Only the syntactic name grammar is a wire fault. A name outside the
            // prepared catalog stays a projected call, exactly as on the Messages
            // and Responses routes, so the tool runtime answers the model with a
            // recoverable `UnknownTool` result instead of killing the stream.
            let name = ToolName::new(name_raw.to_owned())
                .map_err(|_| err(ChatStreamErrorKind::InvalidResponse, "invalid tool name"))?;
            let call_id =
                CallId::generate(self.clock.as_ref(), self.ids.as_ref()).map_err(|_| {
                    err(
                        ChatStreamErrorKind::InvalidResponse,
                        "call identity generation failed",
                    )
                })?;
            let correlation =
                ToolCorrelation::new(Some(remote.to_owned()), None, ToolCallerLineage::Direct)
                    .map_err(|_| {
                        err(
                            ChatStreamErrorKind::InvalidResponse,
                            "invalid remote call id",
                        )
                    })?;
            let location = OutputLocation::new(0, next_block);
            next_block = next_block.checked_add(1).ok_or_else(|| {
                err(
                    ChatStreamErrorKind::ResourceLimit,
                    "Chat block index overflow",
                )
            })?;
            terminal_events.push(ModelEvent::BlockStarted(BlockStarted::new(
                item_id,
                location,
                BlockStart::ToolCall {
                    call_id,
                    tool: name.clone(),
                    owner: ToolExecutionOwner::Local,
                    state: ToolCallState::Requested,
                    correlation: correlation.clone(),
                },
            )));
            terminal_events.push(ModelEvent::ToolInputDelta(ToolInputDelta::new(
                location,
                call.arguments.as_bytes(),
            )));
            terminal_events.push(ModelEvent::BlockCompleted(BlockCompleted::new(location)));
            blocks.push((
                location,
                ContentBlock::ToolCall(
                    ToolCallBlock::new(
                        call_id,
                        name,
                        ToolExecutionOwner::Local,
                        ToolCallState::Requested,
                        correlation,
                        call.arguments.clone(),
                    )
                    .map_err(|_| {
                        err(
                            ChatStreamErrorKind::InvalidResponse,
                            "invalid canonical tool call",
                        )
                    })?,
                ),
            ));
        }
        if blocks.is_empty() {
            return self.fail(
                ChatStreamErrorKind::InvalidResponse,
                "Chat completed without output",
            );
        }
        blocks.sort_by_key(|(location, _)| *location);
        let item = ConversationItem::new(
            item_id,
            Actor::Assistant,
            blocks.into_iter().map(|(_, block)| block).collect(),
            Provenance::ModelOutput,
        )
        .map_err(|_| {
            err(
                ChatStreamErrorKind::InvalidResponse,
                "invalid canonical assistant item",
            )
        })?;
        let response = ModelResponse::new(
            vec![item],
            FinishDetails::new(finish.canonical, Some(finish.wire.clone())),
            Some(usage),
            self.response_id.clone(),
            self.binding.plan,
        )
        .with_metadata({
            let mut metadata =
                ModelResponseMetadata::new(self.model.clone(), self.request_id.clone());
            if let Some(served_by) = self.served_by {
                metadata = metadata.with_served_by(served_by);
            }
            metadata
        });

        self.next_block = next_block;
        output.extend(terminal_events);
        self.terminal = Some(ChatTerminalProjection { response, finish });
        self.done = true;
        Ok(())
    }

    fn check_output_bound(&mut self) -> Result<(), ChatStreamError> {
        let bytes = self
            .text
            .len()
            .checked_add(self.reasoning.len())
            .ok_or_else(|| {
                err(
                    ChatStreamErrorKind::ResourceLimit,
                    "output byte count overflow",
                )
            })?;
        if bytes > self.binding.limits.output.get() {
            return self.fail(
                ChatStreamErrorKind::ResourceLimit,
                "output exceeded its byte bound",
            );
        }
        Ok(())
    }
    fn item_id(&mut self) -> Result<EntryId, ChatStreamError> {
        if let Some(id) = self.item_id {
            return Ok(id);
        }
        let id = EntryId::generate(self.clock.as_ref(), self.ids.as_ref()).map_err(|_| {
            err(
                ChatStreamErrorKind::InvalidResponse,
                "item identity generation failed",
            )
        })?;
        self.item_id = Some(id);
        Ok(id)
    }
    fn open_text(
        &mut self,
        output: &mut Vec<ModelEvent>,
    ) -> Result<OutputLocation, ChatStreamError> {
        if let Some(location) = self.text_location {
            return Ok(location);
        }
        let location = OutputLocation::new(0, self.next_block);
        self.next_block += 1;
        let item = self.item_id()?;
        output.push(ModelEvent::BlockStarted(BlockStarted::new(
            item,
            location,
            BlockStart::Text { phase: None },
        )));
        self.text_location = Some(location);
        Ok(location)
    }
    fn open_reasoning(
        &mut self,
        output: &mut Vec<ModelEvent>,
    ) -> Result<OutputLocation, ChatStreamError> {
        if let Some(location) = self.reasoning_location {
            return Ok(location);
        }
        let location = OutputLocation::new(0, self.next_block);
        self.next_block += 1;
        let item = self.item_id()?;
        output.push(ModelEvent::BlockStarted(BlockStarted::new(
            item,
            location,
            BlockStart::Reasoning {
                visibility: ReasoningVisibility::Visible,
                replay: None,
            },
        )));
        self.reasoning_location = Some(location);
        Ok(location)
    }
    fn fail<T>(
        &mut self,
        kind: ChatStreamErrorKind,
        message: impl Into<String>,
    ) -> Result<T, ChatStreamError> {
        self.failed = true;
        Err(err(kind, message))
    }
}

#[derive(Deserialize)]
struct Chunk {
    id: Option<String>,
    object: Option<String>,
    created: Option<u64>,
    model: Option<String>,
    #[serde(default)]
    choices: Vec<Choice>,
    usage: Option<UsageWire>,
    error: Option<serde_json::Value>,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}
#[derive(Deserialize)]
struct Choice {
    index: u32,
    #[serde(default)]
    delta: Delta,
    finish_reason: Option<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}
#[derive(Default, Deserialize)]
struct Delta {
    role: Option<String>,
    content: Option<String>,
    reasoning: Option<String>,
    reasoning_content: Option<String>,
    reasoning_details: Option<Vec<Box<RawValue>>>,
    #[serde(default)]
    tool_calls: Vec<ToolDelta>,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
struct ToolDelta {
    index: u32,
    id: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    function: Option<FunctionDelta>,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}
#[derive(Deserialize)]
struct FunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}
#[derive(Deserialize)]
struct UsageWire {
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: PromptDetails,
    #[serde(default)]
    completion_tokens_details: CompletionDetails,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}
#[derive(Default, Deserialize)]
struct PromptDetails {
    #[serde(default)]
    cached_tokens: u64,
    #[serde(default)]
    cache_write_tokens: u64,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}
#[derive(Default, Deserialize)]
struct CompletionDetails {
    #[serde(default)]
    reasoning_tokens: u64,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::needless_pass_by_value,
    clippy::unreadable_literal,
    reason = "focused protocol fixtures use infallible constructors and owned JSON values"
)]
mod tests;
