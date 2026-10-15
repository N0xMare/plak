//! Shared stream front-half around [`ResponsesStreamKernel`].
//!
//! Dialects keep JSON event-kind parse hooks; the host owns admitted push/finish
//! trial semantics and common error maps / event-kind shapes.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::AtomicU8;

use plakous_attempt::{is_decoding, mark_decode_failed, mark_decoded};
use plakous_http::SseError;
use plakous_model::{Digest, DigestDomain};
use plakous_responses::{
    BomPolicy, DialectStreamHooks, FinishPolicy, LifecycleDialectPolicy, ReasoningLifecycleMode,
    ResponsesDecodeLimits, ResponsesDialectProfile, ResponsesFieldError, ResponsesLifecycle,
    ResponsesLifecycleError, ResponsesStreamKernel, ResponsesStreamLimits, SequencePolicy,
    SequenceValidationError, TerminalPolicy, required_object, required_provider_label,
    required_string, required_u32,
};
use serde_json::{Map, Value};

mod event_kind;
pub use event_kind::ResponsesStreamEventKind;

use crate::response_host::{
    ResponsesAnnotation, ResponsesHostDecodeError, ResponsesOutputItem, ResponsesProviderError,
    ResponsesProviderErrorPolicy, ResponsesResponse, ResponsesResponseLimits,
    ResponsesResponseStatus, ResponsesUsagePolicy, parse_annotation, parse_output_item,
    parse_provider_error_with_canonical_type, parse_unique_json_value,
    validate_bounded_unknown_item,
};

/// Closed capability data for one admitted Responses stream dialect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResponsesStreamCapabilities {
    enabled: u8,
}

impl ResponsesStreamCapabilities {
    const CUSTOM_TOOLS: u8 = 1;
    const CITATIONS: u8 = 1 << 1;
    const INCOMPLETE_TERMINAL: u8 = 1 << 2;
    const FAILED_TERMINAL: u8 = 1 << 3;
    const RETAIN_UNKNOWN: u8 = 1 << 4;
    const DONE_TERMINAL: u8 = 1 << 5;
    const REASONING_CONTENT_TEXT: u8 = 1 << 6;

    /// Capability data required by the Codex Responses surface.
    #[must_use]
    pub const fn codex() -> Self {
        Self {
            enabled: Self::CUSTOM_TOOLS
                | Self::CITATIONS
                | Self::INCOMPLETE_TERMINAL
                | Self::FAILED_TERMINAL
                | Self::RETAIN_UNKNOWN,
        }
    }

    /// Builds a dialect with no optional extension surface admitted.
    #[must_use]
    pub const fn minimal() -> Self {
        Self { enabled: 0 }
    }

    /// Capability data required by the SuperGrok Responses surface.
    #[must_use]
    pub const fn grok() -> Self {
        Self {
            enabled: Self::INCOMPLETE_TERMINAL | Self::FAILED_TERMINAL | Self::RETAIN_UNKNOWN,
        }
    }

    /// Capability data admitted by the OpenRouter beta fixture surface.
    #[must_use]
    pub const fn openrouter() -> Self {
        Self {
            enabled: Self::INCOMPLETE_TERMINAL
                | Self::FAILED_TERMINAL
                | Self::DONE_TERMINAL
                | Self::REASONING_CONTENT_TEXT,
        }
    }

    const fn has(self, capability: u8) -> bool {
        self.enabled & capability != 0
    }
}

/// Closed typed-event allowlist for one Responses dialect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponsesEventPolicy {
    /// Shared OpenAI-compatible documented event surface.
    OpenAiCompatible,
    /// Exact documented OpenRouter Responses beta event surface.
    OpenRouter,
    /// Exact SuperGrok typed surface; every other bounded event is preserved unknown.
    Grok,
}

impl ResponsesEventPolicy {
    fn is_typed(self, kind: &str) -> bool {
        match self {
            Self::OpenAiCompatible => true,
            Self::OpenRouter => matches!(
                kind,
                "response.created"
                    | "response.in_progress"
                    | "response.output_item.added"
                    | "response.output_item.done"
                    | "response.content_part.added"
                    | "response.content_part.delta"
                    | "response.content_part.done"
                    | "response.output_text.delta"
                    | "response.output_text.done"
                    | "response.refusal.delta"
                    | "response.refusal.done"
                    | "response.reasoning_text.delta"
                    | "response.reasoning_text.done"
                    | "response.reasoning.delta"
                    | "response.reasoning_summary_part.added"
                    | "response.reasoning_summary_text.delta"
                    | "response.reasoning_summary_text.done"
                    | "response.reasoning_summary_part.done"
                    | "response.function_call_arguments.delta"
                    | "response.function_call_arguments.done"
                    | "response.completed"
                    | "response.incomplete"
                    | "response.failed"
                    | "response.done"
                    | "response.error"
                    | "error"
            ),
            Self::Grok => matches!(
                kind,
                "response.created"
                    | "response.output_item.added"
                    | "response.output_item.done"
                    | "response.content_part.added"
                    | "response.content_part.done"
                    | "response.output_text.delta"
                    | "response.reasoning_summary_text.delta"
                    | "response.function_call_arguments.delta"
                    | "response.function_call_arguments.done"
                    | "response.completed"
                    | "response.incomplete"
                    | "response.failed"
                    | "error"
            ),
        }
    }
}

/// Whether stream JSON rejects duplicate object members before Value decoding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponsesJsonPolicy {
    /// Standard serde JSON decoding.
    Standard,
    /// Reject duplicate members at every nesting depth.
    UniqueMembers,
}

/// Immutable family/dialect construction data for concrete stream decoding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponsesStreamDialectData {
    profile: ResponsesDialectProfile,
    lifecycle: LifecycleDialectPolicy,
    expected_model: String,
    usage_policy: ResponsesUsagePolicy,
    response_limits: ResponsesResponseLimits,
    capabilities: ResponsesStreamCapabilities,
    event_policy: ResponsesEventPolicy,
    error_policy: ResponsesProviderErrorPolicy,
    json_policy: ResponsesJsonPolicy,
    typed_terminal_cutoff: bool,
}

impl ResponsesStreamDialectData {
    /// Builds explicit data for one admitted Responses dialect.
    #[must_use]
    pub fn new(
        profile: ResponsesDialectProfile,
        lifecycle: LifecycleDialectPolicy,
        expected_model: impl Into<String>,
        usage_policy: ResponsesUsagePolicy,
        response_limits: ResponsesResponseLimits,
        capabilities: ResponsesStreamCapabilities,
    ) -> Self {
        Self {
            profile,
            lifecycle,
            expected_model: expected_model.into(),
            usage_policy,
            response_limits,
            capabilities,
            event_policy: ResponsesEventPolicy::OpenAiCompatible,
            error_policy: ResponsesProviderErrorPolicy::OpenAiCompatible,
            json_policy: ResponsesJsonPolicy::Standard,
            typed_terminal_cutoff: false,
        }
    }

