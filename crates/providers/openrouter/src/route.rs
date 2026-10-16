//! Immutable OpenRouter routes and origin-bound credential sources.

use std::fmt;
use std::sync::Arc;

use plakous_model::{Digest, DigestDomain, DigestError, IdentityError, RouteId};
use secrecy::{ExposeSecret as _, SecretString as SecrecyString};
use thiserror::Error;

use plakous_chat_completions::ChatPhysicalRequestBinding;
use plakous_messages::MessagesPhysicalRequestBinding;
use plakous_responses_dialect::{CredentialGenerationFingerprint, ResponsesPhysicalRequestBinding};

use crate::{OpenRouterAccessMode, OpenRouterRouteKind};

const OPENROUTER_PLATFORM_ORIGIN: &str = "https://openrouter.ai";
/// Exact upstream endpoint selection used by OpenRouter's provider router.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct OpenRouterRoutingPolicy {
    endpoint: String,
    provider_name: String,
}

impl OpenRouterRoutingPolicy {
    /// Creates a strict one-endpoint policy with fallbacks disabled.
    ///
    /// # Errors
    ///
    /// Returns an error unless `endpoint` is a bounded canonical provider
    /// endpoint slug.
    pub fn strict(
        endpoint: impl Into<String>,
        provider_name: impl Into<String>,
    ) -> Result<Self, OpenRouterRouteError> {
        let endpoint = endpoint.into();
        let provider_name = provider_name.into();
        if endpoint.is_empty()
            || endpoint.len() > 256
            || !endpoint.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
            })
            || endpoint.split('/').any(str::is_empty)
            || endpoint.contains(':')
            || endpoint.split('/').any(reserved_router_identity)
            || !valid_provider_name(&provider_name)
        {
            return Err(OpenRouterRouteError::InvalidRoutingPolicy);
        }
        Ok(Self {
            endpoint,
            provider_name,
        })
    }

    /// Returns the exact full endpoint slug used for both `only` and `order`.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Returns the exact provider identity expected in router metadata.
    #[must_use]
    pub fn provider_name(&self) -> &str {
        &self.provider_name
    }

    /// Reports that backup provider endpoints are forbidden.
    #[must_use]
    pub const fn allows_fallbacks(&self) -> bool {
        false
    }

    /// Reports that every emitted parameter must be supported upstream.
    #[must_use]
    pub const fn requires_parameters(&self) -> bool {
        true
    }
}

fn valid_provider_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b' ' | b'-' | b'_' | b'.'))
}

fn reserved_router_identity(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "auto" | "free" | "latest" | "router" | "routers" | "preset" | "presets"
    )
}

/// Exact retention and provider-data policy used by a route.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenRouterRetentionPolicy {
    /// Use OpenRouter's documented gateway-default retention behavior.
    GatewayDefault,
    /// Restrict routing to no-collection, zero-data-retention endpoints.
    Strict,
}

impl OpenRouterRetentionPolicy {
    /// Creates the strict no-collection, ZDR-required policy.
    #[must_use]
    pub const fn strict() -> Self {
        Self::Strict
    }

    /// Creates the gateway-default retention policy without stronger claims.
    #[must_use]
    pub const fn gateway_default() -> Self {
        Self::GatewayDefault
    }

    /// Returns the provider-router `data_collection` spelling when constrained.
    #[must_use]
    pub const fn data_collection(self) -> Option<&'static str> {
        match self {
            Self::GatewayDefault => None,
            Self::Strict => Some("deny"),
        }
    }

    /// Reports that zero-data-retention endpoints are required.
    #[must_use]
    pub const fn requires_zdr(self) -> bool {
        matches!(self, Self::Strict)
    }

    /// Returns the provider-router `zdr` value when constrained.
    #[must_use]
    pub const fn zdr(self) -> Option<bool> {
        match self {
            Self::GatewayDefault => None,
            Self::Strict => Some(true),
        }
    }

    const fn semantic_label(self) -> &'static [u8] {
        match self {
            Self::GatewayDefault => b"gateway-default",
            Self::Strict => b"data-collection=deny;zdr=true",
        }
    }
}

/// Truthful optional OpenRouter application attribution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpenRouterAttribution;

impl OpenRouterAttribution {
    /// Returns Plak's public project URL for `HTTP-Referer`.
    #[must_use]
    pub const fn referer(self) -> &'static str {
        "https://github.com/N0xMare/plak"
    }

    /// Returns Plak's truthful terminal coding assistant category.
    pub(crate) const CATEGORY: &'static str = "cli-agent";

    /// Returns the truthful application title.
    #[must_use]
    pub const fn title(self) -> &'static str {
        "Plak"
    }
}

/// A validated HTTPS origin without path, query, fragment, or user information.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct OpenRouterOrigin(String);

impl OpenRouterOrigin {
    /// Validates an OpenRouter route origin.
    ///
    /// # Errors
    ///
    /// Returns [`OpenRouterRouteError::InvalidOrigin`] unless the value is a bare
    /// HTTPS origin with no embedded credentials or request path.
    pub fn new(value: impl Into<String>) -> Result<Self, OpenRouterRouteError> {
        let value = value.into();
        let origin =
            plakous_http::Origin::parse(&value).map_err(|_| OpenRouterRouteError::InvalidOrigin)?;
        Ok(Self(format!("https://{}", origin.binding_authority())))
    }

    /// Returns the normalized origin text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for OpenRouterOrigin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("OpenRouterOrigin")
            .field(&self.0)
            .finish()
    }
}

