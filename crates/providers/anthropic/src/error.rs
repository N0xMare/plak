//! Redaction-safe Anthropic protocol errors and provider error mapping.

use std::fmt;

use plakous_model::SafeDiagnostic;
use serde::Deserialize;

/// Stable error categories exposed by the Messages adapter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum AnthropicErrorKind {
    /// Route, credential, model, or profile configuration was invalid.
    InvalidConfiguration,
    /// Canonical input could not be represented without changing semantics.
    UnsupportedSemantic,
    /// The request violated the Messages grammar.
    InvalidRequest,
    /// Native history cannot be replayed on this kernel.
    ProtectedHistory,
    /// SSE framing or event JSON was malformed.
    InvalidResponse,
    /// Provider usage counters could not be normalized exactly.
    InvalidAccounting,
    /// A prepared, decoded, attempt, or terminal operation binding differed.
    BindingMismatch,
    /// A valid JSON event violated the Messages stream state machine.
    InvalidEvent,
    /// EOF arrived before `message_stop`.
    IncompleteResponse,
    /// Paid dispatch accounting may have advanced and requires exact recovery.
    OutcomeUnknown,
    /// Authentication failed.
    Authentication,
    /// Authorization failed.
    Authorization,
    /// The provider rate limited the operation.
    RateLimited,
    /// The provider was overloaded or temporarily unavailable.
    Overloaded,
    /// The provider rejected the request for a context or output limit.
    Limit,
    /// The provider reported another terminal protocol error.
    Provider,
    /// The operation was cancelled.
    Cancelled,
    /// A configured deadline elapsed.
    Timeout,
    /// The bounded event consumer was closed.
    EventSinkClosed,
    /// The provider-blind physical transport failed.
    Transport,
    /// A local transport or decoder resource bound was exceeded.
    ResourceLimit,
}

/// The phase in which an Anthropic operation failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum AnthropicErrorPhase {
    /// Request validation or serialization.
    Prepare,
    /// Connection establishment or HTTP response headers.
    Connect,
    /// Incremental SSE framing and normalization.
    Stream,
    /// Sending a normalized event through bounded backpressure.
    EventDelivery,
    /// Terminal response assembly.
    Complete,
}

/// Retry advice produced by protocol classification, not a retry loop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RetryAdvice {
    /// Repeating the prepared request is not automatically safe.
    Never,
    /// A retry is eligible only when the caller proves the provider did not
    /// accept the request and no output was visible.
    BeforeAcceptance,
    /// A transient provider response is eligible under the operation budget.
    Transient,
}

/// Facts required to classify whether recreating a provider operation is safe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetrySafety {
    provider_accepted: bool,
    visible_output: bool,
    hosted_effects_possible: bool,
}

impl Default for RetrySafety {
    fn default() -> Self {
        Self::new(true, false, false)
    }
}

impl RetrySafety {
    /// Creates explicit provider-acceptance and effect facts.
    #[must_use]
    pub const fn new(
        provider_accepted: bool,
        visible_output: bool,
        hosted_effects_possible: bool,
    ) -> Self {
        Self {
            provider_accepted,
            visible_output,
            hosted_effects_possible,
        }
    }

    /// Returns whether the provider acknowledged the operation.
    #[must_use]
    pub const fn provider_accepted(self) -> bool {
        self.provider_accepted
    }

    /// Returns whether any normalized output became visible.
    #[must_use]
    pub const fn visible_output(self) -> bool {
        self.visible_output
    }

    /// Returns whether provider-hosted external effects may have occurred.
    #[must_use]
    pub const fn hosted_effects_possible(self) -> bool {
        self.hosted_effects_possible
    }
}

/// A typed redaction-safe failure from request translation, streaming, or an attempt.
#[derive(Clone, Eq, PartialEq)]
pub struct AnthropicError {
    kind: AnthropicErrorKind,
    phase: AnthropicErrorPhase,
    message: SafeDiagnostic,
    status: Option<u16>,
    code: Option<String>,
    request_id: Option<String>,
    retry: RetryAdvice,
    retry_after_millis: Option<u64>,
}

impl AnthropicError {
    /// Keeps history-shaped preparation failures actionable.
    pub(crate) const fn preparation_error_kind(&self) -> plakous_model::ModelPreparationErrorKind {
        match self.kind {
            AnthropicErrorKind::ProtectedHistory => {
                plakous_model::ModelPreparationErrorKind::ProtectedHistory
            }
            _ => plakous_model::ModelPreparationErrorKind::Projection,
        }
    }

