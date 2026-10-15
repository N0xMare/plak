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
    }
}

/// Reads the HTTP-shaped status an in-band provider error named, if any.
///
/// A gateway puts the upstream status in `error.code`, as a number or as its
/// decimal string, and that is where this looks first.
///
/// It also reads one prose spelling, and only one: an edge that could not reach
/// its upstream relays that upstream's own body, and the body says
/// `error code: 520` and nothing else machine-readable. The live shape this
/// exists for is exactly that — the diagnostic recorded in the field was
/// "Chat provider returned an in-band error: error code: 520" — so the status
/// was there, in the one place nothing could read it. Recognising that anchored
/// spelling is narrow on purpose: it is not general prose parsing, and anything
/// that does not match leaves the status unknown and the conservative
/// classification in place.
fn in_band_error_status(error: &serde_json::Value) -> Option<u16> {
    if let Some(code) = error.get("code") {
        // Every arm falls through rather than propagating: a `code` that is a
        // float, a word, or out of range is not an answer, and the message may
        // still hold one.
        let status = match code {
            serde_json::Value::Number(number) => {
                number.as_u64().and_then(|value| u16::try_from(value).ok())
            }
            serde_json::Value::String(text) => text.trim().parse::<u16>().ok(),
            _ => None,
        };
        if let Some(status) = status.filter(|status| (100..=599).contains(status)) {
            return Some(status);
        }
    }
    let message = match error {
        serde_json::Value::String(text) => text,
        serde_json::Value::Object(object) => object.get("message")?.as_str()?,
        _ => return None,
    };
    edge_error_code_status(message)
}

/// Reads `error code: NNN` from an edge body, anchored and nothing else.
fn edge_error_code_status(message: &str) -> Option<u16> {
    const ANCHOR: &str = "error code:";
    let lowered = message.to_ascii_lowercase();
    let start = lowered.find(ANCHOR)? + ANCHOR.len();
    let digits: String = message
        .get(start..)?
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    let status = digits.parse::<u16>().ok()?;
    (100..=599).contains(&status).then_some(status)
}

const IN_BAND_ERROR_DETAIL_CHARS: usize = 240;

fn in_band_error_detail(error: &serde_json::Value) -> String {
    let raw = match error {
        serde_json::Value::Object(object) => in_band_error_object_detail(object),
        serde_json::Value::String(text) => text.trim().to_owned(),
        serde_json::Value::Number(number) => number.to_string(),
        serde_json::Value::Bool(value) => value.to_string(),
        serde_json::Value::Null | serde_json::Value::Array(_) => String::new(),
    };
    let sanitized: String = raw
        .chars()
        .filter(|ch| *ch == ' ' || !ch.is_control())
        .take(IN_BAND_ERROR_DETAIL_CHARS)
        .collect();
    let trimmed = sanitized.trim();
    if trimmed.is_empty() {
        "unclassified".to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn in_band_error_object_detail(object: &serde_json::Map<String, serde_json::Value>) -> String {
    if let Some(message) = object
        .get("message")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|message| !message.is_empty())
    {
        return message.to_owned();
    }
    let mut parts = Vec::new();
    for key in ["code", "type"] {
        match object.get(key) {
            Some(serde_json::Value::String(text)) => {
                let text = text.trim();
                if !text.is_empty() {
                    parts.push(format!("{key}={text}"));
                }
            }
            Some(serde_json::Value::Number(number)) => {
                parts.push(format!("{key}={number}"));
            }
            Some(serde_json::Value::Object(nested)) => {
                if let Some(message) = nested
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|message| !message.is_empty())
                {
                    parts.push(format!("{key}={message}"));
                }
            }
            _ => {}
        }
    }
    parts.join("; ")
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
            return self.fail_with_status(
                ChatStreamErrorKind::ProviderError,
                SafeDiagnostic::new(format!(
                    "Chat provider returned an in-band error: {}",
                    in_band_error_detail(error)
                ))
                .to_string(),
                status,
                usage_limit,
            );
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
            let arguments = SemanticJsonV1::parse(&call.arguments).map_err(|_| {
                err(
                    ChatStreamErrorKind::InvalidResponse,
                    "tool arguments are malformed or ambiguous",
                )
            })?;
            if !arguments.value().is_object() {
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

    fn fail_with_status<T>(
        &mut self,
        kind: ChatStreamErrorKind,
        message: impl Into<String>,
        http_status: Option<u16>,
        usage_limit: bool,
    ) -> Result<T, ChatStreamError> {
        self.failed = true;
        Err(ChatStreamError {
            kind,
            message: message.into(),
            http_status,
            usage_limit,
        })
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
impl Delta {
    fn semantic(&self) -> bool {
        self.content.as_ref().is_some_and(|value| !value.is_empty())
            || self
                .reasoning
                .as_ref()
                .is_some_and(|value| !value.is_empty())
            || self
                .reasoning_content
                .as_ref()
                .is_some_and(|value| !value.is_empty())
            || self
                .reasoning_details
                .as_ref()
                .is_some_and(|details| !details.is_empty())
            || !self.tool_calls.is_empty()
    }
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
impl Chunk {
    fn extensions_allowed(&self, extensions: ChatStreamExtensions) -> bool {
        match extensions {
            ChatStreamExtensions::None => {
                self.extra.is_empty()
                    && self.choices.iter().all(Choice::has_no_extensions)
                    && self.usage.as_ref().is_none_or(UsageWire::has_no_extensions)
            }
            ChatStreamExtensions::OpenRouterV1 => {
                keys_allowed(
                    &self.extra,
                    &[
                        "provider",
                        "service_tier",
                        "openrouter_metadata",
                        "system_fingerprint",
                    ],
                ) && self
                    .choices
                    .iter()
                    .all(Choice::has_only_openrouter_v1_extensions)
                    && self
                        .usage
                        .as_ref()
                        .is_none_or(UsageWire::has_only_openrouter_v1_extensions)
            }
        }
    }

    fn first_unreviewed_extension(&self, extensions: ChatStreamExtensions) -> Option<String> {
        let allowed_chunk = match extensions {
            ChatStreamExtensions::None => &[][..],
            ChatStreamExtensions::OpenRouterV1 => &[
                "provider",
                "service_tier",
                "openrouter_metadata",
                "system_fingerprint",
            ][..],
        };
        first_unallowed_key(&self.extra, allowed_chunk)
            .map(|key| format!("chunk.{key}"))
            .or_else(|| {
                self.choices.iter().find_map(|choice| {
                    let allowed_choice = match extensions {
                        ChatStreamExtensions::None => &[][..],
                        ChatStreamExtensions::OpenRouterV1 => &["native_finish_reason"][..],
                    };
                    first_unallowed_key(&choice.extra, allowed_choice)
                        .map(|key| format!("choice.{key}"))
                        .or_else(|| {
                            first_unallowed_key(&choice.delta.extra, &[])
                                .map(|key| format!("choice.delta.{key}"))
                        })
                        .or_else(|| {
                            choice.delta.tool_calls.iter().find_map(|tool| {
                                first_unallowed_key(&tool.extra, &[])
                                    .map(|key| format!("choice.delta.tool.{key}"))
                                    .or_else(|| {
                                        tool.function.as_ref().and_then(|function| {
                                            first_unallowed_key(&function.extra, &[]).map(|key| {
                                                format!("choice.delta.tool.function.{key}")
                                            })
                                        })
                                    })
                            })
                        })
                })
            })
            .or_else(|| {
                let usage = self.usage.as_ref()?;
                let (usage_allowed, prompt_allowed, completion_allowed) = match extensions {
                    ChatStreamExtensions::None => (&[][..], &[][..], &[][..]),
                    ChatStreamExtensions::OpenRouterV1 => (
                        &["cost", "is_byok", "cost_details"][..],
                        &["cache_write_tokens", "audio_tokens", "video_tokens"][..],
                        &["image_tokens", "audio_tokens"][..],
                    ),
                };
                first_unallowed_key(&usage.extra, usage_allowed)
                    .map(|key| format!("usage.{key}"))
                    .or_else(|| {
                        first_unallowed_key(&usage.prompt_tokens_details.extra, prompt_allowed)
                            .map(|key| format!("usage.prompt_tokens_details.{key}"))
                    })
                    .or_else(|| {
                        first_unallowed_key(
                            &usage.completion_tokens_details.extra,
                            completion_allowed,
                        )
                        .map(|key| format!("usage.completion_tokens_details.{key}"))
                    })
            })
    }
}
impl Choice {
    fn has_no_extensions(&self) -> bool {
        self.extra.is_empty() && self.delta.has_no_extensions()
    }

    fn has_only_openrouter_v1_extensions(&self) -> bool {
        keys_allowed(&self.extra, &["native_finish_reason"]) && self.delta.has_no_extensions()
    }
}
impl Delta {
    fn has_no_extensions(&self) -> bool {
        self.extra.is_empty() && self.tool_calls.iter().all(ToolDelta::has_no_extensions)
    }
}
impl ToolDelta {
    fn has_no_extensions(&self) -> bool {
        self.extra.is_empty()
            && self
                .function
                .as_ref()
                .is_none_or(|function| function.extra.is_empty())
    }
}
impl UsageWire {
    fn has_no_extensions(&self) -> bool {
        self.extra.is_empty()
            && self.prompt_tokens_details.extra.is_empty()
            && self.completion_tokens_details.extra.is_empty()
    }

    fn has_only_openrouter_v1_extensions(&self) -> bool {
        keys_allowed(&self.extra, &["cost", "is_byok", "cost_details"])
            && keys_allowed(
                &self.prompt_tokens_details.extra,
                &["cache_write_tokens", "audio_tokens", "video_tokens"],
            )
            && keys_allowed(
                &self.completion_tokens_details.extra,
                &["image_tokens", "audio_tokens"],
            )
    }
}

fn keys_allowed(extra: &BTreeMap<String, serde_json::Value>, allowed: &[&str]) -> bool {
    extra.keys().all(|key| allowed.contains(&key.as_str()))
}

fn first_unallowed_key<'a>(
    extra: &'a BTreeMap<String, serde_json::Value>,
    allowed: &[&str],
) -> Option<&'a str> {
    extra
        .keys()
        .find(|key| !allowed.contains(&key.as_str()))
        .map(String::as_str)
}

impl UsageWire {
    fn canonical(self, extensions: ChatStreamExtensions) -> Result<Usage, ChatStreamError> {
        if self.prompt_tokens.checked_add(self.completion_tokens) != Some(self.total_tokens)
            || (extensions == ChatStreamExtensions::None
                && (self.prompt_tokens_details.cached_tokens > self.prompt_tokens
                    || self.completion_tokens_details.reasoning_tokens > self.completion_tokens))
        {
            return Err(err(
                ChatStreamErrorKind::InvalidResponse,
                "usage counters are inconsistent",
            ));
        }
        let cost_micros = parse_cost_micros(&self.extra, extensions)?;
        Ok(Usage::new(
            self.prompt_tokens,
            self.completion_tokens,
            self.completion_tokens_details.reasoning_tokens,
            self.prompt_tokens_details.cached_tokens,
            self.prompt_tokens_details.cache_write_tokens,
        )
        .with_cost_micros(cost_micros))
    }
}

fn capture_served_provider(
    extra: &BTreeMap<String, serde_json::Value>,
    extensions: ChatStreamExtensions,
    served_by: &mut Option<ServedBy>,
) -> Result<(), ChatStreamError> {
    if extensions != ChatStreamExtensions::OpenRouterV1 {
        return Ok(());
    }
    let Some(value) = extra.get("provider") else {
        return Ok(());
    };
    let text = value.as_str().ok_or_else(|| {
        err(
            ChatStreamErrorKind::InvalidResponse,
            "provider is not a string",
        )
    })?;
    let parsed = ServedBy::new(text)
        .map_err(|_| err(ChatStreamErrorKind::InvalidResponse, "provider is invalid"))?;
    if let Some(existing) = served_by {
        if *existing != parsed {
            return Err(err(
                ChatStreamErrorKind::InvalidResponse,
                "provider changed mid-stream",
            ));
        }
        return Ok(());
    }
    *served_by = Some(parsed);
    Ok(())
}

fn parse_cost_micros(
    extra: &BTreeMap<String, serde_json::Value>,
    extensions: ChatStreamExtensions,
) -> Result<Option<u64>, ChatStreamError> {
    if extensions != ChatStreamExtensions::OpenRouterV1 {
        return Ok(None);
    }
    let Some(value) = extra.get("cost") else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let number = value.as_number().ok_or_else(|| {
        err(
            ChatStreamErrorKind::InvalidResponse,
            "usage cost is not a number",
        )
    })?;
    Ok(Some(usd_number_to_cost_micros(number)?))
}

/// Largest USD value whose micro-dollar rounding fits in `u64`.
const MAX_USD_FOR_COST_MICROS: f64 = 18_446_744_073_709.55;

fn usd_number_to_cost_micros(number: &serde_json::Number) -> Result<u64, ChatStreamError> {
    let invalid = || {
        err(
            ChatStreamErrorKind::InvalidResponse,
            "usage cost is invalid",
        )
    };
    let out_of_range = || {
        err(
            ChatStreamErrorKind::InvalidResponse,
            "usage cost is out of range",
        )
    };
    let usd = number.as_f64().ok_or_else(invalid)?;
    if !usd.is_finite() || usd < 0.0 {
        return Err(invalid());
    }
    if usd > MAX_USD_FOR_COST_MICROS {
        return Err(out_of_range());
    }
    let micros_f64 = (usd * 1_000_000.0).round();
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "usd is bounded below MAX_USD so the rounded micros fit u64"
    )]
    Ok(micros_f64 as u64)
}
fn parse_finish(value: &str) -> Result<ChatFinishReason, ChatStreamError> {
    let canonical = match value {
        "stop" => FinishReason::Stop,
        "tool_calls" => FinishReason::ToolCalls,
        "length" => FinishReason::Length,
        "content_filter" => FinishReason::Policy,
        _ => {
            return Err(err(
                ChatStreamErrorKind::UnsupportedSemantic,
                "finish reason is outside Chat V1",
            ));
        }
    };
    Ok(ChatFinishReason {
        wire: value.to_owned(),
        canonical,
    })
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::needless_pass_by_value,
    clippy::unreadable_literal,
    reason = "focused protocol fixtures use infallible constructors and owned JSON values"
)]
mod tests {
    use super::*;

