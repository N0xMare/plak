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
    /// Values above 24 hours are discarded. This is descriptive metadata and
    /// never causes this crate to retry an operation.
    #[must_use]
    pub fn with_retry_after_millis(mut self, retry_after_millis: Option<u64>) -> Self {
        self.retry_after_millis = retry_after_millis.filter(|value| *value <= 86_400_000);
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

/// Maximum provider error response bytes inspected by the classifier.
pub const MAX_PROVIDER_ERROR_BODY_BYTES: usize = 64 * 1024;

/// Maximum decoded bytes accepted in a provider error type.
pub const MAX_PROVIDER_ERROR_CODE_BYTES: usize = 128;

/// Maximum decoded bytes accepted in a provider error message.
pub const MAX_PROVIDER_ERROR_MESSAGE_BYTES: usize = 4 * 1024;

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

/// Classifies an HTTP or in-stream Anthropic error body without retaining raw content.
#[must_use]
pub fn classify_provider_error(
    status: Option<u16>,
    body: &[u8],
    header_request_id: Option<&str>,
    phase: AnthropicErrorPhase,
) -> AnthropicError {
    classify_provider_error_with_safety(
        status,
        body,
        header_request_id,
        phase,
        RetrySafety::new(true, false, false),
    )
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
mod tests {
    use super::*;

    const SECRET_SENTINEL: &str = "ANTHROPIC_SECRET_SENTINEL_DO_NOT_RETAIN";

    #[test]
    fn a_prompt_too_long_rejection_is_a_limit_and_other_invalid_requests_are_not() {
        let overflow = br#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 270013 tokens > 200000 maximum"}}"#;
        let error =
            classify_provider_error(Some(400), overflow, None, AnthropicErrorPhase::Connect);
        assert_eq!(error.kind(), AnthropicErrorKind::Limit);
        assert!(
            !error.message().contains("270013"),
            "provider prose is never retained"
        );
        let other = br#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: must be at most 8192"}}"#;
        let error = classify_provider_error(Some(400), other, None, AnthropicErrorPhase::Connect);
        assert_eq!(error.kind(), AnthropicErrorKind::InvalidRequest);
    }

    #[test]
    fn public_diagnostics_are_bounded_utf8_safe_and_credential_redacted() {
        let raw = format!(
            "authorization: Bearer {SECRET_SENTINEL} token={SECRET_SENTINEL} sk-{SECRET_SENTINEL} {}",
            "🦀".repeat(plakous_model::MAX_SAFE_DIAGNOSTIC_BYTES)
        );
        let error = AnthropicError::new(
            AnthropicErrorKind::InvalidRequest,
            AnthropicErrorPhase::Prepare,
            raw,
        );
        assert!(!error.message().contains(SECRET_SENTINEL));
        assert!(error.message().len() <= plakous_model::MAX_SAFE_DIAGNOSTIC_BYTES);
        assert!(error.message().is_char_boundary(error.message().len()));
        assert_eq!(error.message(), error.diagnostic().as_str());
        assert_eq!(error.to_string(), error.message());
        assert!(!error.to_string().contains(SECRET_SENTINEL));
        let debug = format!("{error:?}");
        assert!(!debug.contains(SECRET_SENTINEL));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn provider_metadata_stays_separately_validated_and_debug_redacted() {
        let error = AnthropicError::new(
            AnthropicErrorKind::RateLimited,
            AnthropicErrorPhase::Connect,
            format!("Bearer {SECRET_SENTINEL}"),
        )
        .with_provider_metadata(
            Some(429),
            Some("rate_limit_error".to_owned()),
            Some("req_safe_01".to_owned()),
            RetryAdvice::Transient,
        );
        assert_eq!(error.status(), Some(429));
        assert_eq!(error.code(), Some("rate_limit_error"));
        assert_eq!(error.request_id(), Some("req_safe_01"));
        assert!(!error.message().contains(SECRET_SENTINEL));
        let debug = format!("{error:?}");
        assert!(!debug.contains("rate_limit_error"));
        assert!(!debug.contains("req_safe_01"));

        let invalid = AnthropicError::new(
            AnthropicErrorKind::Provider,
            AnthropicErrorPhase::Stream,
            "invalid metadata fixture",
        )
        .with_provider_metadata(
            Some(500),
            Some("bad code".to_owned()),
            Some("bad\nrequest".to_owned()),
            RetryAdvice::Never,
        );
        assert_eq!(invalid.code(), None);
        assert_eq!(invalid.request_id(), None);

        let oversized = AnthropicError::new(
            AnthropicErrorKind::Provider,
            AnthropicErrorPhase::Stream,
            "oversized metadata fixture",
        )
        .with_provider_metadata(
            None,
            Some("c".repeat(MAX_PROVIDER_ERROR_CODE_BYTES + 1)),
            Some("r".repeat(MAX_PROVIDER_REQUEST_ID_BYTES + 1)),
            RetryAdvice::Never,
        );
        assert_eq!(oversized.code(), None);
        assert_eq!(oversized.request_id(), None);
    }

    #[test]
    fn provider_prose_is_classification_only_and_never_public() {
        let body = format!(
            "{{\"type\":\"error\",\"error\":{{\"type\":\"overloaded_error\",\"message\":\"sk-{SECRET_SENTINEL}\"}},\"request_id\":\"req_body_01\"}}"
        );
        let error = classify_provider_error(
            Some(529),
            body.as_bytes(),
            Some("req_header_01"),
            AnthropicErrorPhase::Stream,
        );
        assert_eq!(error.kind(), AnthropicErrorKind::Overloaded);
        assert_eq!(error.code(), Some("overloaded_error"));
        assert_eq!(error.request_id(), Some("req_header_01"));
        assert_eq!(error.message(), "Anthropic provider error");
        assert!(!error.to_string().contains(SECRET_SENTINEL));
        assert!(!format!("{error:?}").contains(SECRET_SENTINEL));
    }

    #[test]
    fn hosted_effect_risk_disables_transient_retry() {
        let body = br#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#;
        let accepted = classify_provider_error_with_safety(
            None,
            body,
            None,
            AnthropicErrorPhase::Stream,
            RetrySafety::new(true, false, false),
        );
        assert_eq!(accepted.retry_advice(), RetryAdvice::Never);

        let before_acceptance = classify_provider_error_with_safety(
            None,
            body,
            None,
            AnthropicErrorPhase::Connect,
            RetrySafety::new(false, false, false),
        );
        assert_eq!(before_acceptance.retry_advice(), RetryAdvice::Transient);

        let unsafe_effect = classify_provider_error_with_safety(
            None,
            body,
            None,
            AnthropicErrorPhase::Stream,
            RetrySafety::new(true, false, true),
        );
        assert_eq!(unsafe_effect.retry_advice(), RetryAdvice::Never);
    }
}