fn local_fixture_origin(origin: &OpenRouterOrigin) -> bool {
    let authority = origin.as_str().strip_prefix("https://").unwrap_or_default();
    let Some((host, port)) = authority.rsplit_once(':') else {
        return false;
    };
    matches!(host, "127.0.0.1" | "localhost") && port.parse::<u16>().is_ok_and(|port| port != 0)
}

/// Typed account and authority identity retained by a credential source.
///
/// A route receives this value from its selected source; route constructors do
/// not accept an unrelated caller label.
#[derive(Clone, Eq, PartialEq)]
pub struct OpenRouterCredentialBinding {
    kind: OpenRouterRouteKind,
    account_binding: String,
    credential_authority: String,
}

impl OpenRouterCredentialBinding {
    /// Creates a Platform account/authority binding owned by an API-key source.
    ///
    /// # Errors
    ///
    /// Returns an error for empty, oversized, or non-canonical identifiers.
    pub fn platform(
        account_binding: impl Into<String>,
        credential_authority: impl Into<String>,
    ) -> Result<Self, OpenRouterRouteError> {
        Self::new(
            OpenRouterRouteKind::Platform,
            account_binding.into(),
            credential_authority.into(),
        )
    }

    fn new(
        kind: OpenRouterRouteKind,
        account_binding: String,
        credential_authority: String,
    ) -> Result<Self, OpenRouterRouteError> {
        if !valid_binding_component(&account_binding)
            || !valid_binding_component(&credential_authority)
        {
            return Err(OpenRouterRouteError::InvalidCredentialBinding);
        }
        Ok(Self {
            kind,
            account_binding,
            credential_authority,
        })
    }

    /// Returns the route family authorized by the source.
    #[must_use]
    pub const fn kind(&self) -> OpenRouterRouteKind {
        self.kind
    }

    /// Returns the opaque stable account binding.
    #[must_use]
    pub fn account_binding(&self) -> &str {
        &self.account_binding
    }

    /// Returns the non-secret source authority identifier.
    #[must_use]
    pub fn credential_authority(&self) -> &str {
        &self.credential_authority
    }
}

impl fmt::Debug for OpenRouterCredentialBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenRouterCredentialBinding")
            .field("kind", &self.kind)
            .field("account_binding", &"<redacted>")
            .field("credential_authority", &"<redacted>")
            .finish()
    }
}

fn valid_binding_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        })
}

/// A secret OpenRouter Platform API key.
///
/// Non-cloneable and backed by [`secrecy::SecretString`] (zeroized on drop).
pub struct OpenRouterApiKey {
    value: SecrecyString,
    generation: CredentialGenerationFingerprint,
}

impl OpenRouterApiKey {
    /// Creates an injected API key.
    ///
    /// # Errors
    ///
    /// Returns [`OpenRouterAuthError::MissingCredential`] for an empty value.
    pub fn new(value: impl Into<String>) -> Result<Self, OpenRouterAuthError> {
        let value = value.into();
        if value.is_empty() {
            return Err(OpenRouterAuthError::MissingCredential);
        }
        let generation = mint_openrouter_generation(value.as_bytes())
            .map_err(|_| OpenRouterAuthError::CredentialSource)?;
        Ok(Self {
            value: SecrecyString::from(value),
            generation,
        })
    }

    fn expose(&self) -> &str {
        self.value.expose_secret()
    }

    pub(crate) fn bearer_bytes(&self) -> Vec<u8> {
        let mut value = Vec::with_capacity(7 + self.expose().len());
        value.extend_from_slice(b"Bearer ");
        value.extend_from_slice(self.expose().as_bytes());
        value
    }

    /// Returns a non-secret grant for this exact key generation.
    ///
    /// Credential sources can retain this value at construction and later
    /// return it from [`OpenRouterApiKeySource::generation_grant`] without
    /// retaining the original generation-binding bytes or resolving secret
    /// material before paid reservation.
    #[must_use]
    pub const fn generation_grant(&self) -> OpenRouterApiKeyGenerationGrant {
        OpenRouterApiKeyGenerationGrant {
            fingerprint: self.generation,
        }
    }
}

impl fmt::Debug for OpenRouterApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OpenRouterApiKey(<redacted>)")
    }
}

impl fmt::Display for OpenRouterApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

/// Non-secret proof of one exact resolved OpenRouter Platform API-key generation.
///
/// It is safe to retain in a paid admission, but cannot be used to reconstruct
/// the key or the credential-owner generation binding.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct OpenRouterApiKeyGenerationGrant {
    fingerprint: CredentialGenerationFingerprint,
}

impl OpenRouterApiKeyGenerationGrant {
    /// Derives a grant from bounded canonical bytes unique to one current key generation.
    ///
    /// This constructor lets credential owners use a non-secret generation
    /// nonce or durable credential fence without exposing key material.
    ///
    /// # Errors
    ///
    /// Rejects an empty or over-64-KiB generation binding.
    pub fn from_generation_binding(generation_binding: &[u8]) -> Result<Self, OpenRouterAuthError> {
        mint_openrouter_generation(generation_binding)
            .map(|fingerprint| Self { fingerprint })
            .map_err(|_| OpenRouterAuthError::CredentialSource)
    }

    fn from_key(key: &OpenRouterApiKey) -> Self {
        key.generation_grant()
    }

    /// Returns the exact non-secret generation fingerprint.
    #[must_use]
    pub const fn fingerprint(self) -> CredentialGenerationFingerprint {
        self.fingerprint
    }

    fn matches_key(self, key: &OpenRouterApiKey) -> bool {
        self == Self::from_key(key)
    }
}