    #[test]
    fn an_in_band_rate_limit_status_with_a_quota_type_is_terminal() {
        let mut projector = projector();
        let error = projector.push(&data(&serde_json::json!({"error":{"code":429,"type":"insufficient_quota","message":"fixture quota"}}))).expect_err("quota");
        assert!(error.usage_limit());
        assert_eq!(error.http_status(), Some(429));
    }

    #[test]
    fn openrouter_usage_chunk_is_structurally_bounded() {
        let data = r#"{"id":"gen-exact","object":"chat.completion.chunk","created":1787623695,"model":"model-literal","provider":"Nvidia","service_tier":null,"choices":[{"index":0,"delta":{"content":"","role":"assistant"},"finish_reason":"length","native_finish_reason":"length"}],"usage":{"prompt_tokens":18,"completion_tokens":8,"total_tokens":26,"cost":0,"is_byok":false,"prompt_tokens_details":{"cached_tokens":0,"cache_write_tokens":0,"audio_tokens":0,"video_tokens":0},"cost_details":{"upstream_inference_cost":0,"upstream_inference_prompt_cost":0,"upstream_inference_completions_cost":0},"completion_tokens_details":{"reasoning_tokens":8,"image_tokens":0,"audio_tokens":0}}}"#;
        let chunk: Chunk = serde_json::from_str(data).expect("OpenRouter chunk grammar");
        assert!(chunk.extensions_allowed(ChatStreamExtensions::OpenRouterV1));
        assert!(!chunk.extensions_allowed(ChatStreamExtensions::None));
    }

    #[test]
    fn openrouter_usage_chunk_projects_cost_micros() {
        let data = r#"{"id":"gen-exact","object":"chat.completion.chunk","created":1787623695,"model":"model-literal","provider":"Nvidia","service_tier":null,"choices":[{"index":0,"delta":{"content":"","role":"assistant"},"finish_reason":"length","native_finish_reason":"length"}],"usage":{"prompt_tokens":18,"completion_tokens":8,"total_tokens":26,"cost":0,"is_byok":false,"prompt_tokens_details":{"cached_tokens":0,"cache_write_tokens":0,"audio_tokens":0,"video_tokens":0},"cost_details":{"upstream_inference_cost":0,"upstream_inference_prompt_cost":0,"upstream_inference_completions_cost":0},"completion_tokens_details":{"reasoning_tokens":8,"image_tokens":0,"audio_tokens":0}}}"#;
        let chunk: Chunk = serde_json::from_str(data).expect("OpenRouter chunk grammar");
        let usage = chunk
            .usage
            .expect("usage")
            .canonical(ChatStreamExtensions::OpenRouterV1)
            .expect("canonical usage");
        assert_eq!(usage.cost_micros(), Some(0));
    }

    #[test]
    fn openrouter_usage_chunk_projects_fractional_cost_micros() {
        let data = r#"{"id":"gen-exact","object":"chat.completion.chunk","created":1787623695,"model":"model-literal","provider":"Nvidia","service_tier":null,"choices":[{"index":0,"delta":{"content":"","role":"assistant"},"finish_reason":"length","native_finish_reason":"length"}],"usage":{"prompt_tokens":18,"completion_tokens":8,"total_tokens":26,"cost":0.0001234,"is_byok":false,"prompt_tokens_details":{"cached_tokens":0,"cache_write_tokens":0,"audio_tokens":0,"video_tokens":0},"completion_tokens_details":{"reasoning_tokens":8,"image_tokens":0,"audio_tokens":0}}}"#;
        let chunk: Chunk = serde_json::from_str(data).expect("OpenRouter chunk grammar");
        let usage = chunk
            .usage
            .expect("usage")
            .canonical(ChatStreamExtensions::OpenRouterV1)
            .expect("canonical usage");
        assert_eq!(usage.cost_micros(), Some(123));
    }

