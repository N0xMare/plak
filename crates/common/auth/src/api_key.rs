//! Injected API-key secrets, generation grants and exact-route sources.

use std::fmt;
use std::sync::Arc;

use crate::SecretString;

mod generation;

pub use generation::{
    CredentialGenerationFingerprint, CredentialGenerationFingerprintError, CredentialProviderTag,
};

const MAX_API_KEY_BYTES: usize = 64 * 1024;

/// A non-cloneable, non-serializable API key, zeroized on drop.
///
/// ```compile_fail
/// use plakous_auth::api_key::ApiKey;
/// let key = ApiKey::new("synthetic-key").unwrap();
/// let _ = key.clone();
/// ```
///
/// ```compile_fail
/// use plakous_auth::api_key::ApiKey;
/// let key = ApiKey::new("synthetic-key").unwrap();
/// let _ = serde_json::to_string(&key);
/// ```
///
/// Raw exposure uses the unsupported integration-only `ExposeApiKey` trait.
///
/// ```compile_fail
/// use plakous_auth::api_key::ApiKey;
/// let key = ApiKey::new("synthetic-key").unwrap();
/// let _ = key.expose_secret();
/// ```
pub struct ApiKey {
    value: SecretString,
    authority: ApiKeyAuthorityToken,
}

impl ApiKey {
    /// Validates bounded, nonempty key text without control bytes.
    ///
    /// # Errors
    /// Returns [`ApiKeyError::InvalidKey`] for invalid text.
    pub fn new(value: impl Into<String>) -> Result<Self, ApiKeyError> {
        let value = SecretString::new(value.into());
        let text = value.expose_secret();
        if text.is_empty()
            || text.len() > MAX_API_KEY_BYTES
            || text.bytes().any(|byte| byte.is_ascii_control())
        {
            return Err(ApiKeyError::InvalidKey);
        }
        Ok(Self {
            value,
            authority: ApiKeyAuthorityToken(Arc::new(())),
        })
    }

    /// Returns the opaque identity minted with this secret container.
    #[must_use]
    pub const fn authority(&self) -> &ApiKeyAuthorityToken {
        &self.authority
    }

    /// Derives non-secret generation authority without retaining its input bytes.
    #[must_use]
    pub fn generation_grant<A>(
        &self,
        provider: CredentialProviderTag,
        authority: A,
    ) -> ApiKeyGenerationGrant<A> {
        ApiKeyGenerationGrant {
            provider,
            authority,
            fingerprint: CredentialGenerationFingerprint::for_valid_key(
                provider,
                self.expose_secret().as_bytes(),
            ),
        }
    }

    /// Validates replacement text while retaining this key container's authority.
    ///
    /// # Errors
    /// Returns [`ApiKeyError::InvalidKey`] without changing this key for invalid text.
    pub fn renew_with(&self, value: impl Into<String>) -> Result<Self, ApiKeyError> {
        let mut renewed = Self::new(value)?;
        renewed.authority = self.authority.clone();
        Ok(renewed)
    }
}

/// Unsupported integration-only raw bearer access, absent from the facade.
#[doc(hidden)]
pub trait ExposeApiKey {
    /// Borrows bearer text only at an integration's audited use site.
    fn expose_secret(&self) -> &str;
}

impl ExposeApiKey for ApiKey {
    fn expose_secret(&self) -> &str {
        self.value.expose_secret()
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ApiKey(<redacted>)")
    }
}

impl fmt::Display for ApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

/// Non-linkable identity of one immutable API-key container.
#[derive(Clone)]
pub struct ApiKeyAuthorityToken(Arc<()>);

impl PartialEq for ApiKeyAuthorityToken {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for ApiKeyAuthorityToken {}

impl fmt::Debug for ApiKeyAuthorityToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ApiKeyAuthorityToken(<redacted>)")
    }
}

/// One provider-tagged key generation and its source-owned authority.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct ApiKeyGenerationGrant<A = ()> {
    provider: CredentialProviderTag,
    authority: A,
    fingerprint: CredentialGenerationFingerprint,
}

impl<A> ApiKeyGenerationGrant<A> {
    /// Hashes a bounded canonical generation nonce or credential fence.
    ///
    /// # Errors
    /// Rejects an empty or oversized generation binding.
    pub fn from_generation_binding(
        provider: CredentialProviderTag,
        authority: A,
        binding: &[u8],
    ) -> Result<Self, ApiKeyError> {
        let fingerprint = CredentialGenerationFingerprint::mint_exact(provider, binding)
            .map_err(|_| ApiKeyError::InvalidGenerationBinding)?;
        Ok(Self {
            provider,
            authority,
            fingerprint,
        })
    }

    /// Exact credential provider, never inferred from a key's spelling.
    #[must_use]
    pub const fn provider(&self) -> CredentialProviderTag {
        self.provider
    }

    /// Source-owned authority retained by this grant.
    #[must_use]
    pub const fn authority(&self) -> &A {
        &self.authority
    }

    /// Exact non-secret generation fingerprint.
    #[must_use]
    pub const fn fingerprint(&self) -> CredentialGenerationFingerprint {
        self.fingerprint
    }

    /// The unchanged owner-only model-list cache key for this generation.
    #[must_use]
    pub fn listing_fingerprint(&self) -> String {
        let mut fingerprint = self.fingerprint.to_hex();
        fingerprint.truncate(16);
        fingerprint
    }
}