    /// Stops reading the body at the first valid typed terminal event and
    /// ignores any bytes after it, as the Codex and xAI reference clients do.
    #[must_use]
    pub const fn with_typed_terminal_cutoff(mut self) -> Self {
        self.typed_terminal_cutoff = true;
        self
    }

    /// Codex subscription Responses data under an already admitted model.
    #[must_use]
    pub fn codex(expected_model: impl Into<String>) -> Self {
        let mut data = Self::new(
            ResponsesDialectProfile::openai_platform(),
            LifecycleDialectPolicy::openai_codex(),
            expected_model,
            ResponsesUsagePolicy::DeriveTotalWhenOmitted,
            ResponsesResponseLimits::default(),
            ResponsesStreamCapabilities::codex(),
        );
        data.typed_terminal_cutoff = true;
        data
    }

    /// SuperGrok subscription data under an already admitted model.
    #[must_use]
    pub fn grok(expected_model: impl Into<String>) -> Self {
        let mut data = Self::new(
            ResponsesDialectProfile::xai(),
            LifecycleDialectPolicy::grok(),
            expected_model,
            ResponsesUsagePolicy::RequireTotal,
            ResponsesResponseLimits::default(),
            ResponsesStreamCapabilities::grok(),
        );
        data.event_policy = ResponsesEventPolicy::Grok;
        data.error_policy = ResponsesProviderErrorPolicy::Grok;
        data.json_policy = ResponsesJsonPolicy::UniqueMembers;
        data.typed_terminal_cutoff = true;
        data
    }

    /// OpenRouter stateless Responses beta data under an exact admitted model.
    #[must_use]
    pub fn openrouter(expected_model: impl Into<String>) -> Self {
        let mut data = Self::new(
            ResponsesDialectProfile::openrouter(),
            LifecycleDialectPolicy::openrouter(),
            expected_model,
            ResponsesUsagePolicy::RequireTotal,
            ResponsesResponseLimits::default(),
            ResponsesStreamCapabilities::openrouter(),
        );
        data.event_policy = ResponsesEventPolicy::OpenRouter;
        data.error_policy = ResponsesProviderErrorPolicy::OpenRouter;
        data.json_policy = ResponsesJsonPolicy::UniqueMembers;
        data
    }

    /// Replaces response/decode bounds without changing dialect semantics.
    #[must_use]
    pub fn with_response_limits(mut self, limits: ResponsesResponseLimits) -> Self {
        self.response_limits = limits;
        self
    }

    /// Returns the exact aggregate usage policy.
    #[must_use]
    pub const fn usage_policy(&self) -> ResponsesUsagePolicy {
        self.usage_policy
    }

    pub(crate) const fn typed_terminal_cutoff(&self) -> bool {
        self.typed_terminal_cutoff
    }

    /// Returns how provider-reported response model identity is bound.
    #[must_use]
    pub const fn model_binding(&self) -> plakous_responses::ResponseModelBinding {
        self.lifecycle.model_binding
    }

    pub(crate) fn execution_identity(&self) -> Option<Digest> {
        let mut owned = Vec::new();
        let sequence = match self.profile.sequence() {
            SequencePolicy::RequiredStrict => b"required-strict".as_slice(),
            SequencePolicy::OptionalMonotonic { fields } => {
                owned.extend(fields.iter().map(|field| field.as_bytes().to_vec()));
                b"optional-monotonic".as_slice()
            }
            _ => return None,
        };
        let terminal = match self.profile.terminal() {
            TerminalPolicy::TypedOnly => b"typed-only".as_slice(),
            TerminalPolicy::TypedThenDone => b"typed-then-done".as_slice(),
            TerminalPolicy::TypedOptionalDone => b"typed-optional-done".as_slice(),
            _ => return None,
        };
        let finish = match self.profile.finish() {
            FinishPolicy::BlankLineClosed => b"blank-line-closed".as_slice(),
            FinishPolicy::AutoDispatchResidual => b"auto-dispatch-residual".as_slice(),
            _ => return None,
        };
        let bom = match self.profile.bom() {
            BomPolicy::Strip => b"strip".as_slice(),
            BomPolicy::Ignore => b"ignore".as_slice(),
            _ => return None,
        };
        let reasoning = match self.lifecycle.reasoning {
            ReasoningLifecycleMode::DeltaOnly => b"delta-only".as_slice(),
            ReasoningLifecycleMode::FullParts => b"full-parts".as_slice(),
            _ => return None,
        };
        let model_binding = match self.lifecycle.model_binding {
            plakous_responses::ResponseModelBinding::ExactRequest => b"exact-request".as_slice(),
            plakous_responses::ResponseModelBinding::ProviderReported => {
                b"provider-reported".as_slice()
            }
        };
        let usage = match self.usage_policy {
            ResponsesUsagePolicy::RequireTotal => b"require-total".as_slice(),
            ResponsesUsagePolicy::DeriveTotalWhenOmitted => b"derive-total".as_slice(),
        };
        let lifecycle = [
            u8::from(self.lifecycle.require_created_id),
            u8::from(self.lifecycle.require_created_model),
            u8::from(self.lifecycle.content_text_done),
            u8::from(self.lifecycle.reasoning_content_text),
            u8::from(self.lifecycle.intermediate_response_id),
            u8::from(self.lifecycle.strict_terminal_output),
            u8::from(self.lifecycle.reasoning_terminal_ciphertext_refresh),
            u8::from(self.lifecycle.sparse_terminal_model_from_prepared),
            u8::from(self.lifecycle.sparse_terminal_output_from_completed),
            u8::from(self.lifecycle.incomplete_length_text_snapshot),
            u8::from(self.typed_terminal_cutoff),
        ];
        let response_bytes = self.response_limits.max_response_bytes().to_be_bytes();
        let output_items = self.response_limits.max_output_items().to_be_bytes();
        let unknown_bytes = self.response_limits.max_unknown_bytes().to_be_bytes();
        let capabilities = [self.capabilities.enabled];
        let event_policy = match self.event_policy {
            ResponsesEventPolicy::OpenAiCompatible => b"openai-compatible".as_slice(),
            ResponsesEventPolicy::OpenRouter => b"openrouter".as_slice(),
            ResponsesEventPolicy::Grok => b"grok".as_slice(),
        };
        let error_policy = match self.error_policy {
            ResponsesProviderErrorPolicy::OpenAiCompatible => b"openai-compatible".as_slice(),
            ResponsesProviderErrorPolicy::OpenRouter => b"openrouter".as_slice(),
            ResponsesProviderErrorPolicy::Grok => b"grok".as_slice(),
        };
        let json_policy = match self.json_policy {
            ResponsesJsonPolicy::Standard => b"standard".as_slice(),
            ResponsesJsonPolicy::UniqueMembers => b"unique-members".as_slice(),
        };
        let mut parts = vec![
            b"responses-stream-dialect-v3".as_slice(),
            sequence,
            terminal,
            finish,
            bom,
            reasoning,
            model_binding,
            usage,
            lifecycle.as_slice(),
            self.expected_model.as_bytes(),
            response_bytes.as_slice(),
            output_items.as_slice(),
            unknown_bytes.as_slice(),
            capabilities.as_slice(),
            event_policy,
            error_policy,
            json_policy,
        ];
        parts.extend(owned.iter().map(Vec::as_slice));
        Digest::sha256(DigestDomain::ModelBinding, 1, &parts).ok()
    }
}

