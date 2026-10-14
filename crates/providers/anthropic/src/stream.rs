//! Thin Anthropic binding over the neutral Messages stream projector.

use std::fmt;
use std::sync::Arc;

use plakous_messages::{
    MessagesStreamError, MessagesStreamErrorKind, MessagesStreamErrorPhase,
    MessagesStreamProjector, MessagesTerminalProjection,
};
use plakous_model::{Clock, IdGenerator, ModelEvent, ModelId, ModelProviderRequestId};

use crate::{
    AnthropicError, AnthropicErrorKind, AnthropicErrorPhase, PreparedMessagesRequest, RetrySafety,
    classify_provider_error_with_safety,
};

pub use plakous_http::{SseDecoder, SseLimits, SseMessage};
pub use plakous_messages::{
    MessagesStopDetails, MessagesStopDetailsKind, MessagesStopOutcome, MessagesStopReason,
    MessagesStreamLimits,
};

/// Anthropic attempt-bound wrapper over the neutral Messages projector.
pub struct AnthropicMessagesStreamDecoder {
    inner: MessagesStreamProjector,
}

impl fmt::Debug for AnthropicMessagesStreamDecoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnthropicMessagesStreamDecoder")
            .field("inner", &self.inner)
            .finish()
    }
}

impl AnthropicMessagesStreamDecoder {
    /// Creates a prepared-only decoder.
    pub fn new(
        prepared: PreparedMessagesRequest,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
    ) -> Result<Self, AnthropicError> {
        Self::with_limits(prepared, clock, ids, MessagesStreamLimits::default())
    }

    /// Creates a prepared-only decoder with exact stream limits.
    pub fn with_limits(
        prepared: PreparedMessagesRequest,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
        limits: MessagesStreamLimits,
    ) -> Result<Self, AnthropicError> {
        Ok(Self {
            inner: MessagesStreamProjector::with_limits(
                prepared.stream_binding()?,
                clock,
                ids,
                limits,
            )
            .map_err(map_stream_error)?,
        })
    }

    #[must_use]
    /// Binds validated provider request metadata before decoding.
    pub fn with_provider_request_id(mut self, request_id: ModelProviderRequestId) -> Self {
        self.inner = self.inner.with_provider_request_id(request_id);
        self
    }

    /// Projects the next byte fragment into canonical events.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<ModelEvent>, AnthropicError> {
        self.inner.push(bytes).map_err(map_stream_error)
    }

    /// Seals EOF and requires a successful Messages terminal event.
    pub fn finish_eof(&mut self) -> Result<Vec<ModelEvent>, AnthropicError> {
        self.inner.finish_eof().map_err(map_stream_error)
    }

    /// Returns the exact provider-reported model after message start.
    pub const fn reported_model(&self) -> Option<&ModelId> {
        self.inner.reported_model()
    }
    /// Returns the provider message identity after message start.
    pub fn message_id(&self) -> Option<&str> {
        self.inner.message_id()
    }
    /// Returns the exact typed Messages terminal outcome.
    pub const fn stop_outcome(&self) -> Option<&MessagesStopOutcome> {
        self.inner.stop_outcome()
    }

    /// Returns the complete neutral Messages projection after clean EOF.
    pub fn into_terminal_projection(self) -> Result<MessagesTerminalProjection, AnthropicError> {
        self.inner
            .into_terminal_projection()
            .map_err(map_stream_error)
    }
}

pub(crate) fn map_stream_error(error: MessagesStreamError) -> AnthropicError {
    if error.kind() == MessagesStreamErrorKind::ProviderError {
        return classify_provider_error_with_safety(
            None,
            error.provider_payload().unwrap_or_default(),
            None,
            AnthropicErrorPhase::Stream,
            RetrySafety::new(
                true,
                error.visible_output(),
                error.hosted_effects_possible(),
            ),
        );
    }
    let kind = match error.kind() {
        MessagesStreamErrorKind::InvalidResponse => AnthropicErrorKind::InvalidResponse,
        MessagesStreamErrorKind::InvalidEvent => AnthropicErrorKind::InvalidEvent,
        MessagesStreamErrorKind::InvalidAccounting => AnthropicErrorKind::InvalidAccounting,
        MessagesStreamErrorKind::UnsupportedSemantic => AnthropicErrorKind::UnsupportedSemantic,
        MessagesStreamErrorKind::IncompleteResponse => AnthropicErrorKind::IncompleteResponse,
        MessagesStreamErrorKind::ProviderError => unreachable!(),
    };
    let phase = match error.phase() {
        MessagesStreamErrorPhase::Stream => AnthropicErrorPhase::Stream,
        MessagesStreamErrorPhase::Complete => AnthropicErrorPhase::Complete,
    };
    AnthropicError::new(
        kind,
        phase,
        error.message().replace("Messages", "Anthropic"),
    )
}
