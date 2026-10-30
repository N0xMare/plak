//! Redaction-safe protocol, stream, and attempt errors.

use std::fmt;

use plakous_model::{ModelErrorKind, SafeDiagnostic};

use thiserror::Error;

/// The operation phase in which an `OpenAI` error occurred.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ErrorPhase {
    /// Request validation or serialization.
    Request,
    /// Authentication resolution.
    Authentication,
    /// Connection establishment.
    Connect,
    /// Waiting for the first stream event.
    FirstEvent,
    /// Reading or decoding a stream.
    Stream,
    /// Terminal response validation.
    Terminal,
}

/// Whether one physical operation could be considered by a retry owner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RetryClass {
    /// Retrying this attempt is forbidden.
    Never,
    /// The full-operation service may retry only if no request was accepted.
    BeforeAcceptance,
    /// The provider explicitly marked a transient condition.
    ProviderTransient,
    /// Only the operation owner may retry despite duplicate inference or billing risk.
    ///
    /// This disposition must not be treated as permission for a caller or Tower
    /// layer to retry the complete logical operation blindly.
    OperationOwnedDuplicateRisk,
}

/// A typed, redaction-safe `OpenAI` operation error.
#[derive(Clone, Eq, Error, PartialEq)]
#[error("OpenAI {phase:?} error: {safe_message}")]
pub struct OpenAiError {
    phase: ErrorPhase,
    kind: ModelErrorKind,
    retry: RetryClass,
    safe_status: Option<u16>,
    provider_code: Option<String>,
    request_id: Option<String>,
    safe_message: SafeDiagnostic,
}

/// Maximum bytes retained for a validated provider error code.
pub(crate) const MAX_PROVIDER_CODE_BYTES: usize = 128;

/// Maximum bytes retained for a validated provider request ID.
pub const MAX_PROVIDER_REQUEST_ID_BYTES: usize = 256;

impl OpenAiError {
    /// Creates a typed error and sanitizes the diagnostic for public retention.
    ///
    /// Arbitrary caller text is credential-redacted and UTF-8-safely bounded by
    /// [`SafeDiagnostic`].
    #[must_use]
    pub fn new(
        phase: ErrorPhase,
        kind: ModelErrorKind,
        retry: RetryClass,
        safe_message: impl Into<String>,
    ) -> Self {
        Self {
            phase,
            kind,
            retry,
            safe_status: None,
            provider_code: None,
            request_id: None,
            safe_message: SafeDiagnostic::new(safe_message.into()),
        }
    }

    /// Attaches a safe HTTP status, provider code, and request ID.
    ///
    /// Non-HTTP status values and metadata outside the public byte ceilings or
    /// conservative ASCII identifier grammar are discarded.
    #[must_use]
    pub fn with_provider_metadata(
        mut self,
        safe_status: Option<u16>,
        provider_code: Option<String>,
        request_id: Option<String>,
    ) -> Self {
        self.safe_status = safe_status.filter(|status| (100..=599).contains(status));
        self.provider_code =
            provider_code.filter(|value| valid_provider_metadata(value, MAX_PROVIDER_CODE_BYTES));
        self.request_id = request_id
            .filter(|value| valid_provider_metadata(value, MAX_PROVIDER_REQUEST_ID_BYTES));
        self
    }

    /// Returns the attributed operation phase.
    #[must_use]
    pub const fn phase(&self) -> ErrorPhase {
        self.phase
    }

    /// Returns the canonical failure category.
    #[must_use]
    pub const fn kind(&self) -> ModelErrorKind {
        self.kind
    }

    /// Returns retry classification for the protocol's sole retry owner.
    #[must_use]
    pub const fn retry_class(&self) -> RetryClass {
        self.retry
    }

    /// Returns a safe HTTP status when available.
    #[must_use]
    pub const fn safe_status(&self) -> Option<u16> {
        self.safe_status
    }

    /// Returns a bounded, non-secret provider error code when available.
    #[must_use]
    pub fn provider_code(&self) -> Option<&str> {
        self.provider_code.as_deref()
    }

    /// Returns a safe provider request identity when available.
    #[must_use]
    pub fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }

    /// Returns the bounded and sanitized public diagnostic.
    #[must_use]
    pub fn safe_message(&self) -> &str {
        self.safe_message.as_str()
    }

    /// Returns the retained bounded and sanitized diagnostic value.
    #[must_use]
    pub const fn diagnostic(&self) -> &SafeDiagnostic {
        &self.safe_message
    }
}

impl fmt::Debug for OpenAiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiError")
            .field("phase", &self.phase)
            .field("kind", &self.kind)
            .field("retry", &self.retry)
            .field("safe_status", &self.safe_status)
            .field(
                "provider_code",
                &self.provider_code.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "request_id",
                &self.request_id.as_ref().map(|_| "<redacted>"),
            )
            .field("safe_message", &self.safe_message)
            .finish()
    }
}

pub(crate) fn valid_provider_metadata(value: &str, maximum_bytes: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum_bytes
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        })
}

#[cfg(test)]
mod tests;