    /// Creates an error without provider metadata.
    ///
    /// Arbitrary caller text is credential-redacted and UTF-8-safely bounded
    /// by [`SafeDiagnostic`] before it can be retained or displayed.
    #[must_use]
    pub fn new(
        kind: AnthropicErrorKind,
        phase: AnthropicErrorPhase,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            phase,
            message: SafeDiagnostic::new(message.into()),
            status: None,
            code: None,
            request_id: None,
            retry: RetryAdvice::Never,
            retry_after_millis: None,
        }
    }

    /// Adds safe provider metadata and retry classification.
    #[must_use]
    pub fn with_provider_metadata(
        mut self,
        status: Option<u16>,
        code: Option<String>,
        request_id: Option<String>,
        retry: RetryAdvice,
    ) -> Self {
        self.status = status;
        self.code =
            code.filter(|value| valid_provider_metadata(value, MAX_PROVIDER_ERROR_CODE_BYTES));
        self.request_id = request_id
            .filter(|value| valid_provider_metadata(value, MAX_PROVIDER_REQUEST_ID_BYTES));
        self.retry = retry;
        self
    }

    /// Adds bounded provider retry-delay advice in milliseconds.
    ///
    /// Values above 24 hours saturate at 24 hours. This is descriptive
    /// metadata and never causes this crate to retry an operation.
    #[must_use]
    pub fn with_retry_after_millis(mut self, retry_after_millis: Option<u64>) -> Self {
        self.retry_after_millis =
            retry_after_millis.map(|value| value.min(plakous_model::ModelRetryDelay::MAX_MILLIS));
        self
    }

    pub(crate) fn with_notice(mut self, notice: &'static str) -> Self {
        self.message = SafeDiagnostic::new(notice);
        self
    }

    /// Returns the stable category.
    #[must_use]
    pub const fn kind(&self) -> AnthropicErrorKind {
        self.kind
    }

    /// Returns the failing operation phase.
    #[must_use]
    pub const fn phase(&self) -> AnthropicErrorPhase {
        self.phase
    }

    /// Returns the redaction-safe diagnostic.
    #[must_use]
    pub fn message(&self) -> &str {
        self.message.as_str()
    }

    /// Returns the bounded, credential-redacted diagnostic value.
    #[must_use]
    pub const fn diagnostic(&self) -> &SafeDiagnostic {
        &self.message
    }

    /// Returns the HTTP status when available.
    #[must_use]
    pub const fn status(&self) -> Option<u16> {
        self.status
    }

    /// Returns the provider error type when available.
    #[must_use]
    pub fn code(&self) -> Option<&str> {
        self.code.as_deref()
    }

    /// Returns the provider request ID when available.
    #[must_use]
    pub fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }

    /// Returns protocol retry advice. The attempt service itself does not retry.
    #[must_use]
    pub const fn retry_advice(&self) -> RetryAdvice {
        self.retry
    }

    /// Returns bounded provider retry-delay advice in milliseconds.
    #[must_use]
    pub const fn retry_after_millis(&self) -> Option<u64> {
        self.retry_after_millis
    }

    /// Creates a premature-EOF error.
    #[must_use]
    pub fn incomplete() -> Self {
        Self::new(
            AnthropicErrorKind::IncompleteResponse,
            AnthropicErrorPhase::Stream,
            "Anthropic stream ended before message_stop",
        )
    }
}

impl fmt::Debug for AnthropicError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AnthropicError")
            .field("kind", &self.kind)
            .field("phase", &self.phase)
            .field("message", &"<redacted>")
            .field("status", &self.status)
            .field("code", &self.code.as_ref().map(|_| "<redacted>"))
            .field(
                "request_id",
                &self.request_id.as_ref().map(|_| "<redacted>"),
            )
            .field("retry", &self.retry)
            .field("retry_after_millis", &self.retry_after_millis)
            .finish()
    }
}

impl fmt::Display for AnthropicError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.message, formatter)
    }
}

impl std::error::Error for AnthropicError {}

impl From<plakous_auth::api_key::ApiKeyError> for AnthropicError {
    fn from(error: plakous_auth::api_key::ApiKeyError) -> Self {
        crate::route::api_key_error(error)
    }
}

/// Maximum provider error response bytes inspected by the classifier.
pub(crate) const MAX_PROVIDER_ERROR_BODY_BYTES: usize = 64 * 1024;

/// Maximum decoded bytes accepted in a provider error type.
pub(crate) const MAX_PROVIDER_ERROR_CODE_BYTES: usize = 128;

