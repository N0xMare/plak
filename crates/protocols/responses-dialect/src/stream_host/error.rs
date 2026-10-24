use plakous_http::SseError;
use plakous_responses::{
    ResponsesFieldError, ResponsesLifecycleError, ResponsesStreamLimits, SequenceValidationError,
};

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
