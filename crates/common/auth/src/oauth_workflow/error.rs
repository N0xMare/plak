//! Redaction-safe OAuth workflow errors.

use thiserror::Error;

/// Pure OAuth workflow failure.
///
/// Variants deliberately carry no provider names, secrets, raw query strings,
/// form bodies, or token material. Higher layers map these to dialect errors.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum OAuthWorkflowError {
    /// Cryptographic binding material or correlation failed.
    #[error("oauth binding failure")]
    Binding,
    /// Callback target, query, or correlation failed.
    #[error("oauth callback failure")]
    Callback,
    /// Login or poll deadline elapsed.
    #[error("oauth flow expired")]
    Expired,
    /// A response body exceeded the configured byte bound.
    #[error("oauth response too large")]
    ResponseTooLarge,
    /// JSON could not be decoded under workflow bounds.
    #[error("oauth invalid json")]
    InvalidJson,
    /// An endpoint or redirect URL could not be constructed or matched.
    #[error("oauth invalid url")]
    InvalidUrl,
    /// A form body, field set, or interval value was invalid.
    #[error("oauth invalid form")]
    InvalidForm,
}