/// Maximum decoded bytes accepted in a provider error message.
pub(crate) const MAX_PROVIDER_ERROR_MESSAGE_BYTES: usize = 4 * 1024;

/// Maximum decoded bytes accepted in a provider request ID.
pub const MAX_PROVIDER_REQUEST_ID_BYTES: usize = 256;

#[derive(Deserialize)]
struct ErrorBody {
    error: ProviderErrorBody,
    #[serde(default)]
    request_id: Option<String>,
}

#[derive(Deserialize)]
struct ProviderErrorBody {
    #[serde(rename = "type")]
    kind: String,
    /// Length-checked and read only to classify; never retained.
    message: String,
}

impl ErrorBody {
    fn fields_are_bounded(&self) -> bool {
        self.error.kind.len() <= MAX_PROVIDER_ERROR_CODE_BYTES
            && self.error.message.len() <= MAX_PROVIDER_ERROR_MESSAGE_BYTES
            && self
                .request_id
                .as_ref()
                .is_none_or(|value| value.len() <= MAX_PROVIDER_REQUEST_ID_BYTES)
    }
}

pub(crate) fn valid_provider_metadata(value: &str, max_bytes: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_bytes
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        })
}

/// Classifies a provider error using explicit acceptance/output/effect facts.
///
/// Transient status alone never permits retry after provider acceptance,
/// visible output, or possible hosted effects.
#[must_use]
pub fn classify_provider_error_with_safety(
    status: Option<u16>,
    body: &[u8],
    header_request_id: Option<&str>,
    phase: AnthropicErrorPhase,
    safety: RetrySafety,
) -> AnthropicError {
    // The outer byte ceiling is checked before JSON parsing so serde can never
    // allocate from an unbounded provider response. Decoded field ceilings are
    // then checked before any provider metadata is retained.
    let parsed = (body.len() <= MAX_PROVIDER_ERROR_BODY_BYTES)
        .then(|| serde_json::from_slice::<ErrorBody>(body).ok())
        .flatten()
        .filter(ErrorBody::fields_are_bounded);
    let code = parsed.as_ref().map(|value| value.error.kind.as_str());
    let kind = match code {
        Some("authentication_error") => AnthropicErrorKind::Authentication,
        Some("permission_error") => AnthropicErrorKind::Authorization,
        Some("billing_error") => AnthropicErrorKind::Provider,
        Some("rate_limit_error") => AnthropicErrorKind::RateLimited,
        Some("overloaded_error") => AnthropicErrorKind::Overloaded,
        Some("request_too_large") => AnthropicErrorKind::Limit,
        // "prompt is too long: N tokens > M maximum" is the only name the
        // provider gives a context overflow; it arrives as a plain 400.
        Some("invalid_request_error")
            if parsed.as_ref().is_some_and(|value| {
                plakous_model::provider_rejection_is_context_overflow(None, &value.error.message)
            }) =>
        {
            AnthropicErrorKind::Limit
        }
        Some("invalid_request_error") => AnthropicErrorKind::InvalidRequest,
        _ if status == Some(401) => AnthropicErrorKind::Authentication,
        _ if status == Some(403) => AnthropicErrorKind::Authorization,
        _ if status == Some(429) => AnthropicErrorKind::RateLimited,
        _ if status.is_some_and(|value| value >= 500) => AnthropicErrorKind::Overloaded,
        _ => AnthropicErrorKind::Provider,
    };
    let retry = match kind {
        AnthropicErrorKind::RateLimited | AnthropicErrorKind::Overloaded
            if !safety.provider_accepted()
                && !safety.visible_output()
                && !safety.hosted_effects_possible() =>
        {
            RetryAdvice::Transient
        }
        _ => RetryAdvice::Never,
    };
    // Provider prose is intentionally classification-only input. It is never
    // retained or exposed through Display, Debug, or the diagnostic accessor.
    let message = "Anthropic provider error";
    let request_id = header_request_id
        .filter(|value| valid_provider_metadata(value, MAX_PROVIDER_REQUEST_ID_BYTES))
        .map(str::to_owned)
        .or_else(|| {
            parsed.as_ref().and_then(|value| {
                value
                    .request_id
                    .as_deref()
                    .filter(|request_id| {
                        valid_provider_metadata(request_id, MAX_PROVIDER_REQUEST_ID_BYTES)
                    })
                    .map(str::to_owned)
            })
        });
    AnthropicError::new(kind, phase, message).with_provider_metadata(
        status,
        code.map(str::to_owned),
        request_id,
        retry,
    )
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests;