impl fmt::Debug for OpenRouterApiKeyGenerationGrant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenRouterApiKeyGenerationGrant")
            .field("fingerprint", &self.fingerprint)
            .finish()
    }
}

fn mint_openrouter_generation(
    binding: &[u8],
) -> Result<
    CredentialGenerationFingerprint,
    plakous_responses_dialect::CredentialGenerationFingerprintError,
> {
    CredentialGenerationFingerprint::mint_exact(
        plakous_responses_dialect::CredentialProviderTag::OpenRouterPlatform,
        binding,
    )
}

/// Resolves an OpenRouter Platform API key at dispatch time.
pub trait OpenRouterApiKeySource: Send + Sync + 'static {
    /// Returns the typed account/authority identity owned by this source.
    fn credential_binding(&self) -> Option<&OpenRouterCredentialBinding> {
        None
    }

    /// Resolves the currently active key for a prevalidated admitted attempt.
    ///
    /// # Errors
    ///
    /// Returns a typed authentication error without embedding secret material.
    fn resolve(
        &self,
        request: &OpenRouterCredentialRequest<'_>,
    ) -> Result<OpenRouterApiKey, OpenRouterAuthError>;

    /// Resolves a non-secret grant for the exact currently active key generation.
    ///
    /// Sources return only non-secret generation metadata here. The
    /// compatibility default fails closed so paid denial can never materialize
    /// the key before reservation.
    ///
    /// # Errors
    ///
    /// Returns a redaction-safe credential-source error.
    fn generation_grant(
        &self,
        _request: &OpenRouterCredentialRequest<'_>,
    ) -> Result<OpenRouterApiKeyGenerationGrant, OpenRouterAuthError> {
        self.current_generation_grant()
    }

    /// Resolves a key for one exact admitted Chat physical request.
    fn resolve_chat(
        &self,
        _request: &OpenRouterChatCredentialRequest<'_>,
    ) -> Result<OpenRouterApiKey, OpenRouterAuthError> {
        Err(OpenRouterAuthError::CredentialSource)
    }

    /// Returns the generation for one exact admitted Chat physical request.
    fn generation_grant_chat(
        &self,
        _request: &OpenRouterChatCredentialRequest<'_>,
    ) -> Result<OpenRouterApiKeyGenerationGrant, OpenRouterAuthError> {
        self.current_generation_grant()
    }

    /// Resolves a key for one exact admitted Messages physical request.
    fn resolve_messages(
        &self,
        _request: &OpenRouterMessagesCredentialRequest<'_>,
    ) -> Result<OpenRouterApiKey, OpenRouterAuthError> {
        Err(OpenRouterAuthError::CredentialSource)
    }

    /// Returns the generation for one exact admitted Messages physical request.
    fn generation_grant_messages(
        &self,
        _request: &OpenRouterMessagesCredentialRequest<'_>,
    ) -> Result<OpenRouterApiKeyGenerationGrant, OpenRouterAuthError> {
        self.current_generation_grant()
    }

    /// Resolves a key for one exact authenticated catalog operation.
    fn resolve_catalog(
        &self,
        _request: &OpenRouterCatalogCredentialRequest<'_>,
    ) -> Result<OpenRouterApiKey, OpenRouterAuthError> {
        Err(OpenRouterAuthError::CredentialSource)
    }

    /// Returns the generation for one exact authenticated catalog operation.
    fn generation_grant_catalog(
        &self,
        _request: &OpenRouterCatalogCredentialRequest<'_>,
    ) -> Result<OpenRouterApiKeyGenerationGrant, OpenRouterAuthError> {
        self.current_generation_grant()
    }

    /// Resolves a catalog key only while the admitted generation remains exact.
    fn resolve_generation_catalog(
        &self,
        request: &OpenRouterCatalogCredentialRequest<'_>,
        expected: OpenRouterApiKeyGenerationGrant,
    ) -> Result<OpenRouterApiKey, OpenRouterAuthError> {
        let key = self.resolve_catalog(request)?;
        if !expected.matches_key(&key) {
            return Err(OpenRouterAuthError::BindingMismatch);
        }
        Ok(key)
    }

    /// Resolves a key for one exact hosted-retrieval side request.
    fn resolve_retrieval(
        &self,
        _request: &OpenRouterRetrievalCredentialRequest<'_>,
    ) -> Result<OpenRouterApiKey, OpenRouterAuthError> {
        Err(OpenRouterAuthError::CredentialSource)
    }

    /// Returns the generation for one exact hosted-retrieval side request.
    fn generation_grant_retrieval(
        &self,
        _request: &OpenRouterRetrievalCredentialRequest<'_>,
    ) -> Result<OpenRouterApiKeyGenerationGrant, OpenRouterAuthError> {
        self.current_generation_grant()
    }

    /// Resolves a side-request key only while the admitted generation remains exact.
    fn resolve_generation_retrieval(
        &self,
        request: &OpenRouterRetrievalCredentialRequest<'_>,
        expected: OpenRouterApiKeyGenerationGrant,
    ) -> Result<OpenRouterApiKey, OpenRouterAuthError> {
        let key = self.resolve_retrieval(request)?;
        if !expected.matches_key(&key) {
            return Err(OpenRouterAuthError::BindingMismatch);
        }
        Ok(key)
    }

    /// Resolves the Chat key only while the admitted generation remains exact.
    fn resolve_generation_chat(
        &self,
        request: &OpenRouterChatCredentialRequest<'_>,
        expected: OpenRouterApiKeyGenerationGrant,
    ) -> Result<OpenRouterApiKey, OpenRouterAuthError> {
        let key = self.resolve_chat(request)?;
        if !expected.matches_key(&key) {
            return Err(OpenRouterAuthError::BindingMismatch);
        }
        Ok(key)
    }

    /// Resolves the Messages key only while the admitted generation remains exact.
    fn resolve_generation_messages(
        &self,
        request: &OpenRouterMessagesCredentialRequest<'_>,
        expected: OpenRouterApiKeyGenerationGrant,
    ) -> Result<OpenRouterApiKey, OpenRouterAuthError> {
        let key = self.resolve_messages(request)?;
        if !expected.matches_key(&key) {
            return Err(OpenRouterAuthError::BindingMismatch);
        }
        Ok(key)
    }

    /// Returns the current non-secret credential-generation authority before
    /// one physical attempt is constructed.
    ///
    /// This value authorizes only runtime binding and paid reservation. Secret
    /// resolution still requires an exact physical request and revalidates the
    /// same generation at the transport fence.
    fn current_generation_grant(
        &self,
    ) -> Result<OpenRouterApiKeyGenerationGrant, OpenRouterAuthError> {
        Err(OpenRouterAuthError::CredentialSource)
    }

    /// Resolves the key only if the same exact generation remains current.
    ///
    /// # Errors
    ///
    /// Fails closed when rotation occurred after paid reservation.
    fn resolve_generation(
        &self,
        request: &OpenRouterCredentialRequest<'_>,
        expected: OpenRouterApiKeyGenerationGrant,
    ) -> Result<OpenRouterApiKey, OpenRouterAuthError> {
        let key = self.resolve(request)?;
        if !expected.matches_key(&key) {
            return Err(OpenRouterAuthError::BindingMismatch);
        }
        Ok(key)
    }
}