/// Concrete Responses stream host failure (redaction-safe and provider-neutral).
#[derive(Clone, Copy, Debug, Eq, thiserror::Error, PartialEq)]
#[non_exhaustive]
pub enum ResponsesFamilyStreamError {
    /// Invalid UTF-8 in SSE framing.
    #[error("invalid UTF-8 in Responses stream")]
    InvalidUtf8,
    /// Malformed SSE framing.
    #[error("malformed Responses SSE framing")]
    MalformedSse,
    /// Malformed JSON payload.
    #[error("malformed Responses JSON event")]
    InvalidJson,
    /// A required or documented field was invalid.
    #[error("invalid Responses event field: {0}")]
    InvalidField(&'static str),
    /// Event name and JSON type differed.
    #[error("Responses SSE event name does not match JSON type")]
    EventNameMismatch,
    /// Sequence policy failed.
    #[error("Responses sequence policy failed")]
    InvalidSequence,
    /// Shared item/content/tool lifecycle failed.
    #[error("Responses lifecycle failed")]
    InvalidLifecycle,
    /// Shared lifecycle failed with one static redaction-safe reason.
    #[error("Responses lifecycle failed: {0}")]
    Lifecycle(&'static str),
    /// The response stopped at its output bound with an item still open, so
    /// the item never became output.
    #[error("Responses stream stopped at the output bound with an unfinished item")]
    OutputBoundReached,
    /// Event arrived after a typed terminal.
    #[error("Responses event arrived after terminal")]
    EventAfterTerminal,
    /// EOF arrived inside an unfinished frame.
    #[error("truncated Responses SSE frame")]
    Truncated,
    /// Required typed terminal was absent.
    #[error("Responses stream omitted a typed terminal")]
    MissingTerminal,
    /// Required trailing DONE marker was absent or invalid.
    #[error("Responses DONE marker policy failed")]
    DonePolicy,
    /// A declared event surface was not admitted.
    #[error("Responses event surface is not admitted: {0}")]
    Unsupported(&'static str),
    /// A configured resource bound was exceeded.
    #[error("Responses stream resource limit exceeded: {0}")]
    ResourceLimit(&'static str),
    /// Decoder is already terminally failed.
    #[error("Responses decoder already failed")]
    DecoderFailed,
}

impl From<ResponsesFieldError> for ResponsesFamilyStreamError {
    fn from(error: ResponsesFieldError) -> Self {
        match error {
            ResponsesFieldError::Missing { field } | ResponsesFieldError::Invalid { field } => {
                Self::InvalidField(field)
            }
            _ => Self::InvalidField("unknown Responses field"),
        }
    }
}

impl From<ResponsesHostDecodeError> for ResponsesFamilyStreamError {
    fn from(error: ResponsesHostDecodeError) -> Self {
        match error {
            ResponsesHostDecodeError::MissingField(field)
            | ResponsesHostDecodeError::InvalidField(field) => Self::InvalidField(field),
            ResponsesHostDecodeError::ResourceLimit(resource) => Self::ResourceLimit(resource),
            ResponsesHostDecodeError::InvalidUsageAccounting => {
                Self::InvalidField("usage accounting")
            }
        }
    }
}

impl ResponsesStreamHostError for ResponsesFamilyStreamError {
    fn invalid_utf8() -> Self {
        Self::InvalidUtf8
    }

    fn resource_limit_frame(_maximum: usize) -> Self {
        Self::ResourceLimit("SSE frame bytes")
    }

    fn resource_limit(resource: &'static str, _maximum: usize) -> Self {
        Self::ResourceLimit(resource)
    }

    fn decoder_failed() -> Self {
        Self::DecoderFailed
    }

    fn malformed_sse() -> Self {
        Self::MalformedSse
    }

    fn duplicate_or_reordered(_previous: u64, _sequence: u64) -> Self {
        Self::InvalidSequence
    }

    fn sequence_gap(_expected: u64, _sequence: u64) -> Self {
        Self::InvalidSequence
    }

    fn inconsistent_sequence_presence() -> Self {
        Self::InvalidSequence
    }

    fn done_forbidden() -> Self {
        Self::DonePolicy
    }

    fn event_after_terminal() -> Self {
        Self::EventAfterTerminal
    }

    fn truncated() -> Self {
        Self::Truncated
    }

    fn missing_terminal() -> Self {
        Self::MissingTerminal
    }

    fn missing_done() -> Self {
        Self::DonePolicy
    }

    fn attempt_not_admitted() -> Self {
        Self::InvalidLifecycle
    }

    fn from_field_error(error: ResponsesFieldError) -> Self {
        error.into()
    }
}

/// Concrete typed event emitted by [`ResponsesFamilySseDecoder`].
pub type ResponsesFamilyStreamEvent =
    ResponsesStreamEventKind<ResponsesResponse, ResponsesProviderError>;

struct ResponsesFamilyHooks {
    data: ResponsesStreamDialectData,
    lifecycle: ResponsesLifecycle,
}

impl ResponsesFamilyHooks {
    fn new(data: ResponsesStreamDialectData) -> Self {
        let lifecycle = ResponsesLifecycle::new(data.lifecycle);
        Self { data, lifecycle }
    }

    fn correlated_item_id(
        &self,
        object: &Map<String, Value>,
        output_index: u32,
    ) -> Result<Option<String>, ResponsesFamilyStreamError> {
        let item_id = plakous_responses::optional_provider_label(object, "item_id")?;
        if self.data.event_policy == ResponsesEventPolicy::OpenRouter && item_id.is_none() {
            return Err(ResponsesFamilyStreamError::InvalidField("item_id"));
        }
        self.lifecycle
            .validate_output_item_id(output_index, item_id)
            .map_err(map_lifecycle_error)?;
        Ok(item_id.map(str::to_owned))
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one linear host step whose ordering is the invariant"
    )]
    fn parse_event(
        &mut self,
        value: &Value,
    ) -> Result<ResponsesFamilyStreamEvent, ResponsesFamilyStreamError> {
        let object = value
            .as_object()
            .ok_or(ResponsesFamilyStreamError::InvalidField("event"))?;
        let kind = required_provider_label(object, "type")?;
        self.lifecycle
            .validate_intermediate_response_id(object, kind)
            .map_err(map_lifecycle_error)?;
        if !self.data.event_policy.is_typed(kind) {
            if !self
                .data
                .capabilities
                .has(ResponsesStreamCapabilities::RETAIN_UNKNOWN)
            {
                return Err(ResponsesFamilyStreamError::Unsupported("unknown event"));
            }
            if value.to_string().len() > self.data.response_limits.max_unknown_bytes() {
                return Err(ResponsesFamilyStreamError::ResourceLimit(
                    "unknown event bytes",
                ));
            }
            return Ok(ResponsesStreamEventKind::Unknown {
                kind: kind.to_owned(),
                native: value.clone(),
            });
        }
        let event = match kind {
            "response.created" => {
                let response = required_object(object, "response")?;
                ResponsesStreamEventKind::ResponseCreated {
                    response_id: plakous_responses::optional_provider_label(response, "id")?
                        .map(str::to_owned),
                    model: plakous_responses::optional_provider_label(response, "model")?
                        .map(str::to_owned),
                }
            }
            "response.in_progress" => {
                let response = required_object(object, "response")?;
                let response_id = required_provider_label(response, "id")?;
                let model = required_provider_label(response, "model")?;
                if self.lifecycle.created_id() != Some(response_id)
                    || self.lifecycle.created_model() != Some(model)
                {
                    return Err(ResponsesFamilyStreamError::InvalidLifecycle);
                }
                ResponsesStreamEventKind::ResponseInProgress {
                    response_id: response_id.to_owned(),
                    model: model.to_owned(),
                }
            }
            "response.output_item.added" | "response.output_item.done" => {
                let output_index = required_output_index::<ResponsesFamilyStreamError>(
                    object,
                    ResponsesDecodeLimits::new(
                        self.data.response_limits.max_unknown_bytes(),
                        self.data.response_limits.max_response_bytes(),
                        usize::MAX,
                        self.data.response_limits.max_output_items(),
                    )
                    .map_err(|_| ResponsesFamilyStreamError::ResourceLimit("decode limits"))?,
                )?;
                let item = parse_output_item::<ResponsesHostDecodeError>(
                    object
                        .get("item")
                        .ok_or(ResponsesFamilyStreamError::InvalidField("item"))?,
                )?;
                validate_bounded_unknown_item(&item, self.data.response_limits)?;
                if matches!(item, ResponsesOutputItem::CustomToolCall { .. })
                    && !self
                        .data
                        .capabilities
                        .has(ResponsesStreamCapabilities::CUSTOM_TOOLS)
                {
                    return Err(ResponsesFamilyStreamError::Unsupported("custom tools"));
                }
                if kind.ends_with("added") {
                    ResponsesStreamEventKind::OutputItemAdded { output_index, item }
                } else {
                    ResponsesStreamEventKind::OutputItemDone { output_index, item }
                }
            }
            "response.content_part.added" | "response.content_part.done" => {
                let output_index = output_index(object, self.data.response_limits)?;
                let _ = self.correlated_item_id(object, output_index)?;
                let content_index = required_u32(object, "content_index")?;
                let part = object
                    .get("part")
                    .filter(|part| part.is_object())
                    .ok_or(ResponsesFamilyStreamError::InvalidField("part"))?
                    .clone();
                if part.get("type").and_then(Value::as_str) == Some("reasoning_text")
                    && !self
                        .data
                        .capabilities
                        .has(ResponsesStreamCapabilities::REASONING_CONTENT_TEXT)
                {
                    return Err(ResponsesFamilyStreamError::Unsupported(
                        "reasoning content text",
                    ));
                }
                if !matches!(
                    part.get("type").and_then(Value::as_str),
                    Some("output_text" | "refusal")
                ) && part.to_string().len() > self.data.response_limits.max_unknown_bytes()
                {
                    return Err(ResponsesFamilyStreamError::ResourceLimit(
                        "unknown content part bytes",
                    ));
                }
                if kind.ends_with("added") {
                    ResponsesStreamEventKind::ContentPartAdded {
                        output_index,
                        content_index,
                        part,
                    }
                } else {
                    ResponsesStreamEventKind::ContentPartDone {
                        output_index,
                        content_index,
                        part,
                    }
                }
            }
            "response.content_part.delta"
                if self.data.event_policy == ResponsesEventPolicy::OpenRouter =>
            {
                let output_index = output_index(object, self.data.response_limits)?;
                let _ = self.correlated_item_id(object, output_index)?;
                let content_index = required_u32(object, "content_index")?;
                let delta = required_string(object, "delta")?.to_owned();
                match self
                    .lifecycle
                    .content_part_kind(output_index, content_index)
                    .map_err(map_lifecycle_error)?
                {
                    "output_text" => ResponsesStreamEventKind::OutputTextDelta {
                        output_index,
                        content_index,
                        delta,
                    },
                    "refusal" => ResponsesStreamEventKind::RefusalDelta {
                        output_index,
                        content_index,
                        delta,
                    },
                    _ => return Err(ResponsesFamilyStreamError::InvalidLifecycle),
                }
            }
            "response.output_text.delta" => {
                let output_index = output_index(object, self.data.response_limits)?;
                let _ = self.correlated_item_id(object, output_index)?;
                ResponsesStreamEventKind::OutputTextDelta {
                    output_index,
                    content_index: required_u32(object, "content_index")?,
                    delta: required_string(object, "delta")?.to_owned(),
                }
            }
            "response.output_text.done" => {
                let output_index = output_index(object, self.data.response_limits)?;
                let _ = self.correlated_item_id(object, output_index)?;
                ResponsesStreamEventKind::OutputTextDone {
                    output_index,
                    content_index: required_u32(object, "content_index")?,
                    text: required_string(object, "text")?.to_owned(),
                }
            }
            "response.refusal.delta" => {
                let output_index = output_index(object, self.data.response_limits)?;
                let _ = self.correlated_item_id(object, output_index)?;
                ResponsesStreamEventKind::RefusalDelta {
                    output_index,
                    content_index: required_u32(object, "content_index")?,
                    delta: required_string(object, "delta")?.to_owned(),
                }
            }
            "response.refusal.done" => {
                let output_index = output_index(object, self.data.response_limits)?;
                let _ = self.correlated_item_id(object, output_index)?;
                ResponsesStreamEventKind::RefusalDone {
                    output_index,
                    content_index: required_u32(object, "content_index")?,
                    refusal: required_string(object, "refusal")?.to_owned(),
                }
            }
            "response.reasoning_summary_part.added" | "response.reasoning_summary_part.done" => {
                let output_index = output_index(object, self.data.response_limits)?;
                let _ = self.correlated_item_id(object, output_index)?;
                let summary_index = required_u32(object, "summary_index")?;
                let part = object
                    .get("part")
                    .filter(|part| part.is_object())
                    .ok_or(ResponsesFamilyStreamError::InvalidField("part"))?
                    .clone();
                if kind.ends_with("added") {
                    ResponsesStreamEventKind::ReasoningSummaryPartAdded {
                        output_index,
                        summary_index,
                        part,
                    }
                } else {
                    ResponsesStreamEventKind::ReasoningSummaryPartDone {
                        output_index,
                        summary_index,
                        part,
                    }
                }
            }
            "response.reasoning_summary_text.delta" => {
                let output_index = output_index(object, self.data.response_limits)?;
                let _ = self.correlated_item_id(object, output_index)?;
                ResponsesStreamEventKind::ReasoningSummaryTextDelta {
                    output_index,
                    summary_index: required_u32(object, "summary_index")?,
                    delta: required_string(object, "delta")?.to_owned(),
                }
            }
            "response.reasoning_text.delta" | "response.reasoning_text.done"
                if self
                    .data
                    .capabilities
                    .has(ResponsesStreamCapabilities::REASONING_CONTENT_TEXT) =>
            {
                let output_index = output_index(object, self.data.response_limits)?;
                let content_index = required_u32(object, "content_index")?;
                let item_id = self.correlated_item_id(object, output_index)?;
                if kind.ends_with("delta") {
                    ResponsesStreamEventKind::ReasoningTextDelta {
                        output_index,
                        content_index,
                        item_id,
                        delta: required_string(object, "delta")?.to_owned(),
                    }
                } else {
                    ResponsesStreamEventKind::ReasoningTextDone {
                        output_index,
                        content_index,
                        item_id,
                        text: required_string(object, "text")?.to_owned(),
                    }
                }
            }
            "response.reasoning.delta"
                if self.data.event_policy == ResponsesEventPolicy::OpenRouter =>
            {
                let output_index = output_index(object, self.data.response_limits)?;
                let _ = self.correlated_item_id(object, output_index)?;
                ResponsesStreamEventKind::ReasoningSummaryTextDelta {
                    output_index,
                    summary_index: object
                        .get("summary_index")
                        .map_or(Ok(0), |_| required_u32(object, "summary_index"))?,
                    delta: required_string(object, "delta")?.to_owned(),
                }
            }
            "response.reasoning_summary_text.done" => {
                let output_index = output_index(object, self.data.response_limits)?;
                let _ = self.correlated_item_id(object, output_index)?;
                ResponsesStreamEventKind::ReasoningSummaryTextDone {
                    output_index,
                    summary_index: required_u32(object, "summary_index")?,
                    text: required_string(object, "text")?.to_owned(),
                }
            }
            "response.function_call_arguments.delta" => {
                let output_index = output_index(object, self.data.response_limits)?;
                ResponsesStreamEventKind::FunctionCallArgumentsDelta {
                    output_index,
                    item_id: self.correlated_item_id(object, output_index)?,
                    delta: required_string(object, "delta")?.to_owned(),
                }
            }
            "response.function_call_arguments.done" => {
                let output_index = output_index(object, self.data.response_limits)?;
                ResponsesStreamEventKind::FunctionCallArgumentsDone {
                    output_index,
                    item_id: self.correlated_item_id(object, output_index)?,
                    arguments: required_string(object, "arguments")?.to_owned(),
                }
            }
            "response.custom_tool_call_input.delta" => {
                if !self
                    .data
                    .capabilities
                    .has(ResponsesStreamCapabilities::CUSTOM_TOOLS)
                {
                    return Err(ResponsesFamilyStreamError::Unsupported("custom tools"));
                }
                let output_index = output_index(object, self.data.response_limits)?;
                ResponsesStreamEventKind::CustomToolCallInputDelta {
                    output_index,
                    item_id: self.correlated_item_id(object, output_index)?,
                    delta: required_string(object, "delta")?.to_owned(),
                }
            }
            "response.custom_tool_call_input.done" => {
                if !self
                    .data
                    .capabilities
                    .has(ResponsesStreamCapabilities::CUSTOM_TOOLS)
                {
                    return Err(ResponsesFamilyStreamError::Unsupported("custom tools"));
                }
                let output_index = output_index(object, self.data.response_limits)?;
                ResponsesStreamEventKind::CustomToolCallInputDone {
                    output_index,
                    item_id: self.correlated_item_id(object, output_index)?,
                    input: required_string(object, "input")?.to_owned(),
                }
            }
            "response.output_text.annotation.added" => {
                if !self
                    .data
                    .capabilities
                    .has(ResponsesStreamCapabilities::CITATIONS)
                {
                    return Err(ResponsesFamilyStreamError::Unsupported("citations"));
                }
                let annotation = parse_annotation::<ResponsesHostDecodeError>(
                    object
                        .get("annotation")
                        .ok_or(ResponsesFamilyStreamError::InvalidField("annotation"))?,
                )?;
                if matches!(
                    &annotation,
                    ResponsesAnnotation::Opaque { native, .. }
                        if native.to_string().len()
                            > self.data.response_limits.max_unknown_bytes()
                ) {
                    return Err(ResponsesFamilyStreamError::ResourceLimit(
                        "unknown annotation bytes",
                    ));
                }
                let output_index = output_index(object, self.data.response_limits)?;
                let _ = self.correlated_item_id(object, output_index)?;
                ResponsesStreamEventKind::OutputTextAnnotationAdded {
                    output_index,
                    content_index: required_u32(object, "content_index")?,
                    annotation_index: required_u32(object, "annotation_index")?,
                    annotation,
                }
            }
            "response.completed" | "response.incomplete" | "response.failed" | "response.done" => {
                self.parse_terminal(kind, object)?
            }
            "response.error" if self.data.event_policy == ResponsesEventPolicy::OpenRouter => {
                let payload = object.get("error").unwrap_or(value);
                let request_id = plakous_responses::optional_provider_label(object, "request_id")?;
                let canonical_error_type =
                    plakous_responses::optional_provider_label(object, "error_type")?;
                ResponsesStreamEventKind::Error(
                    parse_provider_error_with_canonical_type(
                        payload,
                        self.data.response_limits,
                        self.data.error_policy,
                        canonical_error_type,
                    )?
                    .with_request_id(request_id)?,
                )
            }
            "error" => {
                let payload = object.get("error").unwrap_or(value);
                let request_id = plakous_responses::optional_provider_label(object, "request_id")?;
                let canonical_error_type =
                    if self.data.error_policy == ResponsesProviderErrorPolicy::OpenRouter {
                        plakous_responses::optional_provider_label(object, "error_type")?
                    } else {
                        None
                    };
                ResponsesStreamEventKind::Error(
                    parse_provider_error_with_canonical_type(
                        payload,
                        self.data.response_limits,
                        self.data.error_policy,
                        canonical_error_type,
                    )?
                    .with_request_id(request_id)?,
                )
            }
            _ => {
                if kind.starts_with("response.custom_tool_call")
                    || kind.starts_with("response.function_call_arguments")
                    || kind.starts_with("response.output_text.annotation")
                {
                    return Err(ResponsesFamilyStreamError::Unsupported(
                        "declared event subvariant",
                    ));
                }
                if !self
                    .data
                    .capabilities
                    .has(ResponsesStreamCapabilities::RETAIN_UNKNOWN)
                {
                    return Err(ResponsesFamilyStreamError::Unsupported("unknown event"));
                }
                if value.to_string().len() > self.data.response_limits.max_unknown_bytes() {
                    return Err(ResponsesFamilyStreamError::ResourceLimit(
                        "unknown event bytes",
                    ));
                }
                ResponsesStreamEventKind::Unknown {
                    kind: kind.to_owned(),
                    native: value.clone(),
                }
            }
        };
        if !matches!(
            event,
            ResponsesStreamEventKind::ResponseCompleted(_)
                | ResponsesStreamEventKind::ResponseIncomplete(_)
                | ResponsesStreamEventKind::ResponseFailed(_)
                | ResponsesStreamEventKind::Error(_)
        ) {
            event
                .apply_intermediate_lifecycle(
                    &mut self.lifecycle,
                    object,
                    &self.data.expected_model,
                )
                .map_err(map_lifecycle_error)?;
        }
        Ok(event)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one linear host step whose ordering is the invariant"
    )]
    fn parse_terminal(
        &self,
        event_kind: &str,
        object: &Map<String, Value>,
    ) -> Result<ResponsesFamilyStreamEvent, ResponsesFamilyStreamError> {
        let response_value = object
            .get("response")
            .ok_or(ResponsesFamilyStreamError::InvalidField("response"))?;
        let mut reconciled_response = response_value.clone();
        let response_object = reconciled_response
            .as_object_mut()
            .ok_or(ResponsesFamilyStreamError::InvalidField("response"))?;
        if !response_object.contains_key("model")
            && self.data.lifecycle.sparse_terminal_model_from_prepared
        {
            response_object.insert(
                "model".to_owned(),
                Value::String(self.data.expected_model.clone()),
            );
        }
        if !response_object.contains_key("output")
            && self.data.lifecycle.sparse_terminal_output_from_completed
        {
            let output = self
                .lifecycle
                .synthesize_missing_output()
                .map_err(map_lifecycle_error)?;
            response_object.insert("output".to_owned(), Value::Array(output));
        }
        let mut response = ResponsesResponse::parse_reconciled_with_native(
            &reconciled_response,
            response_value,
            self.data.usage_policy,
            self.data.error_policy,
            self.data.response_limits,
        )?;
        if self.data.lifecycle.model_binding
            == plakous_responses::ResponseModelBinding::ExactRequest
            && response.model() != self.data.expected_model
        {
            return Err(ResponsesFamilyStreamError::InvalidLifecycle);
        }
        let (expected_status, allow) = match event_kind {
            "response.completed" => ("completed", true),
            "response.done" => (
                "completed",
                self.data
                    .capabilities
                    .has(ResponsesStreamCapabilities::DONE_TERMINAL),
            ),
            "response.incomplete" => (
                "incomplete",
                self.data
                    .capabilities
                    .has(ResponsesStreamCapabilities::INCOMPLETE_TERMINAL),
            ),
            "response.failed" => (
                "failed",
                self.data
                    .capabilities
                    .has(ResponsesStreamCapabilities::FAILED_TERMINAL),
            ),
            _ => return Err(ResponsesFamilyStreamError::InvalidLifecycle),
        };
        if !allow {
            return Err(ResponsesFamilyStreamError::Unsupported("terminal status"));
        }
        let actual_status = match response.status() {
            ResponsesResponseStatus::Completed => "completed",
            ResponsesResponseStatus::Incomplete => "incomplete",
            ResponsesResponseStatus::Failed => "failed",
            _ => return Err(ResponsesFamilyStreamError::InvalidLifecycle),
        };
        let output = response
            .output()
            .iter()
            .map(ResponsesOutputItem::native)
            .collect::<Vec<_>>();
        self.lifecycle
            .terminal_response(
                response.id(),
                Some(response.model()),
                expected_status,
                actual_status,
                response
                    .incomplete_details()
                    .map(crate::ResponsesIncompleteDetails::native),
                response.error().is_some(),
                &output,
            )
            .map_err(map_lifecycle_error)?;
        if !self.data.lifecycle.strict_terminal_output {
            let streamed = self
                .lifecycle
                .synthesize_missing_output()
                .map_err(map_lifecycle_error)?;
            if !streamed.is_empty() {
                response = response
                    .with_authoritative_stream_output(&streamed, self.data.response_limits)?;
            }
        }
        Ok(match response.status() {
            ResponsesResponseStatus::Completed => {
                ResponsesStreamEventKind::ResponseCompleted(response)
            }
            ResponsesResponseStatus::Incomplete => {
                ResponsesStreamEventKind::ResponseIncomplete(response)
            }
            ResponsesResponseStatus::Failed => ResponsesStreamEventKind::ResponseFailed(response),
            _ => return Err(ResponsesFamilyStreamError::InvalidLifecycle),
        })
    }
}

impl DialectStreamHooks for ResponsesFamilyHooks {
    type Event = ResponsesFamilyStreamEvent;
    type Error = ResponsesFamilyStreamError;

    fn map_sse_error(error: SseError, limits: &ResponsesStreamLimits) -> Self::Error {
        map_sse_error_standard(error, limits)
    }

    fn map_sequence_error(error: SequenceValidationError) -> Self::Error {
        map_sequence_error_standard(error)
    }

    fn map_done_forbidden() -> Self::Error {
        ResponsesFamilyStreamError::DonePolicy
    }

    fn map_event_after_terminal() -> Self::Error {
        ResponsesFamilyStreamError::EventAfterTerminal
    }

    fn map_truncated() -> Self::Error {
        ResponsesFamilyStreamError::Truncated
    }

    fn map_missing_terminal() -> Self::Error {
        ResponsesFamilyStreamError::MissingTerminal
    }

    fn map_missing_done() -> Self::Error {
        ResponsesFamilyStreamError::DonePolicy
    }

    fn map_resource_limit(resource: &'static str, _maximum: usize) -> Self::Error {
        ResponsesFamilyStreamError::ResourceLimit(resource)
    }

    fn map_field_error(error: ResponsesFieldError) -> Self::Error {
        error.into()
    }

    fn event_is_terminal(event: &Self::Event) -> bool {
        event.is_terminal()
    }

    fn decode_json_event(
        &mut self,
        data: &str,
        event_name: Option<&str>,
        _sequence: Option<u64>,
    ) -> Result<Option<Self::Event>, Self::Error> {
        let value: Value = match self.data.json_policy {
            ResponsesJsonPolicy::Standard => serde_json::from_str(data),
            ResponsesJsonPolicy::UniqueMembers => parse_unique_json_value(data),
        }
        .map_err(|_| ResponsesFamilyStreamError::InvalidJson)?;
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or(ResponsesFamilyStreamError::InvalidField("type"))?;
        if event_name.is_some_and(|event_name| event_name != kind) {
            return Err(ResponsesFamilyStreamError::EventNameMismatch);
        }
        self.parse_event(&value).map(Some)
    }
}

/// Concrete non-cloning Responses SSE decoder used by Codex-family migration.
pub struct ResponsesFamilySseDecoder {
    kernel: Option<ResponsesStreamKernel>,
    hooks: ResponsesFamilyHooks,
    failed: bool,
}

impl ResponsesFamilySseDecoder {
    /// Creates a decoder from immutable admitted family/dialect data.
    #[must_use]
    pub fn new(data: ResponsesStreamDialectData, limits: ResponsesStreamLimits) -> Self {
        let mut kernel = ResponsesStreamKernel::new(data.profile, limits);
        if data.typed_terminal_cutoff {
            kernel = kernel.with_typed_terminal_cutoff();
        }
        Self {
            kernel: Some(kernel),
            hooks: ResponsesFamilyHooks::new(data),
            failed: false,
        }
    }

    fn with_kernel_mut<T>(
        &mut self,
        operation: impl FnOnce(&mut ResponsesStreamKernel, &mut ResponsesFamilyHooks) -> T,
    ) -> T {
        let Some(mut kernel) = self.kernel.take() else {
            unreachable!("Responses kernel cannot be re-entered");
        };
        let output = operation(&mut kernel, &mut self.hooks);
        self.kernel = Some(kernel);
        output
    }

    /// Feeds arbitrary byte fragments and returns every completed event.
    ///
    /// # Errors
    ///
    /// Returns a terminal family error for malformed, over-limit, or invalid
    /// lifecycle input.
    pub fn push(
        &mut self,
        bytes: &[u8],
    ) -> Result<Vec<ResponsesFamilyStreamEvent>, ResponsesFamilyStreamError> {
        if self.failed {
            return Err(ResponsesFamilyStreamError::DecoderFailed);
        }
        match self.with_kernel_mut(|kernel, hooks| kernel.push(bytes, hooks)) {
            Ok(events) => Ok(events),
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }

    /// Validates EOF and returns auto-dispatched residual events.
    ///
    /// # Errors
    ///
    /// Returns a terminal family error for truncation or missing terminal state.
    pub fn finish(
        &mut self,
    ) -> Result<Vec<ResponsesFamilyStreamEvent>, ResponsesFamilyStreamError> {
        if self.failed {
            return Err(ResponsesFamilyStreamError::DecoderFailed);
        }
        match self.with_kernel_mut(ResponsesStreamKernel::finish) {
            Ok(events) => Ok(events),
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }
}

impl fmt::Debug for ResponsesFamilySseDecoder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponsesFamilySseDecoder")
            .field("kernel", &self.kernel)
            .field("expected_model", &"<redacted>")
            .field("failed", &self.failed)
            .finish_non_exhaustive()
    }
}

fn output_index(
    object: &Map<String, Value>,
    limits: ResponsesResponseLimits,
) -> Result<u32, ResponsesFamilyStreamError> {
    let index = required_u32(object, "output_index")?;
    if usize::try_from(index).map_or(true, |index| index >= limits.max_output_items()) {
        return Err(ResponsesFamilyStreamError::ResourceLimit("output items"));
    }
    Ok(index)
}

fn map_lifecycle_error(error: ResponsesLifecycleError) -> ResponsesFamilyStreamError {
    match error {
        ResponsesLifecycleError::InvalidLifecycle { reason } => {
            ResponsesFamilyStreamError::Lifecycle(reason)
        }
        ResponsesLifecycleError::BindingMismatch => {
            ResponsesFamilyStreamError::Lifecycle("response lifecycle binding mismatch")
        }
        ResponsesLifecycleError::OutputBoundReached => {
            ResponsesFamilyStreamError::OutputBoundReached
        }
        ResponsesLifecycleError::MissingField { field }
        | ResponsesLifecycleError::InvalidField { field } => {
            ResponsesFamilyStreamError::InvalidField(field)
        }
        _ => ResponsesFamilyStreamError::InvalidLifecycle,
    }
}

/// Stream-error factories for shared SSE / sequence maps.
pub trait ResponsesStreamHostError: Sized {
    /// Invalid UTF-8 in an SSE frame.
    fn invalid_utf8() -> Self;
    /// SSE frame byte limit.
    fn resource_limit_frame(maximum: usize) -> Self;
    /// Named resource limit.
    fn resource_limit(resource: &'static str, maximum: usize) -> Self;
    /// Decoder already failed.
    fn decoder_failed() -> Self;
    /// Malformed SSE.
    fn malformed_sse() -> Self;
    /// Duplicate or reordered sequence.
    fn duplicate_or_reordered(previous: u64, sequence: u64) -> Self;
    /// Sequence gap.
    fn sequence_gap(expected: u64, sequence: u64) -> Self;
    /// Sequence presence inconsistency.
    fn inconsistent_sequence_presence() -> Self;
    /// Forbidden / unexpected `[DONE]`.
    fn done_forbidden() -> Self;
    /// Event after terminal.
    fn event_after_terminal() -> Self;
    /// Truncated frame.
    fn truncated() -> Self;
    /// Missing typed terminal.
    fn missing_terminal() -> Self;
    /// Missing required `[DONE]`.
    fn missing_done() -> Self;
    /// Attempt not admitted / not decoding.
    fn attempt_not_admitted() -> Self;
    /// Map field errors.
    fn from_field_error(error: ResponsesFieldError) -> Self;
}

/// Standard SSE framing error map used by OpenRouter / xAI-style dialects.
#[allow(
    clippy::needless_pass_by_value,
    reason = "owned value matches the dialect hook and caller ownership"
)]
pub fn map_sse_error_standard<E: ResponsesStreamHostError>(
    error: SseError,
    limits: &ResponsesStreamLimits,
) -> E {
    match error {
        SseError::InvalidUtf8 => E::invalid_utf8(),
        SseError::ResourceLimit("SSE line bytes" | "SSE event bytes") => {
            E::resource_limit_frame(limits.max_frame_bytes())
        }
        SseError::ResourceLimit(resource) => E::resource_limit(resource, limits.max_frame_bytes()),
        SseError::AlreadyFailed => E::decoder_failed(),
        _ => E::malformed_sse(),
    }
}

/// Standard sequence-policy error map.
pub fn map_sequence_error_standard<E: ResponsesStreamHostError>(
    error: SequenceValidationError,
) -> E {
    match error {
        SequenceValidationError::DuplicateOrReordered { previous, sequence } => {
            E::duplicate_or_reordered(previous, sequence)
        }
        SequenceValidationError::Gap { expected, sequence } => E::sequence_gap(expected, sequence),
        SequenceValidationError::MissingRequired
        | SequenceValidationError::InconsistentPresence
        | _ => E::inconsistent_sequence_presence(),
    }
}

/// Validates `output_index` against decode limits.
///
/// # Errors
///
/// Returns `E` when the field is missing/invalid or exceeds the item budget.
pub fn required_output_index<E>(
    object: &Map<String, Value>,
    limits: ResponsesDecodeLimits,
) -> Result<u32, E>
where
    E: From<ResponsesFieldError> + ResponsesStreamHostError,
{
    let index = required_u32(object, "output_index")?;
    let index_usize = usize::try_from(index).map_err(|_| {
        E::from_field_error(ResponsesFieldError::Invalid {
            field: "output_index",
        })
    })?;
    if index_usize >= limits.max_output_items() {
        return Err(E::resource_limit("output items", limits.max_output_items()));
    }
    Ok(index)
}

/// Admitted SSE decoder front-half around one kernel + hook state.
///
/// A decode failure poisons the operation, so pushes mutate the owned kernel and
/// hooks directly. This avoids cloning all framing, lifecycle, and projection
/// state for every transport fragment.
pub struct ResponsesSseDecoderFront<H> {
    attempt_lifecycle: Arc<AtomicU8>,
    kernel: ResponsesStreamKernel,
    hooks: H,
    failed: bool,
}

impl<H> ResponsesSseDecoderFront<H> {
    /// Builds a front-half decoder.
    #[must_use]
    pub fn new(attempt_lifecycle: Arc<AtomicU8>, kernel: ResponsesStreamKernel, hooks: H) -> Self {
        Self {
            attempt_lifecycle,
            kernel,
            hooks,
            failed: false,
        }
    }

