use crate::{
    ResponsesFieldError, ResponsesLifecycleError, ResponsesStreamLimits, SequenceValidationError,
};
use plakous_http::SseError;

use crate::response_host::{ResponsesHostDecodeError, ResponsesProviderError, ResponsesResponse};

use super::ResponsesStreamEventKind;

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

/// Concrete typed event emitted by [`ResponsesFamilySseDecoder`](super::ResponsesFamilySseDecoder).
pub type ResponsesFamilyStreamEvent =
    ResponsesStreamEventKind<ResponsesResponse, ResponsesProviderError>;

pub(super) fn map_lifecycle_error(error: ResponsesLifecycleError) -> ResponsesFamilyStreamError {
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
    }
}

/// Standard SSE framing error map used by OpenRouter / xAI-style dialects.
#[allow(
    clippy::needless_pass_by_value,
    reason = "owned value matches the dialect hook and caller ownership"
)]
pub fn map_sse_error_standard(
    error: SseError,
    _limits: &ResponsesStreamLimits,
) -> ResponsesFamilyStreamError {
    match error {
        SseError::InvalidUtf8 => ResponsesFamilyStreamError::InvalidUtf8,
        SseError::ResourceLimit("SSE line bytes" | "SSE event bytes") => {
            ResponsesFamilyStreamError::ResourceLimit("SSE frame bytes")
        }
        SseError::ResourceLimit(resource) => ResponsesFamilyStreamError::ResourceLimit(resource),
        SseError::AlreadyFailed => ResponsesFamilyStreamError::DecoderFailed,
        _ => ResponsesFamilyStreamError::MalformedSse,
    }
}

/// Standard sequence-policy error map.
pub fn map_sequence_error_standard(_error: SequenceValidationError) -> ResponsesFamilyStreamError {
    ResponsesFamilyStreamError::InvalidSequence
}