impl<A> fmt::Debug for ApiKeyGenerationGrant<A> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ApiKeyGenerationGrant(<redacted>)")
    }
}

/// Route data exposed by a credential request, without its admission proof.
///
/// The four built-in integrations construct their concrete request types
/// privately. Applications can implement metadata selections for generation
/// lookup, but those selections do not satisfy the concrete bearer-request
/// type required by an integration's source.
pub trait ApiKeyRequest {
    /// Exact provider domain retained by the selected route.
    fn provider(&self) -> CredentialProviderTag;
    /// Exact credential audience; no origin fallback is permitted.
    fn origin(&self) -> &str;
    /// Application-owned credential reference, when the route uses one.
    fn credential_reference(&self) -> Option<&str> {
        None
    }
}

/// Application-owned generation and bearer resolution for one typed route.
pub trait ApiKeySource<R: ApiKeyRequest + ?Sized, A = ()>: Send + Sync {
    /// Stable, non-secret source-owned account/authority identity.
    fn authority(&self) -> &A;

    /// Resolves only non-secret generation metadata before paid reservation.
    ///
    /// # Errors
    /// Returns a redaction-safe source or exact-binding error.
    fn generation_grant(
        &self,
        selection: &dyn ApiKeyRequest,
    ) -> Result<ApiKeyGenerationGrant<A>, ApiKeyError>;

    /// Resolves the bearer only while the admitted generation remains current.
    ///
    /// # Errors
    /// Returns a redaction-safe source or exact-binding error.
    fn api_key(
        &self,
        request: &R,
        expected: &ApiKeyGenerationGrant<A>,
    ) -> Result<ApiKey, ApiKeyError>;
}

/// A single injected key bound to one exact audience and source authority.
pub struct StaticApiKeySource<A = ()> {
    key: ApiKey,
    origin: String,
    reference: Option<String>,
    grant: ApiKeyGenerationGrant<A>,
}

impl<A> StaticApiKeySource<A> {
    /// The source-owned authority, without selecting a request or exposing a key.
    #[must_use]
    pub const fn authority(&self) -> &A {
        &self.grant.authority
    }

    /// Binds a key using its bytes as the exact generation identity.
    #[must_use]
    pub fn for_key(
        key: ApiKey,
        provider: CredentialProviderTag,
        origin: impl Into<String>,
        reference: Option<String>,
        authority: A,
    ) -> Self {
        let grant = key.generation_grant(provider, authority);
        Self::new(key, origin, reference, grant)
    }

    /// Binds a key and a non-secret grant to an exact audience and reference.
    #[must_use]
    pub fn new(
        key: ApiKey,
        origin: impl Into<String>,
        reference: Option<String>,
        grant: ApiKeyGenerationGrant<A>,
    ) -> Self {
        Self {
            key,
            origin: origin.into(),
            reference,
            grant,
        }
    }

    fn accepts<R: ApiKeyRequest + ?Sized>(&self, request: &R) -> bool {
        request.provider() == self.grant.provider
            && request.origin() == self.origin
            && request.credential_reference() == self.reference.as_deref()
    }

    /// Resolves only metadata for an exact audience/reference selection.
    ///
    /// # Errors
    /// Refuses a different provider, audience or credential reference.
    pub fn generation_grant(
        &self,
        selection: &dyn ApiKeyRequest,
    ) -> Result<ApiKeyGenerationGrant<A>, ApiKeyError>
    where
        A: Clone,
    {
        if !self.accepts(selection) {
            return Err(ApiKeyError::BindingMismatch);
        }
        Ok(self.grant.clone())
    }
}

impl<R, A> ApiKeySource<R, A> for StaticApiKeySource<A>
where
    R: ApiKeyRequest + ?Sized,
    A: Clone + Eq + Send + Sync,
{
    fn authority(&self) -> &A {
        &self.grant.authority
    }

    fn generation_grant(
        &self,
        selection: &dyn ApiKeyRequest,
    ) -> Result<ApiKeyGenerationGrant<A>, ApiKeyError> {
        Self::generation_grant(self, selection)
    }

    fn api_key(
        &self,
        request: &R,
        expected: &ApiKeyGenerationGrant<A>,
    ) -> Result<ApiKey, ApiKeyError> {
        if !self.accepts(request) || expected != &self.grant {
            return Err(ApiKeyError::BindingMismatch);
        }
        ApiKey::new(self.key.expose_secret())
    }
}

impl<A> fmt::Debug for StaticApiKeySource<A> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("StaticApiKeySource(<redacted>)")
    }
}

/// Redaction-safe API-key construction and resolution failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ApiKeyError {
    /// The key is empty, oversized or contains control bytes.
    #[error("API key must be bounded, non-empty, and contain no control bytes")]
    InvalidKey,
    /// The generation binding is empty or oversized.
    #[error("invalid API-key generation binding")]
    InvalidGenerationBinding,
    /// The exact audience, source authority or admitted generation changed.
    #[error("API-key credential binding mismatch")]
    BindingMismatch,
    /// The selected credential owner could not supply its credential.
    #[error("API-key credential source is unavailable")]
    Unavailable,
}

#[cfg(test)]
mod tests;