    /// Returns a shared handle to dialect hooks.
    #[must_use]
    pub const fn hooks(&self) -> &H {
        &self.hooks
    }

    /// Returns the stream kernel.
    #[must_use]
    pub const fn kernel(&self) -> &ResponsesStreamKernel {
        &self.kernel
    }

    /// Whether the decoder is terminally failed.
    #[must_use]
    pub const fn failed(&self) -> bool {
        self.failed
    }

    /// Feeds bytes through the kernel under attempt-admission checks.
    ///
    /// # Errors
    ///
    /// Returns dialect stream errors from hooks or admission failures.
    pub fn push<E>(&mut self, chunk: &[u8]) -> Result<Vec<H::Event>, E>
    where
        H: DialectStreamHooks<Error = E>,
        E: ResponsesStreamHostError,
    {
        if self.failed {
            return Err(E::decoder_failed());
        }
        if !is_decoding(&self.attempt_lifecycle) {
            return Err(E::attempt_not_admitted());
        }
        match self.kernel.push(chunk, &mut self.hooks) {
            Ok(events) => Ok(events),
            Err(error) => {
                mark_decode_failed(&self.attempt_lifecycle);
                self.failed = true;
                Err(error)
            }
        }
    }

    /// Finishes the stream, returns residual decoded events, and marks the
    /// attempt decoded on success.
    ///
    /// Auto-dispatch dialects may complete a final SSE frame only at EOF. Those
    /// events are semantic stream output and must be delivered to the caller.
    ///
    /// # Errors
    ///
    /// Returns dialect stream errors from hooks or admission failures.
    pub fn finish_events<E>(mut self) -> Result<Vec<H::Event>, E>
    where
        H: DialectStreamHooks<Error = E>,
        E: ResponsesStreamHostError,
    {
        if self.failed {
            return Err(E::decoder_failed());
        }
        if !is_decoding(&self.attempt_lifecycle) {
            return Err(E::attempt_not_admitted());
        }
        match self.kernel.finish(&mut self.hooks) {
            Ok(residual) => {
                mark_decoded(&self.attempt_lifecycle).map_err(|_| E::attempt_not_admitted())?;
                Ok(residual)
            }
            Err(error) => {
                mark_decode_failed(&self.attempt_lifecycle);
                Err(error)
            }
        }
    }

    /// Finishes a stream whose dialect requires blank-line-closed frames.
    ///
    /// Existing xAI/OpenRouter callers use this compatibility convenience; its
    /// profiles cannot produce residual events. Auto-dispatch dialects must use
    /// [`Self::finish_events`] so EOF-decoded events are not discarded.
    ///
    /// # Errors
    ///
    /// Returns dialect stream errors from hooks or admission failures.
    pub fn finish<E>(self) -> Result<(), E>
    where
        H: DialectStreamHooks<Error = E>,
        E: ResponsesStreamHostError,
    {
        let residual = self.finish_events()?;
        debug_assert!(residual.is_empty());
        Ok(())
    }
}

impl<H> fmt::Debug for ResponsesSseDecoderFront<H> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponsesSseDecoderFront")
            .field("attempt_decoding", &is_decoding(&self.attempt_lifecycle))
            .field("kernel", &self.kernel)
            .field("failed", &self.failed)
            .field("hooks", &"<redacted dialect hooks>")
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests;
