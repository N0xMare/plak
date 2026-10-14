//! Shared projection failure vocabulary for Responses-family stage-2 projectors.

use thiserror::Error;

/// A redaction-safe Responses-family-to-canonical projection failure.
///
/// Dialects re-export branded error enums and map via [`From`]. Dialect-only
/// variants (`ForeignAttemptBinding`, provider-metadata) stay local.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum FamilyProjectionError {
    /// Provider lifecycle or canonical ordering was invalid.
    #[error("invalid Responses-family canonical projection: {0}")]
    InvalidState(&'static str),
    /// A non-success terminal provider state cannot become a canonical success.
    #[error("response ended without canonical success ({0})")]
    TerminalFailure(&'static str),
    /// A configured projector resource bound was exceeded.
    #[error("canonical projection exceeded {0}")]
    ResourceLimit(&'static str),
    /// Canonical identity injection failed.
    #[error("canonical identity injection failed")]
    Identity(#[from] plakous_model::IdentityError),
    /// Canonical content validation failed.
    #[error("canonical content validation failed")]
    Content(#[from] plakous_model::ContentError),
    /// Replay serialization failed.
    #[error("canonical replay serialization failed")]
    Json(#[from] serde_json::Error),
    /// A projected text fragment was not valid UTF-8 at completion.
    #[error("projected content ended with invalid UTF-8")]
    Utf8(#[from] std::string::FromUtf8Error),
}