    #[test]
    fn openrouter_provider_chunk_captures_served_by_once() {
        let binding = binding(ChatStreamLimits::default())
            .with_extensions(ChatStreamExtensions::OpenRouterV1);
        let mut projector = projector_with_binding(binding);
        let mut role = role();
        role["provider"] = json!("Nvidia");
        push_json(&mut projector, role);
        push_json(
            &mut projector,
            chunk(json!([{"index": 0, "delta": {"content": "answer"}, "finish_reason": null}])),
        );
        let _ = complete(&mut projector, "stop");
        let terminal = projector.finish().expect("terminal");
        assert_eq!(
            terminal.response().metadata().served_by(),
            Some(ServedBy::new("Nvidia").expect("provider"))
        );
    }

    #[test]
    fn openrouter_invalid_provider_is_rejected() {
        let binding = binding(ChatStreamLimits::default())
            .with_extensions(ChatStreamExtensions::OpenRouterV1);
        let mut projector = projector_with_binding(binding);
        let mut role = role();
        role["provider"] = json!("x".repeat(65));
        assert_eq!(
            push_chunk(&mut projector, role)
                .expect_err("oversized provider")
                .kind(),
            ChatStreamErrorKind::InvalidResponse
        );
    }

    #[test]
    fn openrouter_contradictory_provider_fails_stream() {
        let binding = binding(ChatStreamLimits::default())
            .with_extensions(ChatStreamExtensions::OpenRouterV1);
        let mut projector = projector_with_binding(binding);
        let mut first = role();
        first["provider"] = json!("Alpha");
        push_json(&mut projector, first);
        let mut second = role();
        second["provider"] = json!("Beta");
        assert_eq!(
            push_chunk(&mut projector, second)
                .expect_err("contradictory provider")
                .kind(),
            ChatStreamErrorKind::InvalidResponse
        );
    }

    #[test]
    fn openrouter_usage_cost_matrix() {
        let extensions = ChatStreamExtensions::OpenRouterV1;
        let canonical = |cost: Value| {
            let mut extra = BTreeMap::new();
            extra.insert("cost".into(), cost);
            UsageWire {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
                prompt_tokens_details: PromptDetails::default(),
                completion_tokens_details: CompletionDetails::default(),
                extra,
            }
            .canonical(extensions)
        };
        assert_eq!(
            canonical(serde_json::json!(0)).expect("zero").cost_micros(),
            Some(0)
        );
        assert_eq!(
            canonical(serde_json::json!(0.0001234))
                .expect("fractional")
                .cost_micros(),
            Some(123)
        );
        assert_eq!(
            canonical(serde_json::json!(42))
                .expect("integer")
                .cost_micros(),
            Some(42_000_000)
        );
        assert_eq!(
            canonical(serde_json::json!(0.0001234567))
                .expect("sixth fractional digit")
                .cost_micros(),
            Some(123)
        );
        assert_eq!(
            canonical(serde_json::json!(0.00000042))
                .expect("sub-micro rounds to zero")
                .cost_micros(),
            Some(0)
        );
        assert_eq!(
            canonical(serde_json::json!(1e-7))
                .expect("one tenth micro")
                .cost_micros(),
            Some(0)
        );
        assert_eq!(
            canonical(serde_json::json!(0.0000015))
                .expect("one and a half micros")
                .cost_micros(),
            Some(2)
        );
        assert_eq!(
            canonical(serde_json::json!(1e-6))
                .expect("exactly one micro")
                .cost_micros(),
            Some(1)
        );
        assert_eq!(
            canonical(serde_json::json!(0.000123456789))
                .expect("long fractional")
                .cost_micros(),
            Some(123)
        );
        let cost_number =
            |text: &str| -> Value { serde_json::from_str(text).expect("decimal literal") };
        assert_eq!(
            canonical(cost_number("0.0001235678"))
                .expect("round seventh digit")
                .cost_micros(),
            Some(124)
        );
        assert_eq!(
            canonical(serde_json::json!(null))
                .expect("null")
                .cost_micros(),
            None
        );
        assert_eq!(
            canonical(serde_json::json!(-1))
                .expect_err("negative")
                .kind(),
            ChatStreamErrorKind::InvalidResponse
        );
        assert_eq!(
            canonical(serde_json::json!("free"))
                .expect_err("non-number")
                .kind(),
            ChatStreamErrorKind::InvalidResponse
        );
        assert_eq!(
            canonical(Value::Number(Number::from(18_446_744_073_795_u64)))
                .expect_err("overflow")
                .kind(),
            ChatStreamErrorKind::InvalidResponse
        );
    }

    #[test]
    fn openrouter_usage_chunk_projects_cache_write_tokens() {
        let data = r#"{"id":"gen-exact","object":"chat.completion.chunk","created":1787623695,"model":"model-literal","provider":"Nvidia","service_tier":null,"choices":[{"index":0,"delta":{"content":"","role":"assistant"},"finish_reason":"length","native_finish_reason":"length"}],"usage":{"prompt_tokens":18,"completion_tokens":8,"total_tokens":26,"cost":0,"is_byok":false,"prompt_tokens_details":{"cached_tokens":0,"cache_write_tokens":5,"audio_tokens":0,"video_tokens":0},"cost_details":{"upstream_inference_cost":0,"upstream_inference_prompt_cost":0,"upstream_inference_completions_cost":0},"completion_tokens_details":{"reasoning_tokens":8,"image_tokens":0,"audio_tokens":0}}}"#;
        let chunk: Chunk = serde_json::from_str(data).expect("OpenRouter chunk grammar");
        let usage = chunk
            .usage
            .expect("usage")
            .canonical(ChatStreamExtensions::OpenRouterV1)
            .expect("canonical usage");
        assert_eq!(usage.cache_write_tokens(), 5);
    }

    #[test]
    fn openrouter_router_metadata_is_observational_and_additive() {
        let value = json!({
            "id": "gen-exact",
            "object": "chat.completion.chunk",
            "created": 1,
            "model": "vendor/model",
            "provider": "Provider",
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            "openrouter_metadata": {
                "requested": "vendor/model",
                "strategy": "fallback",
                "attempt": 2,
                "pipeline": [{
                    "type": "future_gateway_stage",
                    "data": {"opaque_future_member": [1, true, null]}
                }],
                "future_gateway_member": {"opaque": true}
            }
        });
        let chunk: Chunk = serde_json::from_value(value).expect("gateway metadata JSON");
        assert!(chunk.extensions_allowed(ChatStreamExtensions::OpenRouterV1));
        assert!(!chunk.extensions_allowed(ChatStreamExtensions::None));
    }

    #[test]
    fn openrouter_system_fingerprint_is_observational() {
        let value = json!({
            "id": "gen-exact",
            "object": "chat.completion.chunk",
            "created": 1,
            "model": "vendor/model",
            "provider": "Provider",
            "system_fingerprint": "fp-observed",
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
        });
        let chunk: Chunk = serde_json::from_value(value).expect("fingerprint JSON");
        assert!(chunk.extensions_allowed(ChatStreamExtensions::OpenRouterV1));
        assert!(!chunk.extensions_allowed(ChatStreamExtensions::None));
    }
    use plakous_model::{
        DigestDomain, EffectiveProviderConfigurationId, FixedClock, ModelResponseAssembler,
        SelectedProviderConfigurationId, SequenceIdGenerator, Timestamp,
    };
    use serde_json::{Number, Value, json};
    use uuid::Uuid;

    fn binding(limits: ChatStreamLimits) -> PreparedChatStreamBinding {
        binding_with_policy(
            limits,
            ChatReasoningReplay::ExactDetails,
            ReasoningAliasPolicy::Reasoning,
        )
    }

    fn binding_with_policy(
        limits: ChatStreamLimits,
        replay: ChatReasoningReplay,
        alias: ReasoningAliasPolicy,
    ) -> PreparedChatStreamBinding {
        let selected: SelectedProviderConfigurationId = serde_json::from_value(
            serde_json::to_value(
                Digest::sha256(
                    DigestDomain::SelectedProviderConfiguration,
                    5,
                    &[b"chat-stream-selected"],
                )
                .expect("selected digest"),
            )
            .expect("selected value"),
        )
        .expect("selected identity");
        let effective: EffectiveProviderConfigurationId = serde_json::from_value(
            serde_json::to_value(
                Digest::sha256(
                    DigestDomain::EffectiveProviderConfiguration,
                    5,
                    &[b"chat-stream-effective"],
                )
                .expect("effective digest"),
            )
            .expect("effective value"),
        )
        .expect("effective identity");
        let protocol = ProtocolId::new("chat-completions.v1").expect("protocol");
        let model = ModelId::new("model-literal").expect("model");
        let replay_target = ChatReplayTargetV1::new(
            protocol.clone(),
            model.clone(),
            Digest::sha256(DigestDomain::ModelBinding, 1, &[b"account"]).expect("account"),
            selected,
            effective,
        );
        PreparedChatStreamBinding::new(
            Digest::sha256(DigestDomain::RequestPlan, 1, &[b"chat-stream-test"]).expect("digest"),
            protocol,
            model,
            replay_target,
            ReasoningPolicy::Disabled,
            replay,
            alias,
            limits,
        )
    }