/// A caller-injected in-memory API-key source.
pub struct StaticOpenRouterApiKeySource {
    key: OpenRouterApiKey,
    generation_grant: OpenRouterApiKeyGenerationGrant,
    credential_binding: OpenRouterCredentialBinding,
}

impl StaticOpenRouterApiKeySource {
    /// Creates a static source with a source-owned typed route binding.
    ///
    /// # Errors
    ///
    /// Returns a route error for an invalid account or authority identity.
    pub fn new(
        key: OpenRouterApiKey,
        account_binding: impl Into<String>,
        authority_id: impl Into<String>,
    ) -> Result<Self, OpenRouterRouteError> {
        let generation_grant = OpenRouterApiKeyGenerationGrant::from_key(&key);
        Ok(Self {
            key,
            generation_grant,
            credential_binding: OpenRouterCredentialBinding::platform(
                account_binding,
                authority_id,
            )?,
        })
    }

    /// Creates a static source with an explicit non-secret generation binding.
    ///
    /// This is the preferred constructor for credential stores that already own
    /// a rotation nonce or durable generation fence.
    ///
    /// # Errors
    ///
    /// Returns a route error for an invalid account, authority, or generation identity.
    pub fn new_with_generation_binding(
        key: OpenRouterApiKey,
        account_binding: impl Into<String>,
        authority_id: impl Into<String>,
        generation_binding: &[u8],
    ) -> Result<Self, OpenRouterRouteError> {
        let generation_grant =
            OpenRouterApiKeyGenerationGrant::from_generation_binding(generation_binding)
                .map_err(|_| OpenRouterRouteError::InvalidCredentialBinding)?;
        Ok(Self {
            key,
            generation_grant,
            credential_binding: OpenRouterCredentialBinding::platform(
                account_binding,
                authority_id,
            )?,
        })
    }
}

impl fmt::Debug for StaticOpenRouterApiKeySource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StaticOpenRouterApiKeySource")
            .field("key", &"<redacted>")
            .field("generation_grant", &self.generation_grant)
            .field("credential_binding", &self.credential_binding)
            .finish()
    }
}

impl OpenRouterApiKeySource for StaticOpenRouterApiKeySource {
    fn credential_binding(&self) -> Option<&OpenRouterCredentialBinding> {
        Some(&self.credential_binding)
    }

    fn resolve(
        &self,
        _request: &OpenRouterCredentialRequest<'_>,
    ) -> Result<OpenRouterApiKey, OpenRouterAuthError> {
        OpenRouterApiKey::new(self.key.expose())
    }

    fn generation_grant(
        &self,
        _request: &OpenRouterCredentialRequest<'_>,
    ) -> Result<OpenRouterApiKeyGenerationGrant, OpenRouterAuthError> {
        Ok(self.generation_grant)
    }

    fn resolve_chat(
        &self,
        _request: &OpenRouterChatCredentialRequest<'_>,
    ) -> Result<OpenRouterApiKey, OpenRouterAuthError> {
        OpenRouterApiKey::new(self.key.expose())
    }

    fn generation_grant_chat(
        &self,
        _request: &OpenRouterChatCredentialRequest<'_>,
    ) -> Result<OpenRouterApiKeyGenerationGrant, OpenRouterAuthError> {
        Ok(self.generation_grant)
    }

    fn resolve_messages(
        &self,
        _request: &OpenRouterMessagesCredentialRequest<'_>,
    ) -> Result<OpenRouterApiKey, OpenRouterAuthError> {
        OpenRouterApiKey::new(self.key.expose())
    }

    fn generation_grant_messages(
        &self,
        _request: &OpenRouterMessagesCredentialRequest<'_>,
    ) -> Result<OpenRouterApiKeyGenerationGrant, OpenRouterAuthError> {
        Ok(self.generation_grant)
    }

