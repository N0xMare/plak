//! Redaction-safe native Codex authentication errors.

use super::Error;

/// Redaction-safe native authentication failure.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum NativeCodexAuthError {
    /// Transient model lookup retries exhausted their bounded budget.
    #[error(transparent)]
    Lookup(plakous_exchange::lookup::LookupError),
    /// The exact experimental notice was not accepted.
    #[error("native Codex compatibility profile was not accepted exactly")]
    ProfileNotAccepted,
    /// The pinned profile is expired, killed, or accepted from the future.
    #[error("native Codex compatibility profile is not currently admissible")]
    ProfileNotCurrent,
    /// Explicit private storage could not be opened or mutated safely.
    #[error("native Codex credential store failed")]
    Store,
    /// A peer owns the credential record's writer lock.
    #[error("native Codex credential store is busy")]
    StoreBusy,
    /// Secure randomness or a local login binding failed.
    #[error("native Codex login binding failed")]
    Binding,
    /// A bounded local callback was invalid or expired.
    #[error("native Codex browser callback was invalid")]
    Callback,
    /// The provider does not offer device-code login for this account or server.
    #[error("native Codex device-code login is not available for this account/server")]
    DeviceLoginUnavailable,
    /// The device code expired before the operator approved it.
    #[error("native Codex device code expired before it was approved")]
    DeviceCodeExpired,
    /// The login token request failed before a complete response was available.
    #[error("native Codex login token transport failed")]
    LoginTokenTransport,
    /// The login token endpoint rejected the authorization-code exchange.
    #[error("native Codex login token endpoint rejected the exchange")]
    LoginTokenRejected,
    /// The successful login token response was not provider-compatible.
    #[error("native Codex login token response was invalid")]
    LoginTokenResponseInvalid,
    /// Login signing keys could not be fetched or resolved safely.
    #[error("native Codex login signing keys failed validation")]
    LoginSigningKeys,
    /// Login identity, claims, or account verification failed.
    #[error("native Codex login account verification failed")]
    LoginAccountVerification,
    /// A validated login could not be committed safely.
    #[error("native Codex login persistence failed")]
    LoginPersistence,
    /// Provider transport failed without exposing provider data.
    #[error("native Codex authentication transport failed")]
    Transport(Option<plakous_model::ModelNetworkFailure>),
    /// Provider response status or framing was not accepted.
    #[error("native Codex authentication response was rejected")]
    Provider,
    /// OAuth/OIDC/JWKS data failed strict validation.
    #[error("native Codex authentication response failed validation")]
    Validation,
    /// Credential is absent, revoked, expired, or quarantined.
    #[error("native Codex credential is unavailable")]
    Unavailable,
    /// Credential account continuity no longer matches the admitted route.
    #[error("native Codex credential account changed")]
    AccountChanged,
    /// In-process authentication coordination failed.
    #[error("native Codex authentication coordination failed")]
    Coordination,
    /// Model catalog data was invalid or stale.
    #[error("native Codex model catalog failed validation")]
    Catalog,
    /// The stored login cannot be used by this build (for example it was
    /// made by an earlier build without a login selection epoch); log in again.
    #[error("native Codex login must be renewed with a new login")]
    LoginRequired,
    /// A newer build wrote the stored login; this build neither uses nor
    /// rewrites it.
    #[error("native Codex login was written by a newer build")]
    WrittenByNewerBuild,
    /// The stored login was written under another compatibility profile;
    /// only a new login replaces it.
    #[error("native Codex login was written under another compatibility profile")]
    ProfileChanged,
    /// The selected login changed (a new login) under an admitted receipt.
    #[error("native Codex login selection changed")]
    SelectionChanged,
    /// Stored catalog credential payload could not be decoded.
    #[error("native Codex catalog credential payload failed validation")]
    CatalogCredential,
    /// Catalog authorization header could not be constructed safely.
    #[error("native Codex catalog authorization header failed validation")]
    CatalogAuthorizationHeader,
    /// Catalog account header could not be constructed safely.
    #[error("native Codex catalog account header failed validation")]
    CatalogAccountHeader,
    /// Pre-catalog credential refresh failed strict local validation.
    #[error("native Codex catalog refresh failed validation")]
    CatalogRefresh,
    /// Catalog response media type was not the pinned JSON form.
    #[error("native Codex catalog media type failed validation")]
    CatalogMediaType,
    /// Catalog JSON did not match the pinned document schema.
    #[error("native Codex catalog document failed validation")]
    CatalogDocument,
    /// One catalog model identifier was outside the pinned grammar.
    #[error("native Codex catalog model identifier failed validation")]
    CatalogIdentifier,
}