    fn projector() -> ChatStreamProjector {
        projector_with_limits(ChatStreamLimits::default())
    }

    fn projector_with_limits(limits: ChatStreamLimits) -> ChatStreamProjector {
        projector_with_binding(binding(limits))
    }

    fn projector_with_binding(binding: PreparedChatStreamBinding) -> ChatStreamProjector {
        ChatStreamProjector::new(
            binding,
            Some(ModelProviderRequestId::new("request-head-1").expect("request id")),
            Arc::new(FixedClock::new(
                Timestamp::new(1_700_000_000, 0).expect("time"),
            )),
            Arc::new(SequenceIdGenerator::new((1_u128..=32).map(Uuid::from_u128)).expect("ids")),
        )
    }

    fn chunk(choices: Value) -> Value {
        json!({
            "id": "response-1",
            "object": "chat.completion.chunk",
            "created": 1_700_000_000_u64,
            "model": "model-literal",
            "choices": choices
        })
    }

    fn data(value: &Value) -> Vec<u8> {
        format!("data: {value}\n\n").into_bytes()
    }

    fn push_json(projector: &mut ChatStreamProjector, value: Value) -> Vec<ModelEvent> {
        projector.push(&data(&value)).expect("valid chunk")
    }

    fn push_chunk(
        projector: &mut ChatStreamProjector,
        value: Value,
    ) -> Result<Vec<ModelEvent>, ChatStreamError> {
        projector.push(&data(&value))
    }

    fn role() -> Value {
        chunk(json!([{"index": 0, "delta": {"role": "assistant"}, "finish_reason": null}]))
    }

    fn finish(reason: &str) -> Value {
        chunk(json!([{"index": 0, "delta": {}, "finish_reason": reason}]))
    }

    fn usage() -> Value {
        json!({
            "id": "response-1",
            "object": "chat.completion.chunk",
            "created": 1_700_000_000_u64,
            "model": "model-literal",
            "choices": [],
            "usage": {
                "prompt_tokens": 11,
                "completion_tokens": 7,
                "total_tokens": 18,
                "prompt_tokens_details": {"cached_tokens": 3},
                "completion_tokens_details": {"reasoning_tokens": 4}
            }
        })
    }

    fn complete(projector: &mut ChatStreamProjector, reason: &str) -> Vec<ModelEvent> {
        let mut events = push_json(projector, finish(reason));
        events.extend(push_json(projector, usage()));
        events.extend(projector.push(b"data: [DONE]\n\n").expect("done"));
        events
    }

    fn detail_chunk(details: &str) -> Vec<u8> {
        format!(
            "data: {{\"id\":\"response-1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"model-literal\",\"choices\":[{{\"index\":0,\"delta\":{{\"reasoning_details\":[{details}]}},\"finish_reason\":null}}]}}\n\n"
        )
        .into_bytes()
    }