    fn resolve_generation_chat(
        &self,
        request: &OpenRouterChatCredentialRequest<'_>,
        expected: OpenRouterApiKeyGenerationGrant,
    ) -> Result<OpenRouterApiKey, OpenRouterAuthError> {
        if expected != self.generation_grant {
            return Err(OpenRouterAuthError::BindingMismatch);
        }
        self.resolve_chat(request)
    }

    fn resolve_generation_messages(
        &self,
        request: &OpenRouterMessagesCredentialRequest<'_>,
        expected: OpenRouterApiKeyGenerationGrant,
    ) -> Result<OpenRouterApiKey, OpenRouterAuthError> {
        if expected != self.generation_grant {
            return Err(OpenRouterAuthError::BindingMismatch);
        }
        self.resolve_messages(request)
    }

    fn resolve_catalog(
        &self,
        _request: &OpenRouterCatalogCredentialRequest<'_>,
    ) -> Result<OpenRouterApiKey, OpenRouterAuthError> {
        OpenRouterApiKey::new(self.key.expose())
    }

    fn generation_grant_catalog(
        &self,
        _request: &OpenRouterCatalogCredentialRequest<'_>,
    ) -> Result<OpenRouterApiKeyGenerationGrant, OpenRouterAuthError> {
        Ok(self.generation_grant)
    }

    fn resolve_generation_catalog(
        &self,
        request: &OpenRouterCatalogCredentialRequest<'_>,
        expected: OpenRouterApiKeyGenerationGrant,
    ) -> Result<OpenRouterApiKey, OpenRouterAuthError> {
        if expected != self.generation_grant {
            return Err(OpenRouterAuthError::BindingMismatch);
        }
        self.resolve_catalog(request)
    }

    fn resolve_retrieval(
        &self,
        _request: &OpenRouterRetrievalCredentialRequest<'_>,
    ) -> Result<OpenRouterApiKey, OpenRouterAuthError> {
        OpenRouterApiKey::new(self.key.expose())
    }

    fn generation_grant_retrieval(
        &self,
        _request: &OpenRouterRetrievalCredentialRequest<'_>,
    ) -> Result<OpenRouterApiKeyGenerationGrant, OpenRouterAuthError> {
        Ok(self.generation_grant)
    }

    fn resolve_generation_retrieval(
        &self,
        request: &OpenRouterRetrievalCredentialRequest<'_>,
        expected: OpenRouterApiKeyGenerationGrant,
    ) -> Result<OpenRouterApiKey, OpenRouterAuthError> {
        if expected != self.generation_grant {
            return Err(OpenRouterAuthError::BindingMismatch);
        }
        self.resolve_retrieval(request)
    }

    fn current_generation_grant(
        &self,
    ) -> Result<OpenRouterApiKeyGenerationGrant, OpenRouterAuthError> {
        Ok(self.generation_grant)
    }

    fn resolve_generation(
        &self,
        request: &OpenRouterCredentialRequest<'_>,
        expected: OpenRouterApiKeyGenerationGrant,
    ) -> Result<OpenRouterApiKey, OpenRouterAuthError> {
        if expected != self.generation_grant {
            return Err(OpenRouterAuthError::BindingMismatch);
        }
        self.resolve(request)
    }
}

/// Proof that an exact admitted attempt was validated against the captured route.
///
/// Construction is crate-private, so credential sources cannot be invoked
/// through a zero-argument or origin-only route path.
pub struct OpenRouterCredentialRequest<'a> {
    binding: &'a ResponsesPhysicalRequestBinding,
}

/// Proof that a Chat credential resolution names one admitted physical request.
pub struct OpenRouterChatCredentialRequest<'a> {
    binding: &'a ChatPhysicalRequestBinding,
}

/// Proof that Messages credential resolution names one admitted physical request.
pub struct OpenRouterMessagesCredentialRequest<'a> {
    binding: &'a MessagesPhysicalRequestBinding,
}

impl OpenRouterMessagesCredentialRequest<'_> {
    /// Returns the exact admitted Messages physical-attempt binding.
    #[must_use]
    pub const fn physical_binding(&self) -> &MessagesPhysicalRequestBinding {
        self.binding
    }
}

impl fmt::Debug for OpenRouterMessagesCredentialRequest<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenRouterMessagesCredentialRequest")
            .field("binding", &self.binding)
            .finish()
    }
}

/// Proof that credential resolution belongs to one exact catalog operation.
pub struct OpenRouterCatalogCredentialRequest<'a> {
    operation: &'a Digest,
}

impl OpenRouterCatalogCredentialRequest<'_> {
    /// Returns the exact non-secret catalog-operation identity.
    #[must_use]
    pub const fn operation(&self) -> Digest {
        *self.operation
    }
}

impl fmt::Debug for OpenRouterCatalogCredentialRequest<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenRouterCatalogCredentialRequest")
            .field("operation", self.operation)
            .finish()
    }
}

/// Proof that credential resolution belongs to one exact hosted-retrieval
/// side request: the digest names the route and the exact body it carries.
pub struct OpenRouterRetrievalCredentialRequest<'a> {
    operation: &'a Digest,
}

impl OpenRouterRetrievalCredentialRequest<'_> {
    /// Returns the exact non-secret side-request identity.
    #[must_use]
    pub const fn operation(&self) -> Digest {
        *self.operation
    }
}

impl fmt::Debug for OpenRouterRetrievalCredentialRequest<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenRouterRetrievalCredentialRequest")
            .field("operation", self.operation)
            .finish()
    }
}

