//! Redaction-safe native authentication and transport errors.

use super::{
    AuthTransportError, CredentialRefreshFailureReason, Error, PrivateStoreError, network_failure,
};

/// Redaction-safe native authentication failure.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum XaiNativeAuthError {
    /// Transient model lookup retries exhausted their bounded budget.
    #[error(transparent)]
    Lookup(plakous_exchange::lookup::LookupError),
    /// The caller did not explicitly accept the exact pinned revision.
    #[error("the exact xAI compatibility profile was not accepted")]
    CompatibilityNotAccepted,
    /// The pinned compatibility profile expired or was killed in code.
    #[error("the pinned xAI compatibility profile is inactive")]
    CompatibilityInactive,
    /// Required local randomness was unavailable.
    #[error("secure randomness is unavailable")]
    RandomnessUnavailable,
    /// The selected slot has no credential.
    #[error("the selected xAI slot is not logged in")]
    NotLoggedIn,
    /// The selected slot is not Ready.
    #[error("the selected xAI credential is not ready")]
    NotReady,
    /// The fixed browser callback socket could not be acquired.
    #[error("the fixed xAI loopback callback is unavailable")]
    LoopbackUnavailable,
    /// The callback was malformed or failed correlation.
    #[error("the xAI OAuth callback is invalid")]
    InvalidCallback,
    /// The login or device code expired.
    #[error("the xAI login expired")]
    LoginExpired,
    /// The resource owner denied authorization.
    #[error("xAI authorization was denied")]
    AuthorizationDenied,
    /// Browser or device login failed during the bounded OAuth token exchange.
    #[error("the xAI login token exchange failed")]
    LoginTokenExchange,
    /// Browser or device login failed while resolving trusted signing keys.
    #[error("the xAI login signing-key verification failed")]
    LoginSigningKeys,
    /// Browser or device login failed while verifying the provider account.
    #[error("the xAI login account verification failed")]
    LoginAccountVerification,
    /// A verified browser or device login could not be committed durably.
    #[error("the xAI login credential could not be persisted")]
    LoginPersistence,
    /// A strict OAuth token response check failed.
    #[error("the xAI OAuth token response is invalid ({})", .0.as_str())]
    InvalidTokenResponse(CredentialRefreshFailureReason),
    /// ID-token signature, header, claims, nonce, or times failed validation.
    #[error("the xAI ID token is invalid ({})", .0.as_str())]
    InvalidIdToken(CredentialRefreshFailureReason),
    /// JWKS grammar, bounds, or key metadata was invalid.
    #[error("the xAI JWKS response is invalid")]
    InvalidJwks,
    /// The signed token referenced no current bounded signing key.
    #[error("the xAI ID token signing key is unknown")]
    UnknownSigningKey,
    /// `UserInfo` or token subject changed account.
    #[error("the xAI OAuth account changed")]
    AccountChanged,
    /// The app-owned login selection changed since a route was captured.
    #[error("the selected xAI login changed")]
    SelectionChanged,
    /// Stored opaque provider data was malformed or foreign.
    #[error("the stored xAI credential is invalid")]
    InvalidStoredCredential,
    /// The stored credential was written in a format this build no longer reads.
    #[error("the stored xAI credential format is retired; log in again")]
    StoredCredentialFormatRetired,
    /// A newer build wrote the stored credential; this build neither uses
    /// nor rewrites it.
    #[error("the stored xAI credential was written by a newer build")]
    StoredCredentialWrittenByNewerBuild,
    /// The fresh account-scoped model catalog did not admit the exact pinned model.
    #[error("the exact xAI subscription model is not available in a fresh catalog")]
    CatalogUnavailable,
    /// The caller requested an alias or model outside the exact admitted tuple.
    #[error("the requested xAI subscription model is not exactly admitted")]
    RequestedModelNotAdmitted,
    /// Refresh entered durable Unknown and cannot be retried automatically.
    #[error("the xAI refresh outcome is quarantined")]
    RefreshQuarantined,
    /// A fixed HTTPS request could not be constructed.
    #[error("the fixed xAI authentication request is invalid")]
    Request,
    /// The fixed authentication exchange failed.
    #[error("the xAI authentication exchange failed")]
    Transport(Option<plakous_model::ModelNetworkFailure>),
    /// The provider rejected the fixed authentication exchange.
    #[error("xAI rejected the authentication exchange with HTTP {0}")]
    HttpStatus(u16),
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
    /// Route identity construction failed.
    #[error(transparent)]
    Route(#[from] crate::XaiRouteError),
    /// Model resolution failed.
    #[error(transparent)]
    Model(#[from] crate::XaiModelError),
    /// Language-model construction failed.
    #[error(transparent)]
    LanguageModel(#[from] crate::XaiLanguageModelConfigError),
    /// Neutral descriptor digest construction failed.
    #[error(transparent)]
    Descriptor(#[from] plakous_model::ValidationError),
    /// Neutral identity construction failed.
    #[error(transparent)]
    Identity(#[from] plakous_model::IdentityError),
}

impl From<AuthTransportError> for XaiNativeAuthError {
    fn from(error: AuthTransportError) -> Self {
        Self::Transport(match error {
            AuthTransportError::Failure(failure) => network_failure(failure),
            AuthTransportError::TooLarge => None,
        })
    }
}

#[derive(Clone, Copy)]
pub(super) enum LoginHttpStage {
    SigningKeys,
    AccountVerification,
}

impl LoginHttpStage {
    pub(super) const fn error(self) -> XaiNativeAuthError {
        match self {
            Self::SigningKeys => XaiNativeAuthError::LoginSigningKeys,
            Self::AccountVerification => XaiNativeAuthError::LoginAccountVerification,
        }
    }
}
