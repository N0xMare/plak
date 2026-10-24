//! Redaction-safe native Claude OAuth errors.

use super::{Error, PrivateStoreError};

/// Redaction-safe native Claude OAuth failure.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ClaudeNativeAuthError {
    /// The caller did not explicitly accept the exact pinned revision.
    #[error("the exact Claude OAuth compatibility profile was not accepted")]
    CompatibilityNotAccepted,
    /// The pinned compatibility profile expired or was killed in code.
    #[error("the pinned Claude OAuth compatibility profile is inactive")]
    CompatibilityInactive,
    /// The selected slot has no credential.
    #[error("the selected Claude OAuth slot is not logged in")]
    NotLoggedIn,
    /// The selected slot is not Ready.
    #[error("the selected Claude OAuth credential is not ready")]
    NotReady,
    /// The pasted authorization input failed correlation or grammar.
    #[error("the Claude OAuth authorization input is invalid")]
    InvalidCallback,
    /// The login expired before completion.
    #[error("the Claude OAuth login expired")]
    LoginExpired,
    /// Authorization-code token exchange failed.
    #[error("the Claude OAuth login token exchange failed")]
    LoginTokenExchange,
    /// Token scope did not admit inference.
    #[error("the Claude OAuth login account verification failed")]
    LoginAccountVerification,
    /// Authenticated account metadata is missing, malformed, or conflicting.
    #[error("the authenticated Claude OAuth account identity is unavailable")]
    AccountIdentityUnavailable,
    /// A verified login could not be committed durably.
    #[error("the Claude OAuth login credential could not be persisted")]
    LoginPersistence,
    /// A strict OAuth token response check failed.
    #[error("the Claude OAuth token response is invalid")]
    InvalidTokenResponse,
    /// Stored opaque provider data was malformed or foreign.
    #[error("the stored Claude OAuth credential is invalid")]
    InvalidStoredCredential,
    /// The stored credential was written in a format this build no longer reads.
    #[error("the stored Claude OAuth credential format is retired; log in again")]
    StoredCredentialFormatRetired,
    /// A newer build wrote the stored credential; this build neither uses
    /// nor rewrites it.
    #[error("the stored Claude OAuth credential was written by a newer build")]
    StoredCredentialWrittenByNewerBuild,
    /// Refresh entered durable Unknown and cannot be retried automatically.
    #[error("the Claude OAuth refresh outcome is quarantined")]
    RefreshQuarantined,
    /// The fixed authentication exchange failed.
    #[error("the Claude OAuth authentication exchange failed")]
    Transport(Option<plakous_model::ModelNetworkFailure>),
    /// Provider-neutral binding generation failed.
    #[error(transparent)]
    Binding(#[from] plakous_auth::BindingError),
    /// Provider-neutral credential value validation failed.
    #[error(transparent)]
    CredentialValue(#[from] plakous_auth::CredentialValueError),
    /// Private selected-slot storage failed.
    #[error(transparent)]
    Store(#[from] PrivateStoreError),
    /// Fixed request construction failed.
    #[error(transparent)]
    HttpRequest(#[from] plakous_http::RequestError),
}