impl OpenRouterChatCredentialRequest<'_> {
    /// Returns the exact admitted Chat physical-attempt binding.
    #[must_use]
    pub const fn physical_binding(&self) -> &ChatPhysicalRequestBinding {
        self.binding
    }
}

impl fmt::Debug for OpenRouterChatCredentialRequest<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenRouterChatCredentialRequest")
            .field("binding", &self.binding)
            .finish()
    }
}

impl OpenRouterCredentialRequest<'_> {
    /// Returns the exact admitted physical-attempt binding.
    #[must_use]
    pub const fn physical_binding(&self) -> &ResponsesPhysicalRequestBinding {
        self.binding
    }
}

impl fmt::Debug for OpenRouterCredentialRequest<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenRouterCredentialRequest")
            .field("binding", &self.binding)
            .finish()
    }
}

/// Routing-neutral identity for OpenRouter Chat and catalog operations.
///
/// Endpoint/provider selection deliberately does not appear here. It belongs
/// to the separately selected Chat routing policy and fresh discovery evidence.
#[derive(Clone, Eq, PartialEq)]
pub struct OpenRouterChatRouteIdentity {
    route_id: RouteId,
    fingerprint: String,
    origin: OpenRouterOrigin,
    access_mode: OpenRouterAccessMode,
    account_binding: Digest,
    credential_authority_binding: Digest,
    retention_policy: Option<OpenRouterRetentionPolicy>,
    attribution: Option<OpenRouterAttribution>,
    credential_binding: OpenRouterCredentialBinding,
}

impl OpenRouterChatRouteIdentity {
    fn new(
        origin: OpenRouterOrigin,
        access_mode: OpenRouterAccessMode,
        credential_binding: &OpenRouterCredentialBinding,
        retention_policy: Option<OpenRouterRetentionPolicy>,
        attribution: Option<OpenRouterAttribution>,
    ) -> Result<Self, OpenRouterRouteError> {
        if credential_binding.kind() != OpenRouterRouteKind::Platform {
            return Err(OpenRouterRouteError::CredentialBindingRouteMismatch);
        }
        let account_binding = Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[
                b"plakous-openrouter-chat-account-binding-v1",
                origin.as_str().as_bytes(),
                credential_binding.account_binding().as_bytes(),
            ],
        )?;
        let credential_authority_binding = Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[
                b"plakous-openrouter-chat-credential-authority-binding-v1",
                origin.as_str().as_bytes(),
                credential_binding.credential_authority().as_bytes(),
            ],
        )?;
        let access = match access_mode {
            OpenRouterAccessMode::PlatformApiKey => b"platform-api-key".as_slice(),
            OpenRouterAccessMode::LocalFixture => b"local-fixture".as_slice(),
        };
        let digest = Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[
                b"plakous-openrouter/chat-route-v1",
                origin.as_str().as_bytes(),
                account_binding.bytes(),
                credential_authority_binding.bytes(),
                access,
                retention_policy.map_or(b"retention=omitted".as_slice(), |policy| {
                    policy.semantic_label()
                }),
                attribution.map_or(b"attribution=none".as_slice(), |_| {
                    b"attribution=https://github.com/N0xMare/plak;Plak;cli-agent".as_slice()
                }),
            ],
        )?;
        let fingerprint = digest.to_string();
        let route_id = RouteId::new(format!("openrouter.chat.platform:{fingerprint}"))?;
        Ok(Self {
            route_id,
            fingerprint,
            origin,
            access_mode,
            account_binding,
            credential_authority_binding,
            retention_policy,
            attribution,
            credential_binding: credential_binding.clone(),
        })
    }

    /// Returns the neutral route identifier.
    #[must_use]
    pub const fn route_id(&self) -> &RouteId {
        &self.route_id
    }

    /// Returns the exact route fingerprint.
    #[must_use]
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// Returns the immutable origin.
    #[must_use]
    pub const fn origin(&self) -> &OpenRouterOrigin {
        &self.origin
    }

    /// Returns the access/billing mode.
    #[must_use]
    pub const fn access_mode(&self) -> OpenRouterAccessMode {
        self.access_mode
    }

    /// Reports whether the route is a loopback-only test fixture.
    #[must_use]
    pub const fn is_local_fixture(&self) -> bool {
        matches!(self.access_mode, OpenRouterAccessMode::LocalFixture)
    }

    /// Returns the opaque account binding.
    #[must_use]
    pub const fn account_binding(&self) -> Digest {
        self.account_binding
    }

    /// Returns the opaque credential-authority binding.
    #[must_use]
    pub const fn credential_authority_binding(&self) -> Digest {
        self.credential_authority_binding
    }

    /// Returns the retention/data policy.
    #[must_use]
    pub const fn retention_policy(&self) -> Option<OpenRouterRetentionPolicy> {
        self.retention_policy
    }

    /// Returns truthful optional application attribution.
    #[must_use]
    pub const fn attribution(&self) -> Option<OpenRouterAttribution> {
        self.attribution
    }

    fn credential_binding(&self) -> &OpenRouterCredentialBinding {
        &self.credential_binding
    }
}

impl fmt::Debug for OpenRouterChatRouteIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenRouterChatRouteIdentity")
            .field("route_id", &self.route_id)
            .field("fingerprint", &self.fingerprint)
            .field("origin", &self.origin)
            .field("access_mode", &self.access_mode)
            .field("account_binding", &"<redacted digest>")
            .field("credential_authority_binding", &"<redacted digest>")
            .field("retention_policy", &self.retention_policy)
            .field("attribution", &self.attribution)
            .finish_non_exhaustive()
    }
}