    #[test]
    fn projects_fragmented_text_reasoning_usage_and_terminal_response() {
        let mut projector = projector();
        let role_bytes = data(&role());
        let mut events = Vec::new();
        for byte in role_bytes {
            events.extend(projector.push(&[byte]).expect("fragment"));
        }
        events.extend(push_json(
            &mut projector,
            chunk(json!([{"index": 0, "delta": {"reasoning": "why "}, "finish_reason": null}])),
        ));
        events.extend(push_json(
            &mut projector,
            chunk(json!([{"index": 0, "delta": {"reasoning": "so", "content": "answer"}, "finish_reason": null}])),
        ));
        events.extend(complete(&mut projector, "stop"));

        assert!(matches!(
            events.first(),
            Some(ModelEvent::ResponseStarted(_))
        ));
        assert!(events.iter().any(
            |event| matches!(event, ModelEvent::TextDelta(delta) if delta.bytes() == b"answer")
        ));
        assert!(events.iter().any(
            |event| matches!(event, ModelEvent::ReasoningDelta(delta) if delta.bytes() == b"why ")
        ));
        assert!(events.iter().any(|event| matches!(event, ModelEvent::Usage(update) if update.usage() == Usage::new(11, 7, 4, 3, 0))));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, ModelEvent::ResponseCompleted(_)))
        );

        let terminal = projector.finish().expect("terminal");
        assert_eq!(terminal.finish().wire(), "stop");
        assert_eq!(terminal.finish().canonical(), FinishReason::Stop);
        assert_eq!(
            projector
                .finish()
                .expect_err("terminal moves out once")
                .kind(),
            ChatStreamErrorKind::InvalidResponse
        );
        let response = terminal.response();
        assert_eq!(response.response_id(), Some("response-1"));
        assert_eq!(response.provider_request_id(), Some("request-head-1"));
        assert_eq!(
            response.actual_model().map(ModelId::as_str),
            Some("model-literal")
        );
        assert_eq!(response.usage(), Some(Usage::new(11, 7, 4, 3, 0)));
        assert!(response.family_artifact_candidate().is_none());
        let blocks = response.output()[0].blocks();
        assert!(
            matches!(&blocks[0], ContentBlock::Reasoning(block) if block.visibility() == ReasoningVisibility::Visible && block.text() == Some("why so"))
        );
        assert!(matches!(&blocks[1], ContentBlock::Text(block) if block.text() == "answer"));
    }

    #[test]
    fn enforces_fixed_and_response_stable_reasoning_aliases() {
        let limits = ChatStreamLimits::default();
        let mut content = projector_with_binding(binding_with_policy(
            limits,
            ChatReasoningReplay::VisibleOnly,
            ReasoningAliasPolicy::ReasoningContent,
        ));
        push_json(&mut content, role());
        let bytes = data(&chunk(json!([{
            "index": 0,
            "delta": {"reasoning_content": "mémoire"},
            "finish_reason": null
        }])));
        for byte in bytes {
            content.push(&[byte]).expect("fragmented UTF-8 alias");
        }
        complete(&mut content, "stop");
        assert!(matches!(
            &content.finish().expect("content alias").response().output()[0].blocks()[0],
            ContentBlock::Reasoning(block) if block.text() == Some("mémoire")
        ));

        for (alias, delta) in [
            (
                ReasoningAliasPolicy::Reasoning,
                json!({"reasoning_content": "wrong"}),
            ),
            (
                ReasoningAliasPolicy::ReasoningContent,
                json!({"reasoning": "wrong"}),
            ),
            (
                ReasoningAliasPolicy::EitherBindFirst,
                json!({"reasoning": "a", "reasoning_content": "b"}),
            ),
        ] {
            let mut rejected = projector_with_binding(binding_with_policy(
                limits,
                ChatReasoningReplay::VisibleOnly,
                alias,
            ));
            push_json(&mut rejected, role());
            let error = rejected
                .push(&data(&chunk(json!([{
                    "index": 0,
                    "delta": delta,
                    "finish_reason": null
                }]))))
                .expect_err("alias rejected");
            assert_eq!(error.kind(), ChatStreamErrorKind::InvalidResponse);
        }

        let mut switched = projector_with_binding(binding_with_policy(
            limits,
            ChatReasoningReplay::VisibleOnly,
            ReasoningAliasPolicy::EitherBindFirst,
        ));
        push_json(&mut switched, role());
        push_json(
            &mut switched,
            chunk(json!([{
                "index": 0,
                "delta": {"reasoning": "bound"},
                "finish_reason": null
            }])),
        );
        let error = switched
            .push(&data(&chunk(json!([{
                "index": 0,
                "delta": {"reasoning_content": "switch"},
                "finish_reason": null
            }]))))
            .expect_err("response alias switch rejected");
        assert_eq!(error.kind(), ChatStreamErrorKind::InvalidResponse);
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one end-to-end fixture verifies exact replay, terminal ordering, and assembly"
    )]
    fn projects_ordered_exact_reasoning_details_through_the_assembler() {
        let binding = binding_with_policy(
            ChatStreamLimits::default(),
            ChatReasoningReplay::ExactDetails,
            ReasoningAliasPolicy::EitherBindFirst,
        );
        let plan = binding.plan();
        let mut projector = projector_with_binding(binding);
        let mut events = push_json(&mut projector, role());
        events.extend(push_json(
            &mut projector,
            chunk(json!([{
                "index": 0,
                "delta": {"reasoning_content": "visible"},
                "finish_reason": null
            }])),
        ));
        let first = r#"{ "z" : "\u0061", "signature" : "sig\/one" }"#;
        let second = r#"{"encrypted":"A\\u003dB","index":7}"#;
        events.extend(
            projector
                .push(&detail_chunk(&format!("{first},{second}")))
                .expect("two exact details"),
        );
        events.extend(
            projector
                .push(&detail_chunk(r#"{"third" : [3, 2, 1]}"#))
                .expect("detail in later chunk"),
        );
        events.extend(push_json(
            &mut projector,
            chunk(json!([{
                "index": 0,
                "delta": {"tool_calls": [{
                    "index": 0,
                    "id": "call-a",
                    "type": "function",
                    "function": {"name": "lookup", "arguments": "{}"}
                }]},
                "finish_reason": null
            }])),
        ));
        events.extend(complete(&mut projector, "tool_calls"));

        let detail_starts: Vec<_> = events
            .iter()
            .filter_map(|event| {
                let ModelEvent::BlockStarted(started) = event else {
                    return None;
                };
                let BlockStart::Complete(ContentBlock::Reasoning(block)) = started.block() else {
                    return None;
                };
                (block.visibility() == ReasoningVisibility::OpaqueReplay)
                    .then_some((started.location(), block))
            })
            .collect();
        assert_eq!(detail_starts.len(), 3);
        let expected = [first, second, r#"{"third" : [3, 2, 1]}"#];
        for (ordinal, ((location, block), exact)) in detail_starts.iter().zip(expected).enumerate()
        {
            assert_eq!(
                location.block_index(),
                u32::try_from(ordinal + 1).expect("index")
            );
            let bound: ChatReasoningDetailReplayV1 =
                serde_json::from_str(block.replay().expect("detail replay").payload().get())
                    .expect("bound payload");
            assert_eq!(
                bound.source_ordinal(),
                u32::try_from(ordinal).expect("ordinal")
            );
            assert_eq!(bound.exact_json(), exact);
            assert_eq!(bound.alias_policy(), ReasoningAliasPolicy::EitherBindFirst);
            assert_eq!(bound.bound_alias(), ReasoningAliasPolicy::ReasoningContent);
        }
        let detail_locations: BTreeSet<_> = detail_starts
            .iter()
            .map(|(location, _)| *location)
            .collect();
        assert!(!events.iter().any(|event| {
            matches!(event, ModelEvent::BlockCompleted(completed) if detail_locations.contains(&completed.location()))
        }));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, ModelEvent::ResponseCompleted(_)))
        );
        assert!(events.iter().any(|event| {
            matches!(event, ModelEvent::BlockStarted(started) if matches!(started.block(), BlockStart::ToolCall { .. }))
        }));

        let terminal = projector.finish().expect("terminal");
        assert!(terminal.response().family_artifact_candidate().is_none());
        let terminal_blocks = terminal.response().output()[0].blocks();
        assert_eq!(terminal_blocks.len(), 5);
        for (location, event_block) in detail_starts {
            assert_eq!(
                &terminal_blocks[location.block_index() as usize],
                &ContentBlock::Reasoning(event_block.clone())
            );
        }
        let checkpoint = serde_json::to_vec(terminal.response()).expect("checkpoint JSON");
        let restored: ModelResponse =
            serde_json::from_slice(&checkpoint).expect("checkpoint round trip");
        for (block, exact) in restored.output()[0].blocks()[1..4].iter().zip(expected) {
            let ContentBlock::Reasoning(reasoning) = block else {
                panic!("restored detail must remain reasoning");
            };
            let bound: ChatReasoningDetailReplayV1 =
                serde_json::from_str(reasoning.replay().expect("restored replay").payload().get())
                    .expect("restored bound payload");
            assert_eq!(bound.exact_json(), exact);
        }
        let mut assembler = ModelResponseAssembler::new(plan).expect("assembler");
        let mut terminal_response = None;
        events.push(ModelEvent::ResponseCompleted(terminal.response().clone()));
        for event in events {
            if let Some(response) = assembler.push(event).expect("valid lifecycle") {
                terminal_response = Some(response);
            }
        }
        assert_eq!(terminal_response.as_ref(), Some(terminal.response()));
    }

    #[test]
    fn independently_bounds_reasoning_detail_count_and_bytes() {
        let nz = |value| NonZeroUsize::new(value).expect("nonzero");
        for (limits, details) in [
            (
                ChatStreamLimits::default().with_reasoning_detail_limits(nz(1), nz(1024)),
                r#"{"a":1},{"b":2}"#,
            ),
            (
                ChatStreamLimits::default().with_reasoning_detail_limits(nz(8), nz(6)),
                r#"{"long":true}"#,
            ),
        ] {
            let mut projector = projector_with_binding(binding_with_policy(
                limits,
                ChatReasoningReplay::ExactDetails,
                ReasoningAliasPolicy::Reasoning,
            ));
            push_json(&mut projector, role());
            let error = projector
                .push(&detail_chunk(details))
                .expect_err("independent detail limit");
            assert_eq!(error.kind(), ChatStreamErrorKind::ResourceLimit);
        }

        let mut excluded = projector_with_binding(binding_with_policy(
            ChatStreamLimits::default(),
            ChatReasoningReplay::VisibleOnly,
            ReasoningAliasPolicy::Reasoning,
        ));
        push_json(&mut excluded, role());
        assert_eq!(
            excluded
                .push(&detail_chunk(r#"{"a":1}"#))
                .expect_err("details excluded")
                .kind(),
            ChatStreamErrorKind::UnsupportedSemantic
        );
    }

    #[test]
    fn assembles_dense_local_function_calls_with_exact_raw_arguments() {
        let mut projector = projector();
        push_json(&mut projector, role());
        push_json(
            &mut projector,
            chunk(json!([{"index": 0, "delta": {"tool_calls": [
            {"index": 0, "id": "call-a", "type": "function", "function": {"name": "lookup", "arguments": "{\"q\":"}},
            {"index": 1, "id": "call-b", "type": "function", "function": {"name": "write", "arguments": "{\"n\":"}}
        ]}, "finish_reason": null}])),
        );
        push_json(
            &mut projector,
            chunk(json!([{"index": 0, "delta": {"tool_calls": [
            {"index": 0, "function": {"arguments": "\"x\"}"}},
            {"index": 1, "function": {"arguments": "2}"}}
        ]}, "finish_reason": null}])),
        );
        let events = complete(&mut projector, "tool_calls");

        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ModelEvent::ToolInputDelta(_)))
                .count(),
            2
        );
        let terminal = projector.finish().expect("terminal");
        let blocks = terminal.response().output()[0].blocks();
        assert_eq!(blocks.len(), 2);
        assert!(
            matches!(&blocks[0], ContentBlock::ToolCall(call) if call.exposed_name().as_str() == "lookup" && call.raw_arguments() == "{\"q\":\"x\"}" && call.correlation().remote_call_id() == Some("call-a") && call.owner() == &ToolExecutionOwner::Local)
        );
        assert!(
            matches!(&blocks[1], ContentBlock::ToolCall(call) if call.exposed_name().as_str() == "write" && call.raw_arguments() == "{\"n\":2}")
        );
    }

    #[test]
    fn rejects_choice_identity_object_and_unknown_wire_fields() {
        for (value, kind) in [
            (chunk(json!([])), ChatStreamErrorKind::InvalidResponse),
            (
                chunk(json!([
                    {"index": 0, "delta": {}, "finish_reason": null},
                    {"index": 1, "delta": {}, "finish_reason": null}
                ])),
                ChatStreamErrorKind::InvalidResponse,
            ),
            (
                json!({"id":"response-1","object":"other","created":1700000000,"model":"model-literal","choices":[{"index":0,"delta":{},"finish_reason":null}]}),
                ChatStreamErrorKind::InvalidResponse,
            ),
            (
                json!({"id":"response-1","object":"chat.completion.chunk","created":1700000000,"model":"other-model","choices":[{"index":0,"delta":{},"finish_reason":null}]}),
                ChatStreamErrorKind::InvalidResponse,
            ),
            (
                json!({"id":"response-1","object":"chat.completion.chunk","created":1700000000,"model":"model-literal","choices":[{"index":0,"delta":{},"finish_reason":null}],"extra":true}),
                ChatStreamErrorKind::InvalidResponse,
            ),
        ] {
            let error = projector().push(&data(&value)).expect_err("rejected");
            assert_eq!(error.kind(), kind);
        }

        let mut changed_created = projector();
        push_json(&mut changed_created, role());
        let mut changed = chunk(json!([{"index":0,"delta":{},"finish_reason":null}]));
        changed["created"] = json!(1_700_000_001_u64);
        assert_eq!(
            changed_created
                .push(&data(&changed))
                .expect_err("created changed")
                .kind(),
            ChatStreamErrorKind::InvalidResponse
        );
    }

    #[test]
    fn openrouter_rejects_unknown_semantics_outside_reviewed_metadata() {
        let mut projector = ChatStreamProjector::new(
            binding(ChatStreamLimits::default())
                .with_extensions(ChatStreamExtensions::OpenRouterV1),
            Some(ModelProviderRequestId::new("request-1").expect("request id")),
            Arc::new(FixedClock::new(
                Timestamp::new(1_700_000_000, 0).expect("timestamp"),
            )),
            Arc::new(
                SequenceIdGenerator::new((1_u128..=8).map(Uuid::from_u128)).expect("sequence ids"),
            ),
        );
        let value = json!({
            "id": "response-1",
            "object": "chat.completion.chunk",
            "created": 1_700_000_000_u64,
            "model": "model-literal",
            "provider": "reviewed-metadata",
            "choices": [{
                "index": 0,
                "delta": {
                    "role": "assistant",
                    "future_hosted_action": {"id": "effect-1"}
                },
                "finish_reason": null,
                "native_finish_reason": null
            }]
        });
        assert_eq!(
            projector
                .push(&data(&value))
                .expect_err("unknown semantic")
                .kind(),
            ChatStreamErrorKind::InvalidResponse
        );

        let mut sibling = projector_with_binding(
            binding(ChatStreamLimits::default())
                .with_extensions(ChatStreamExtensions::OpenRouterV1),
        );
        let mut value = role();
        value["future_gateway_action"] = json!({"id": "effect-2"});
        let error = sibling
            .push(&data(&value))
            .expect_err("unknown top-level semantic");
        assert_eq!(error.kind(), ChatStreamErrorKind::InvalidResponse);
        assert!(error.message().contains("chunk.future_gateway_action"));
    }

    #[test]
    fn openrouter_metadata_is_ignored_through_the_terminal_projector_lifecycle() {
        let openrouter_binding = binding(ChatStreamLimits::default())
            .with_extensions(ChatStreamExtensions::OpenRouterV1);
        let mut openrouter_projector = projector_with_binding(openrouter_binding);
        push_json(&mut openrouter_projector, role());
        push_json(
            &mut openrouter_projector,
            chunk(json!([{
                "index": 0,
                "delta": {"content": "answer"},
                "finish_reason": null
            }])),
        );
        push_json(&mut openrouter_projector, finish("stop"));
        let mut final_usage = usage();
        final_usage["openrouter_metadata"] = json!({
            "attempt": 2,
            "pipeline": [{
                "type": "future_gateway_stage",
                "data": {"opaque_future_member": [1, true, null]}
            }],
            "future_gateway_member": {"opaque": true}
        });
        push_json(&mut openrouter_projector, final_usage.clone());
        openrouter_projector
            .push(b"data: [DONE]\n\n")
            .expect("done");
        let terminal = openrouter_projector.finish().expect("terminal");
        assert_eq!(terminal.finish().canonical(), FinishReason::Stop);
        assert_eq!(
            terminal.response().usage(),
            Some(Usage::new(11, 7, 4, 3, 0))
        );
        assert!(matches!(
            &terminal.response().output()[0].blocks()[0],
            ContentBlock::Text(block) if block.text() == "answer"
        ));

        let mut generic = projector();
        let error = generic
            .push(&data(&final_usage))
            .expect_err("OpenRouter metadata outside its prepared dialect");
        assert_eq!(error.kind(), ChatStreamErrorKind::InvalidResponse);
        assert!(error.message().contains("chunk.openrouter_metadata"));
    }

    #[test]
    fn any_projection_failure_terminally_poisons_the_projector() {
        let mut projector = projector();
        assert_eq!(
            projector
                .push(b"data: {bad}\n\n")
                .expect_err("malformed JSON")
                .kind(),
            ChatStreamErrorKind::InvalidResponse
        );
        assert_eq!(
            projector
                .push(&data(&role()))
                .expect_err("poisoned projector")
                .message(),
            "Chat projector already failed"
        );
        assert_eq!(
            projector.finish().expect_err("poisoned finish").message(),
            "Chat projector already failed"
        );
    }

    #[test]
    fn rejects_incomplete_and_post_terminal_lifecycles() {
        let mut no_done = projector();
        push_json(&mut no_done, role());
        push_json(
            &mut no_done,
            chunk(json!([{"index":0,"delta":{"content":"x"},"finish_reason":null}])),
        );
        assert_eq!(
            no_done.finish().expect_err("missing done").kind(),
            ChatStreamErrorKind::Incomplete
        );

        let mut no_usage = projector();
        push_json(&mut no_usage, role());
        push_json(
            &mut no_usage,
            chunk(json!([{"index":0,"delta":{"content":"x"},"finish_reason":null}])),
        );
        push_json(&mut no_usage, finish("stop"));
        assert_eq!(
            no_usage
                .push(b"data: [DONE]\n\n")
                .expect_err("missing usage")
                .kind(),
            ChatStreamErrorKind::Incomplete
        );

        let mut post = projector();
        push_json(&mut post, role());
        push_json(
            &mut post,
            chunk(json!([{"index":0,"delta":{"content":"x"},"finish_reason":null}])),
        );
        complete(&mut post, "stop");
        assert_eq!(
            post.push(b"data: {}\n\n")
                .expect_err("post terminal")
                .kind(),
            ChatStreamErrorKind::InvalidResponse
        );
    }

    #[test]
    fn rejects_conflicting_and_malformed_tool_calls() {
        let cases = [
            json!([{"index":0,"delta":{"tool_calls":[{"index":0,"id":"a","type":"hosted","function":{"name":"lookup","arguments":"{}"}}]},"finish_reason":null}]),
        ];
        for choices in cases {
            let mut projector = projector();
            push_json(&mut projector, role());
            assert!(projector.push(&data(&chunk(choices))).is_err());
        }

        let mut unbound = projector();
        push_json(&mut unbound, role());
        push_json(
            &mut unbound,
            chunk(
                json!([{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{}"}}]},"finish_reason":null}]),
            ),
        );
        push_json(&mut unbound, finish("tool_calls"));
        push_json(&mut unbound, usage());
        let error = unbound.push(b"data: [DONE]\n\n").expect_err("unbound slot");
        assert_eq!(error.kind(), ChatStreamErrorKind::InvalidResponse);
        assert!(
            error
                .message()
                .contains("tool-call slot never bound id and name")
        );

        let mut malformed = projector();
        push_json(&mut malformed, role());
        push_json(
            &mut malformed,
            chunk(
                json!([{"index":0,"delta":{"tool_calls":[{"index":0,"id":"a","type":"function","function":{"name":"lookup","arguments":"{"}}]},"finish_reason":null}]),
            ),
        );
        push_json(&mut malformed, finish("tool_calls"));
        push_json(&mut malformed, usage());
        assert_eq!(
            malformed
                .push(b"data: [DONE]\n\n")
                .expect_err("malformed arguments")
                .kind(),
            ChatStreamErrorKind::InvalidResponse
        );

        for raw in ["{\"x\":1,\"x\":2}", "[]"] {
            let mut ambiguous = projector();
            push_json(&mut ambiguous, role());
            push_json(
                &mut ambiguous,
                chunk(
                    json!([{"index":0,"delta":{"tool_calls":[{"index":0,"id":"a","type":"function","function":{"name":"lookup","arguments":raw}}]},"finish_reason":null}]),
                ),
            );
            push_json(&mut ambiguous, finish("tool_calls"));
            push_json(&mut ambiguous, usage());
            assert_eq!(
                ambiguous
                    .push(b"data: [DONE]\n\n")
                    .expect_err("ambiguous arguments")
                    .kind(),
                ChatStreamErrorKind::InvalidResponse
            );
        }
    }

    /// The status an in-band error names, wherever the gateway put it.
    ///
    /// The live shape is the last one: an edge that could not reach its
    /// upstream relays that upstream's body, and the body carries the status
    /// only in prose. The field diagnostic was "Chat provider returned an
    /// in-band error: error code: 520", so the status was there and nothing
    /// could read it.
    #[test]
    fn an_in_band_error_yields_the_status_the_gateway_named() {
        use super::in_band_error_status;

        for (payload, expected) in [
            (json!({"code": 520}), Some(520)),
            (json!({"code": "429"}), Some(429)),
            (
                json!({"code": 502, "message": "upstream unavailable"}),
                Some(502),
            ),
            (json!({"message": "error code: 520"}), Some(520)),
            (
                json!({"message": "Error Code: 429 from the edge"}),
                Some(429),
            ),
            (json!("error code: 524"), Some(524)),
            // Nothing machine-readable and no anchored spelling: the status
            // stays unknown and the conservative classification stands.
            (json!({"message": "Provider returned error"}), None),
            (json!({"code": "failure"}), None),
            (json!({"code": 99}), None),
            (json!({"message": "error code: 9999"}), None),
            (json!({}), None),
        ] {
            assert_eq!(in_band_error_status(&payload), expected, "{payload}");
        }
    }

    #[test]
    fn rejects_usage_mismatch_unsupported_finish_and_provider_error() {
        const SECRET_SENTINEL: &str = "SECRET_SENTINEL_DO_NOT_RETAIN";
        let mut bad_usage = usage();
        bad_usage["usage"]["total_tokens"] = json!(99);
        let mut bad_usage_projector = projector();
        push_json(&mut bad_usage_projector, role());
        push_json(
            &mut bad_usage_projector,
            chunk(json!([{"index":0,"delta":{"content":"x"},"finish_reason":null}])),
        );
        push_json(&mut bad_usage_projector, finish("stop"));
        assert_eq!(
            bad_usage_projector
                .push(&data(&bad_usage))
                .expect_err("accounting")
                .kind(),
            ChatStreamErrorKind::InvalidResponse
        );

        let mut unsupported = projector();
        push_json(&mut unsupported, role());
        assert_eq!(
            unsupported
                .push(&data(&finish("other")))
                .expect_err("finish")
                .kind(),
            ChatStreamErrorKind::UnsupportedSemantic
        );

        let error = projector()
            .push(&data(&json!({"error":{"code":"failure"}})))
            .expect_err("provider error");
        assert_eq!(error.kind(), ChatStreamErrorKind::ProviderError);
        assert_eq!(
            error.message(),
            "Chat provider returned an in-band error: code=failure"
        );

        let nested = projector()
            .push(&data(&json!({
                "error": {"message": "Provider returned error", "code": 502}
            })))
            .expect_err("provider message");
        assert_eq!(
            nested.message(),
            "Chat provider returned an in-band error: Provider returned error"
        );

        let empty = projector()
            .push(&data(&json!({"error":{}})))
            .expect_err("empty provider error");
        assert_eq!(
            empty.message(),
            "Chat provider returned an in-band error: unclassified"
        );

        let redacted = projector()
            .push(&data(&json!({
                "error": {
                    "code": "auth_error",
                    "message": format!("Bearer {SECRET_SENTINEL}")
                }
            })))
            .expect_err("credential-bearing provider error");
        assert_eq!(redacted.kind(), ChatStreamErrorKind::ProviderError);
        assert!(
            !redacted.message().contains(SECRET_SENTINEL),
            "in-band Chat error must not retain credential forms: {}",
            redacted.message()
        );
        assert!(redacted.message().contains("[redacted]"));
    }

    #[test]
    fn enforces_output_argument_event_and_sse_bounds() {
        let nz = |value| NonZeroUsize::new(value).expect("nonzero");
        let output_limits =
            ChatStreamLimits::new(nz(1024), nz(4096), nz(20), nz(1), nz(2), nz(2), nz(1 << 20));
        let mut output = projector_with_limits(output_limits);
        push_json(&mut output, role());
        assert_eq!(
            output
                .push(&data(&chunk(
                    json!([{"index":0,"delta":{"content":"xx"},"finish_reason":null}])
                )))
                .expect_err("output bound")
                .kind(),
            ChatStreamErrorKind::ResourceLimit
        );

        let argument_limits = ChatStreamLimits::new(
            nz(1024),
            nz(4096),
            nz(20),
            nz(100),
            nz(1),
            nz(2),
            nz(1 << 20),
        );
        let mut arguments = projector_with_limits(argument_limits);
        push_json(&mut arguments, role());
        assert_eq!(arguments.push(&data(&chunk(json!([{"index":0,"delta":{"tool_calls":[{"index":0,"id":"a","type":"function","function":{"name":"lookup","arguments":"{}x"}}]},"finish_reason":null}])))).expect_err("argument bound").kind(), ChatStreamErrorKind::ResourceLimit);

        let line_limits =
            ChatStreamLimits::new(nz(4), nz(32), nz(1), nz(10), nz(1), nz(10), nz(1 << 20));
        assert_eq!(
            projector_with_limits(line_limits)
                .push(b"data: too long")
                .expect_err("line bound")
                .kind(),
            ChatStreamErrorKind::ResourceLimit
        );
    }

    #[test]
    fn total_stream_byte_bound_counts_comments_and_spans_tool_calls() {
        let nz = |value| NonZeroUsize::new(value).expect("nonzero");
        let limits = ChatStreamLimits::new(
            nz(1024),
            nz(4096),
            nz(20),
            nz(1024),
            nz(4),
            nz(1024),
            nz(600),
        );
        let mut comments = projector_with_limits(limits);
        let keepalive = b": keepalive\n\n".repeat(50);
        assert!(keepalive.len() > 600);
        assert_eq!(
            comments
                .push(&keepalive)
                .expect_err("comment trickle exceeds the total byte bound")
                .kind(),
            ChatStreamErrorKind::ResourceLimit
        );

        let mut calls = projector_with_limits(limits);
        push_json(&mut calls, role());
        let mut error = None;
        for index in 0..4 {
            let pushed = calls.push(&data(&chunk(json!([{"index":0,"delta":{"tool_calls":[{"index":index,"id":format!("c{index}"),"type":"function","function":{"name":"lookup","arguments":"{}"}}]},"finish_reason":null}]))));
            if let Err(failure) = pushed {
                error = Some(failure);
                break;
            }
        }
        assert_eq!(
            error
                .expect("aggregate tool calls exceed the total byte bound")
                .kind(),
            ChatStreamErrorKind::ResourceLimit
        );
    }

    #[test]
    fn maps_part_indexed_tool_slots_to_dense_ordinals() {
        let mut projector = projector();
        push_json(&mut projector, role());
        push_json(
            &mut projector,
            chunk(json!([{"index": 0, "delta": {"reasoning": "plan"}, "finish_reason": null}])),
        );
        push_json(
            &mut projector,
            chunk(
                json!([{"index": 0, "delta": {"content": "I'll create a file"}, "finish_reason": null}]),
            ),
        );
        push_json(
            &mut projector,
            chunk(json!([{"index": 0, "delta": {"tool_calls": [
                {"index": 2, "id": "call_a1", "type": "function", "function": {"name": "write_file", "arguments": ""}}
            ]}, "finish_reason": null}])),
        );
        push_json(
            &mut projector,
            chunk(json!([{"index": 0, "delta": {"tool_calls": [
                {"index": 2, "function": {"arguments": "{\"path\":\"hello.txt\","}}
            ]}, "finish_reason": null}])),
        );
        push_json(
            &mut projector,
            chunk(json!([{"index": 0, "delta": {"tool_calls": [
                {"index": 2, "function": {"arguments": "\"content\":\"hi\"}"}}
            ]}, "finish_reason": null}])),
        );
        complete(&mut projector, "tool_calls");

        let terminal = projector.finish().expect("terminal");
        let blocks = terminal.response().output()[0].blocks();
        assert_eq!(blocks.len(), 3);
        assert!(matches!(&blocks[0], ContentBlock::Reasoning(_)));
        assert!(matches!(&blocks[1], ContentBlock::Text(_)));
        assert!(matches!(&blocks[2], ContentBlock::ToolCall(call)
                if call.exposed_name().as_str() == "write_file"
                && call.raw_arguments() == "{\"path\":\"hello.txt\",\"content\":\"hi\"}"
                && call.correlation().remote_call_id() == Some("call_a1")));
    }

    #[test]
    fn maps_sparse_tool_index_reorder_by_first_seen_ordinal() {
        let mut projector = projector();
        push_json(&mut projector, role());
        push_json(
            &mut projector,
            chunk(json!([{"index": 0, "delta": {"tool_calls": [
                {"index": 3, "id": "call-b", "type": "function", "function": {"name": "second_tool", "arguments": "{}"}}
            ]}, "finish_reason": null}])),
        );
        push_json(
            &mut projector,
            chunk(json!([{"index": 0, "delta": {"tool_calls": [
                {"index": 2, "id": "call-a", "type": "function", "function": {"name": "first_tool", "arguments": "{}"}}
            ]}, "finish_reason": null}])),
        );
        complete(&mut projector, "tool_calls");

        let terminal = projector.finish().expect("terminal");
        let blocks = terminal.response().output()[0].blocks();
        assert_eq!(blocks.len(), 2);
        assert!(matches!(&blocks[0], ContentBlock::ToolCall(call)
                if call.exposed_name().as_str() == "second_tool"
                && call.correlation().remote_call_id() == Some("call-b")));
        assert!(matches!(&blocks[1], ContentBlock::ToolCall(call)
                if call.exposed_name().as_str() == "first_tool"
                && call.correlation().remote_call_id() == Some("call-a")));
    }

    #[test]
    fn rejects_empty_tool_call_id_or_name() {
        let mut empty_id_projector = projector();
        push_json(&mut empty_id_projector, role());
        assert_eq!(
            push_chunk(
                &mut empty_id_projector,
                chunk(json!([{"index": 0, "delta": {"tool_calls": [
                    {"index": 0, "id": "", "type": "function", "function": {"name": "lookup", "arguments": "{}"}}
                ]}, "finish_reason": null}]))
            )
            .expect_err("empty id")
            .kind(),
            ChatStreamErrorKind::InvalidResponse
        );

        let mut empty_name_projector = projector();
        push_json(&mut empty_name_projector, role());
        assert_eq!(
            push_chunk(
                &mut empty_name_projector,
                chunk(json!([{"index": 0, "delta": {"tool_calls": [
                    {"index": 0, "id": "call-a", "type": "function", "function": {"name": "", "arguments": "{}"}}
                ]}, "finish_reason": null}]))
            )
            .expect_err("empty name")
            .kind(),
            ChatStreamErrorKind::InvalidResponse
        );
    }

    #[test]
    fn projects_unadvertised_tool_name_and_refuses_invalid_spelling() {
        // A name outside the prepared catalog is not a wire fault: the tool runtime
        // answers the model with a recoverable UnknownTool result on the next turn.
        let mut unadvertised = projector();
        push_json(&mut unadvertised, role());
        push_json(
            &mut unadvertised,
            chunk(json!([{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "id": "call-x", "type": "function", "function": {"name": "not_a_tool", "arguments": "{\"path\":\"a\"}"}}
            ]}, "finish_reason": null}])),
        );
        complete(&mut unadvertised, "tool_calls");
        let terminal = unadvertised.finish().expect("terminal");
        let blocks = terminal.response().output()[0].blocks();
        assert_eq!(blocks.len(), 1);
        assert!(matches!(&blocks[0], ContentBlock::ToolCall(call)
                if call.exposed_name().as_str() == "not_a_tool"
                && call.raw_arguments() == "{\"path\":\"a\"}"
                && call.correlation().remote_call_id() == Some("call-x")));

        // Syntactically impossible names remain a wire fault.
        let mut invalid = projector();
        push_json(&mut invalid, role());
        push_json(
            &mut invalid,
            chunk(json!([{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "id": "call-y", "type": "function", "function": {"name": "not a tool", "arguments": "{}"}}
            ]}, "finish_reason": null}])),
        );
        push_json(&mut invalid, finish("tool_calls"));
        push_json(&mut invalid, usage());
        let error = invalid.push(b"data: [DONE]\n\n").expect_err("invalid name");
        assert_eq!(error.kind(), ChatStreamErrorKind::InvalidResponse);
        assert!(error.message().contains("invalid tool name"));

        // A non-object argument payload remains a wire fault.
        let mut non_object = projector();
        push_json(&mut non_object, role());
        push_json(
            &mut non_object,
            chunk(json!([{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "id": "call-z", "type": "function", "function": {"name": "not_a_tool", "arguments": "[]"}}
            ]}, "finish_reason": null}])),
        );
        push_json(&mut non_object, finish("tool_calls"));
        push_json(&mut non_object, usage());
        let error = non_object
            .push(b"data: [DONE]\n\n")
            .expect_err("non-object arguments");
        assert_eq!(error.kind(), ChatStreamErrorKind::InvalidResponse);
        assert!(
            error
                .message()
                .contains("tool arguments must be one JSON object")
        );
    }

    #[test]
    fn maps_parallel_part_indexed_tool_slots() {
        let mut projector = projector();
        push_json(&mut projector, role());
        push_json(
            &mut projector,
            chunk(json!([{"index": 0, "delta": {"content": "go"}, "finish_reason": null}])),
        );
        push_json(
            &mut projector,
            chunk(json!([{"index": 0, "delta": {"tool_calls": [
                {"index": 2, "id": "call-a", "type": "function", "function": {"name": "lookup", "arguments": "{\"q\":"}},
                {"index": 3, "id": "call-b", "type": "function", "function": {"name": "write", "arguments": "{\"n\":"}}
            ]}, "finish_reason": null}])),
        );
        push_json(
            &mut projector,
            chunk(json!([{"index": 0, "delta": {"tool_calls": [
                {"index": 2, "function": {"arguments": "\"x\"}"}},
                {"index": 3, "function": {"arguments": "2}"}}
            ]}, "finish_reason": null}])),
        );
        complete(&mut projector, "tool_calls");

        let terminal = projector.finish().expect("terminal");
        let blocks = terminal.response().output()[0].blocks();
        assert_eq!(blocks.len(), 3);
        assert!(matches!(&blocks[1], ContentBlock::ToolCall(call)
                if call.exposed_name().as_str() == "lookup"
                && call.correlation().remote_call_id() == Some("call-a")));
        assert!(matches!(&blocks[2], ContentBlock::ToolCall(call)
                if call.exposed_name().as_str() == "write"
                && call.correlation().remote_call_id() == Some("call-b")));
    }

    #[test]
    fn admits_tool_call_when_name_arrives_in_second_delta() {
        let mut projector = projector();
        push_json(&mut projector, role());
        push_json(
            &mut projector,
            chunk(json!([{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "id": "call-a", "type": "function", "function": {"arguments": "{\"q\":"}}
            ]}, "finish_reason": null}])),
        );
        push_json(
            &mut projector,
            chunk(json!([{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "function": {"name": "lookup", "arguments": "\"x\"}"}}
            ]}, "finish_reason": null}])),
        );
        complete(&mut projector, "tool_calls");

        let terminal = projector.finish().expect("terminal");
        let blocks = terminal.response().output()[0].blocks();
        assert!(matches!(&blocks[0], ContentBlock::ToolCall(call)
                if call.exposed_name().as_str() == "lookup"
                && call.raw_arguments() == "{\"q\":\"x\"}"
                && call.correlation().remote_call_id() == Some("call-a")));
    }

    #[test]
    fn rejects_tool_slot_without_name_at_finish() {
        let mut projector = projector();
        push_json(&mut projector, role());
        push_json(
            &mut projector,
            chunk(json!([{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "id": "call-a", "type": "function", "function": {"arguments": "{}"}}
            ]}, "finish_reason": null}])),
        );
        push_json(&mut projector, finish("tool_calls"));
        push_json(&mut projector, usage());
        let error = projector
            .push(b"data: [DONE]\n\n")
            .expect_err("missing name");
        assert_eq!(error.kind(), ChatStreamErrorKind::InvalidResponse);
        assert!(
            error
                .message()
                .contains("tool-call slot never bound id and name")
        );
    }

    #[test]
    fn rejects_repeated_tool_call_id_across_slots() {
        let mut projector = projector();
        push_json(&mut projector, role());
        let error = projector
            .push(&data(&chunk(json!([{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "id": "call-a", "type": "function", "function": {"name": "lookup", "arguments": "{}"}},
                {"index": 1, "id": "call-a", "type": "function", "function": {"name": "write", "arguments": "{}"}}
            ]}, "finish_reason": null}]))))
            .expect_err("repeated id");
        assert_eq!(error.kind(), ChatStreamErrorKind::InvalidResponse);
        assert!(error.message().contains("tool-call id repeated"));
    }

    #[test]
    fn rejects_tool_call_slot_index_at_bound() {
        let mut projector = projector();
        push_json(&mut projector, role());
        let error = projector
            .push(&data(&chunk(json!([{"index": 0, "delta": {"tool_calls": [
                {"index": 4096, "id": "call-a", "type": "function", "function": {"name": "lookup", "arguments": "{}"}}
            ]}, "finish_reason": null}]))))
            .expect_err("index bound");
        assert_eq!(error.kind(), ChatStreamErrorKind::ResourceLimit);
        assert!(
            error
                .message()
                .contains("tool-call index exceeded its bound")
        );
    }

    #[test]
    fn rejects_repeated_finish_reason_chunk_without_usage() {
        let openrouter_binding = binding(ChatStreamLimits::default())
            .with_extensions(ChatStreamExtensions::OpenRouterV1);
        let mut projector = projector_with_binding(openrouter_binding);
        push_json(&mut projector, role());
        push_json(
            &mut projector,
            chunk(json!([{"index": 0, "delta": {"content": "answer"}, "finish_reason": null}])),
        );
        push_json(&mut projector, finish("stop"));
        let error = projector
            .push(&data(&chunk(json!([{
                "index": 0,
                "delta": {},
                "finish_reason": "stop",
                "native_finish_reason": "stop"
            }]))))
            .expect_err("second bare finish");
        assert_eq!(error.kind(), ChatStreamErrorKind::InvalidResponse);
        assert!(
            error
                .message()
                .contains("choice index or lifecycle is invalid")
        );
    }

    #[test]
    fn accepts_matching_finish_reason_in_usage_chunk() {
        let openrouter_binding = binding(ChatStreamLimits::default())
            .with_extensions(ChatStreamExtensions::OpenRouterV1);
        let mut projector = projector_with_binding(openrouter_binding);
        push_json(&mut projector, role());
        push_json(
            &mut projector,
            chunk(json!([{"index": 0, "delta": {"content": "answer"}, "finish_reason": null}])),
        );
        push_json(&mut projector, finish("stop"));
        let mut terminal_usage = usage();
        terminal_usage["choices"] = json!([{
            "index": 0,
            "delta": {},
            "finish_reason": "stop",
            "native_finish_reason": "stop"
        }]);
        push_json(&mut projector, terminal_usage);
        projector.push(b"data: [DONE]\n\n").expect("done");
        let terminal = projector.finish().expect("terminal");
        assert_eq!(terminal.finish().canonical(), FinishReason::Stop);
    }
}