/// Exact dynamic OpenRouter route shared by catalog and family dialects.
#[derive(Clone)]
pub struct OpenRouterChatRoute {
    identity: Box<OpenRouterChatRouteIdentity>,
    key_source: Arc<dyn OpenRouterApiKeySource>,
}

impl OpenRouterChatRoute {
    /// Creates the production OpenRouter Chat route without an endpoint pin.
    pub fn new(key_source: Arc<dyn OpenRouterApiKeySource>) -> Result<Self, OpenRouterRouteError> {
        Self::construct(
            key_source,
            OpenRouterOrigin::new(OPENROUTER_PLATFORM_ORIGIN)?,
            OpenRouterAccessMode::PlatformApiKey,
            None,
        )
    }

    /// Creates a production Chat route with explicit documented retention restrictions.
    pub fn new_with_retention(
        key_source: Arc<dyn OpenRouterApiKeySource>,
        retention_policy: OpenRouterRetentionPolicy,
    ) -> Result<Self, OpenRouterRouteError> {
        Self::construct(
            key_source,
            OpenRouterOrigin::new(OPENROUTER_PLATFORM_ORIGIN)?,
            OpenRouterAccessMode::PlatformApiKey,
            Some(retention_policy),
        )
    }

    /// Creates a loopback TLS Chat/catalog route for deterministic tests.
    #[doc(hidden)]
    pub fn new_local_fixture(
        key_source: Arc<dyn OpenRouterApiKeySource>,
        origin: OpenRouterOrigin,
    ) -> Result<Self, OpenRouterRouteError> {
        if !local_fixture_origin(&origin) {
            return Err(OpenRouterRouteError::NonLocalFixtureOrigin);
        }
        Self::construct(key_source, origin, OpenRouterAccessMode::LocalFixture, None)
    }

    /// Creates a loopback TLS route with explicit retention for family dispatch tests.
    #[doc(hidden)]
    pub fn new_local_fixture_with_retention(
        key_source: Arc<dyn OpenRouterApiKeySource>,
        origin: OpenRouterOrigin,
        retention_policy: OpenRouterRetentionPolicy,
    ) -> Result<Self, OpenRouterRouteError> {
        if !local_fixture_origin(&origin) {
            return Err(OpenRouterRouteError::NonLocalFixtureOrigin);
        }
        Self::construct(
            key_source,
            origin,
            OpenRouterAccessMode::LocalFixture,
            Some(retention_policy),
        )
    }

    fn construct(
        key_source: Arc<dyn OpenRouterApiKeySource>,
        origin: OpenRouterOrigin,
        access_mode: OpenRouterAccessMode,
        retention_policy: Option<OpenRouterRetentionPolicy>,
    ) -> Result<Self, OpenRouterRouteError> {
        let credential_binding = key_source
            .credential_binding()
            .ok_or(OpenRouterRouteError::CredentialBindingUnavailable)?;
        let identity = OpenRouterChatRouteIdentity::new(
            origin,
            access_mode,
            credential_binding,
            retention_policy,
            None,
        )?;
        Ok(Self {
            identity: Box::new(identity),
            key_source,
        })
    }

    /// Returns the immutable route identity.
    #[must_use]
    pub const fn identity(&self) -> &OpenRouterChatRouteIdentity {
        &self.identity
    }

    /// Adds Plak's fixed truthful attribution without adding a route pin.
    pub fn with_plak_attribution(mut self) -> Result<Self, OpenRouterRouteError> {
        self.identity = Box::new(OpenRouterChatRouteIdentity::new(
            self.identity.origin.clone(),
            self.identity.access_mode,
            self.identity.credential_binding(),
            self.identity.retention_policy,
            Some(OpenRouterAttribution),
        )?);
        Ok(self)
    }

    pub(crate) fn current_credential_generation_grant(
        &self,
    ) -> Result<OpenRouterApiKeyGenerationGrant, OpenRouterAuthError> {
        if self.key_source.credential_binding() != Some(self.identity.credential_binding()) {
            return Err(OpenRouterAuthError::BindingMismatch);
        }
        self.key_source.current_generation_grant()
    }

    pub(crate) fn catalog_credential(
        &self,
        operation: &Digest,
        expected: OpenRouterApiKeyGenerationGrant,
    ) -> Result<OpenRouterApiKey, OpenRouterAuthError> {
        if self.key_source.credential_binding() != Some(self.identity.credential_binding()) {
            return Err(OpenRouterAuthError::BindingMismatch);
        }
        let request = OpenRouterCatalogCredentialRequest { operation };
        let current = self.key_source.generation_grant_catalog(&request)?;
        if current != expected {
            return Err(OpenRouterAuthError::BindingMismatch);
        }
        self.key_source
            .resolve_generation_catalog(&request, expected)
    }

    pub(crate) fn retrieval_credential(
        &self,
        operation: &Digest,
        expected: OpenRouterApiKeyGenerationGrant,
    ) -> Result<OpenRouterApiKey, OpenRouterAuthError> {
        if self.key_source.credential_binding() != Some(self.identity.credential_binding()) {
            return Err(OpenRouterAuthError::BindingMismatch);
        }
        let request = OpenRouterRetrievalCredentialRequest { operation };
        let current = self.key_source.generation_grant_retrieval(&request)?;
        if current != expected {
            return Err(OpenRouterAuthError::BindingMismatch);
        }
        self.key_source
            .resolve_generation_retrieval(&request, expected)
    }

    pub(crate) fn chat_credential(
        &self,
        physical: &ChatPhysicalRequestBinding,
        expected: OpenRouterApiKeyGenerationGrant,
    ) -> Result<OpenRouterApiKey, OpenRouterAuthError> {
        if self.key_source.credential_binding() != Some(self.identity.credential_binding()) {
            return Err(OpenRouterAuthError::BindingMismatch);
        }
        let request = OpenRouterChatCredentialRequest { binding: physical };
        let current = self.key_source.generation_grant_chat(&request)?;
        if current != expected {
            return Err(OpenRouterAuthError::BindingMismatch);
        }
        self.key_source.resolve_generation_chat(&request, expected)
    }

    pub(crate) fn messages_credential(
        &self,
        physical: &MessagesPhysicalRequestBinding,
        expected: OpenRouterApiKeyGenerationGrant,
    ) -> Result<OpenRouterApiKey, OpenRouterAuthError> {
        if self.key_source.credential_binding() != Some(self.identity.credential_binding()) {
            return Err(OpenRouterAuthError::BindingMismatch);
        }
        let request = OpenRouterMessagesCredentialRequest { binding: physical };
        let current = self.key_source.generation_grant_messages(&request)?;
        if current != expected {
            return Err(OpenRouterAuthError::BindingMismatch);
        }
        self.key_source
            .resolve_generation_messages(&request, expected)
    }

    pub(crate) fn responses_credential_generation_grant(
        &self,
        physical: &ResponsesPhysicalRequestBinding,
    ) -> Result<OpenRouterApiKeyGenerationGrant, OpenRouterAuthError> {
        if self.key_source.credential_binding() != Some(self.identity.credential_binding()) {
            return Err(OpenRouterAuthError::BindingMismatch);
        }
        self.key_source
            .generation_grant(&OpenRouterCredentialRequest { binding: physical })
    }

    /// Resolves the Responses Bearer key at the admitted physical-request fence.
    pub(crate) fn responses_credential(
        &self,
        physical: &ResponsesPhysicalRequestBinding,
        expected: OpenRouterApiKeyGenerationGrant,
    ) -> Result<OpenRouterApiKey, OpenRouterAuthError> {
        if self.key_source.credential_binding() != Some(self.identity.credential_binding()) {
            return Err(OpenRouterAuthError::BindingMismatch);
        }
        let request = OpenRouterCredentialRequest { binding: physical };
        let current = self.key_source.generation_grant(&request)?;
        if current != expected {
            return Err(OpenRouterAuthError::BindingMismatch);
        }
        self.key_source.resolve_generation(&request, expected)
    }

    pub(crate) fn revalidate_responses_credential_generation(
        &self,
        physical: &ResponsesPhysicalRequestBinding,
        expected: OpenRouterApiKeyGenerationGrant,
    ) -> Result<(), OpenRouterAuthError> {
        if self.responses_credential_generation_grant(physical)? != expected {
            return Err(OpenRouterAuthError::BindingMismatch);
        }
        Ok(())
    }
}

impl fmt::Debug for OpenRouterChatRoute {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenRouterChatRoute")
            .field("identity", &self.identity)
            .field("key_source", &"<redacted credential source>")
            .finish()
    }
}

/// A safe OpenRouter route-construction failure.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum OpenRouterRouteError {
    /// An origin was not a bare HTTPS origin.
    #[error("OpenRouter route origin must be a bare HTTPS origin without credentials or path")]
    InvalidOrigin,
    /// A local fixture route was not an explicit loopback host and port.
    #[error("OpenRouter local fixture origin must be HTTPS loopback with an explicit port")]
    NonLocalFixtureOrigin,
    /// A source binding component was empty, oversized, or non-canonical.
    #[error("OpenRouter credential binding must use bounded canonical identifiers")]
    InvalidCredentialBinding,
    /// A provider endpoint slug was empty, oversized, or non-canonical.
    #[error("OpenRouter routing policy requires one bounded canonical endpoint slug")]
    InvalidRoutingPolicy,
    /// The selected credential source did not expose a typed route binding.
    #[error("OpenRouter credential source has no validated route binding")]
    CredentialBindingUnavailable,
    /// The source binding was created for another route family.
    #[error("OpenRouter credential binding belongs to another route family")]
    CredentialBindingRouteMismatch,
    /// Route binding digest construction failed.
    #[error(transparent)]
    Digest(#[from] DigestError),
    /// The derived neutral route identity was invalid.
    #[error(transparent)]
    Identity(#[from] IdentityError),
}

/// A safe OpenRouter credential-resolution failure.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum OpenRouterAuthError {
    /// No credential was available from the selected source.
    #[error("the selected OpenRouter credential source is empty")]
    MissingCredential,
    /// The request target or route identity did not match the credential binding.
    #[error("OpenRouter credential cannot be used for a different origin or route identity")]
    OriginMismatch,
    /// The route or any material prepared target dimension differed.
    #[error("OpenRouter credential route does not match the exact prepared attempt binding")]
    BindingMismatch,
    /// Credential exposure was requested before exact service admission.
    #[error("OpenRouter credential exposure requires an admitted attempt")]
    AttemptNotAdmitted,
    /// Credential exposure was requested without an active physical request.
    #[error("OpenRouter credential exposure requires an active physical request")]
    NoActivePhysicalRequest,
    /// The exact physical attempt already spent its credential-resolution allowance.
    #[error("OpenRouter authorization was already resolved for this physical attempt")]
    AuthorizationAlreadyResolved,
    /// The selected credential source failed without safe detail.
    #[error("the selected OpenRouter credential source failed")]
    CredentialSource,
}
