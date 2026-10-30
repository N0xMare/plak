//! Immutable OpenRouter routes and origin-bound credential sources.

use std::fmt;
use std::sync::Arc;

pub use plakous_auth::api_key::ApiKey;
use plakous_auth::api_key::ExposeApiKey as _;
use plakous_model::{Digest, DigestDomain, DigestError, IdentityError, RouteId};
use thiserror::Error;

use plakous_chat_completions::ChatPhysicalRequestBinding;
use plakous_messages::MessagesPhysicalRequestBinding;
use plakous_responses::ResponsesPhysicalRequestBinding;

use crate::{OpenRouterAccessMode, OpenRouterRouteKind};

const OPENROUTER_PLATFORM_ORIGIN: &str = "https://openrouter.ai";

#[cfg(test)]
mod tests;
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

/// Common generation grant with the exact source-owned OpenRouter binding.
pub type OpenRouterKeyGeneration =
    plakous_auth::api_key::ApiKeyGenerationGrant<OpenRouterCredentialBinding>;
/// Common source over the concrete, privately constructed admitted request.
pub type OpenRouterKeySource = dyn for<'a> plakous_auth::api_key::ApiKeySource<
        OpenRouterCredentialRequest<'a>,
        OpenRouterCredentialBinding,
    >;
/// Common injected static source over the route's account/authority binding.
pub type StaticOpenRouterKeySource =
    plakous_auth::api_key::StaticApiKeySource<OpenRouterCredentialBinding>;

/// Binds one static key to the official audience and a source-owned identity.
///
/// # Errors
/// Rejects invalid account or authority identifiers.
pub fn static_openrouter_api_key_source(
    key: ApiKey,
    account: impl Into<String>,
    authority: impl Into<String>,
) -> Result<StaticOpenRouterKeySource, OpenRouterRouteError> {
    let binding = OpenRouterCredentialBinding::platform(account, authority)?;
    Ok(plakous_auth::api_key::StaticApiKeySource::for_key(
        key,
        plakous_auth::api_key::CredentialProviderTag::OpenRouterPlatform,
        OPENROUTER_PLATFORM_ORIGIN,
        None,
        binding,
    ))
}

/// Binds one static key to an owner-controlled non-secret generation fence.
///
/// # Errors
/// Rejects invalid account, authority or generation identifiers.
pub fn static_openrouter_api_key_source_with_generation_binding(
    key: ApiKey,
    account: impl Into<String>,
    authority: impl Into<String>,
    generation: &[u8],
) -> Result<StaticOpenRouterKeySource, OpenRouterRouteError> {
    let binding = OpenRouterCredentialBinding::platform(account, authority)?;
    let grant = OpenRouterKeyGeneration::from_generation_binding(
        plakous_auth::api_key::CredentialProviderTag::OpenRouterPlatform,
        binding,
        generation,
    )
    .map_err(|_| OpenRouterRouteError::InvalidCredentialBinding)?;
    Ok(plakous_auth::api_key::StaticApiKeySource::new(
        key,
        OPENROUTER_PLATFORM_ORIGIN,
        None,
        grant,
    ))
}

/// Public non-secret selection, never an admitted bearer-resolution request.
pub struct OpenRouterApiKeySelection(());
impl OpenRouterApiKeySelection {
    /// Names the exact official credential audience without resolving a key.
    #[must_use]
    pub const fn platform() -> Self {
        Self(())
    }
}
impl plakous_auth::api_key::ApiKeyRequest for OpenRouterApiKeySelection {
    fn provider(&self) -> plakous_auth::api_key::CredentialProviderTag {
        plakous_auth::api_key::CredentialProviderTag::OpenRouterPlatform
    }
    fn origin(&self) -> &str {
        OPENROUTER_PLATFORM_ORIGIN
    }
}

pub(crate) fn credential_generation(
    source: &OpenRouterKeySource,
    selection: &dyn plakous_auth::api_key::ApiKeyRequest,
) -> Result<OpenRouterKeyGeneration, OpenRouterAuthError> {
    let grant = source.generation_grant(selection)?;
    if grant.provider() != plakous_auth::api_key::CredentialProviderTag::OpenRouterPlatform
        || grant.authority() != source.authority()
    {
        return Err(OpenRouterAuthError::BindingMismatch);
    }
    Ok(grant)
}

fn resolve_key(
    source: &OpenRouterKeySource,
    request: &OpenRouterCredentialRequest<'_>,
    expected: &OpenRouterKeyGeneration,
) -> Result<ApiKey, OpenRouterAuthError> {
    if expected.provider() != plakous_auth::api_key::CredentialProviderTag::OpenRouterPlatform
        || expected.authority() != source.authority()
    {
        return Err(OpenRouterAuthError::BindingMismatch);
    }
    source.api_key(request, expected).map_err(Into::into)
}

pub(crate) fn bearer_bytes(key: &ApiKey) -> Vec<u8> {
    let text = key.expose_secret();
    let mut value = Vec::with_capacity(7 + text.len());
    value.extend_from_slice(b"Bearer ");
    value.extend_from_slice(text.as_bytes());
    value
}

/// Closed purpose of a privately constructed OpenRouter credential request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenRouterCredentialPurpose {
    /// Admitted Responses physical request.
    Responses,
    /// Admitted Chat physical request.
    Chat,
    /// Admitted Messages physical request.
    Messages,
    /// Exact catalog operation.
    Catalog,
    /// Exact retrieval side request.
    Retrieval,
}

#[derive(Debug)]
enum OpenRouterCredentialProof<'a> {
    Responses(&'a ResponsesPhysicalRequestBinding),
    Chat(&'a ChatPhysicalRequestBinding),
    Messages(&'a MessagesPhysicalRequestBinding),
    Catalog(&'a Digest),
    Retrieval(&'a Digest),
}

/// Proof of one exact admitted physical request or bound side operation.
/// Its constructors stay private; a metadata selection cannot resolve a bearer.
#[derive(Debug)]
pub struct OpenRouterCredentialRequest<'a> {
    proof: OpenRouterCredentialProof<'a>,
}

impl<'a> OpenRouterCredentialRequest<'a> {
    fn responses(binding: &'a ResponsesPhysicalRequestBinding) -> Self {
        Self {
            proof: OpenRouterCredentialProof::Responses(binding),
        }
    }
    fn chat(binding: &'a ChatPhysicalRequestBinding) -> Self {
        Self {
            proof: OpenRouterCredentialProof::Chat(binding),
        }
    }
    fn messages(binding: &'a MessagesPhysicalRequestBinding) -> Self {
        Self {
            proof: OpenRouterCredentialProof::Messages(binding),
        }
    }
    fn catalog(operation: &'a Digest) -> Self {
        Self {
            proof: OpenRouterCredentialProof::Catalog(operation),
        }
    }
    fn retrieval(operation: &'a Digest) -> Self {
        Self {
            proof: OpenRouterCredentialProof::Retrieval(operation),
        }
    }

    /// Purpose of the exact admitted request.
    #[must_use]
    pub const fn purpose(&self) -> OpenRouterCredentialPurpose {
        match self.proof {
            OpenRouterCredentialProof::Responses(_) => OpenRouterCredentialPurpose::Responses,
            OpenRouterCredentialProof::Chat(_) => OpenRouterCredentialPurpose::Chat,
            OpenRouterCredentialProof::Messages(_) => OpenRouterCredentialPurpose::Messages,
            OpenRouterCredentialProof::Catalog(_) => OpenRouterCredentialPurpose::Catalog,
            OpenRouterCredentialProof::Retrieval(_) => OpenRouterCredentialPurpose::Retrieval,
        }
    }
    /// Exact Responses physical binding, only for that wire.
    #[must_use]
    pub const fn responses_binding(&self) -> Option<&ResponsesPhysicalRequestBinding> {
        match self.proof {
            OpenRouterCredentialProof::Responses(binding) => Some(binding),
            _ => None,
        }
    }
    /// Exact Chat physical binding, only for that wire.
    #[must_use]
    pub const fn chat_binding(&self) -> Option<&ChatPhysicalRequestBinding> {
        match self.proof {
            OpenRouterCredentialProof::Chat(binding) => Some(binding),
            _ => None,
        }
    }
    /// Exact Messages physical binding, only for that wire.
    #[must_use]
    pub const fn messages_binding(&self) -> Option<&MessagesPhysicalRequestBinding> {
        match self.proof {
            OpenRouterCredentialProof::Messages(binding) => Some(binding),
            _ => None,
        }
    }
    /// Exact bound side-operation digest, only for catalog and retrieval requests.
    #[must_use]
    pub const fn operation(&self) -> Option<Digest> {
        match self.proof {
            OpenRouterCredentialProof::Catalog(operation)
            | OpenRouterCredentialProof::Retrieval(operation) => Some(*operation),
            _ => None,
        }
    }
}

impl plakous_auth::api_key::ApiKeyRequest for OpenRouterCredentialRequest<'_> {
    fn provider(&self) -> plakous_auth::api_key::CredentialProviderTag {
        plakous_auth::api_key::CredentialProviderTag::OpenRouterPlatform
    }
    fn origin(&self) -> &str {
        OPENROUTER_PLATFORM_ORIGIN
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
    pub(crate) clock: Arc<dyn plakous_model::Clock>,
    identity: Box<OpenRouterChatRouteIdentity>,
    key_source: Arc<OpenRouterKeySource>,
}

impl OpenRouterChatRoute {
    /// Creates the production OpenRouter Chat route without an endpoint pin.
    pub fn new(key_source: Arc<OpenRouterKeySource>) -> Result<Self, OpenRouterRouteError> {
        Self::construct(
            key_source,
            OpenRouterOrigin::new(OPENROUTER_PLATFORM_ORIGIN)?,
            OpenRouterAccessMode::PlatformApiKey,
            None,
        )
    }

    /// Creates a production Chat route with explicit documented retention restrictions.
    pub fn new_with_retention(
        key_source: Arc<OpenRouterKeySource>,
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
        key_source: Arc<OpenRouterKeySource>,
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
        key_source: Arc<OpenRouterKeySource>,
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
        key_source: Arc<OpenRouterKeySource>,
        origin: OpenRouterOrigin,
        access_mode: OpenRouterAccessMode,
        retention_policy: Option<OpenRouterRetentionPolicy>,
    ) -> Result<Self, OpenRouterRouteError> {
        let credential_binding = key_source.authority();
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
            clock: Arc::new(plakous_model::SystemClock),
        })
    }

    /// Uses one clock for discovery, admission, and runtime revalidation.
    #[must_use]
    #[cfg(any(test, feature = "test-clock"))]
    #[doc(hidden)]
    pub fn with_clock(mut self, clock: Arc<dyn plakous_model::Clock>) -> Self {
        self.clock = clock;
        self
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
    ) -> Result<OpenRouterKeyGeneration, OpenRouterAuthError> {
        if self.key_source.authority() != self.identity.credential_binding() {
            return Err(OpenRouterAuthError::BindingMismatch);
        }
        credential_generation(
            self.key_source.as_ref(),
            &OpenRouterApiKeySelection::platform(),
        )
    }

    pub(crate) fn catalog_credential(
        &self,
        operation: &Digest,
        expected: &OpenRouterKeyGeneration,
    ) -> Result<ApiKey, OpenRouterAuthError> {
        if self.key_source.authority() != self.identity.credential_binding() {
            return Err(OpenRouterAuthError::BindingMismatch);
        }
        let request = OpenRouterCredentialRequest::catalog(operation);
        let current = credential_generation(self.key_source.as_ref(), &request)?;
        if &current != expected {
            return Err(OpenRouterAuthError::BindingMismatch);
        }
        resolve_key(self.key_source.as_ref(), &request, expected)
    }

    pub(crate) fn retrieval_credential(
        &self,
        operation: &Digest,
        expected: &OpenRouterKeyGeneration,
    ) -> Result<ApiKey, OpenRouterAuthError> {
        if self.key_source.authority() != self.identity.credential_binding() {
            return Err(OpenRouterAuthError::BindingMismatch);
        }
        let request = OpenRouterCredentialRequest::retrieval(operation);
        let current = credential_generation(self.key_source.as_ref(), &request)?;
        if &current != expected {
            return Err(OpenRouterAuthError::BindingMismatch);
        }
        resolve_key(self.key_source.as_ref(), &request, expected)
    }

    pub(crate) fn chat_credential(
        &self,
        physical: &ChatPhysicalRequestBinding,
        expected: &OpenRouterKeyGeneration,
    ) -> Result<ApiKey, OpenRouterAuthError> {
        if self.key_source.authority() != self.identity.credential_binding() {
            return Err(OpenRouterAuthError::BindingMismatch);
        }
        let request = OpenRouterCredentialRequest::chat(physical);
        let current = credential_generation(self.key_source.as_ref(), &request)?;
        if &current != expected {
            return Err(OpenRouterAuthError::BindingMismatch);
        }
        resolve_key(self.key_source.as_ref(), &request, expected)
    }

    pub(crate) fn messages_credential(
        &self,
        physical: &MessagesPhysicalRequestBinding,
        expected: &OpenRouterKeyGeneration,
    ) -> Result<ApiKey, OpenRouterAuthError> {
        if self.key_source.authority() != self.identity.credential_binding() {
            return Err(OpenRouterAuthError::BindingMismatch);
        }
        let request = OpenRouterCredentialRequest::messages(physical);
        let current = credential_generation(self.key_source.as_ref(), &request)?;
        if &current != expected {
            return Err(OpenRouterAuthError::BindingMismatch);
        }
        resolve_key(self.key_source.as_ref(), &request, expected)
    }

    pub(crate) fn responses_credential_generation_grant(
        &self,
        physical: &ResponsesPhysicalRequestBinding,
    ) -> Result<OpenRouterKeyGeneration, OpenRouterAuthError> {
        if self.key_source.authority() != self.identity.credential_binding() {
            return Err(OpenRouterAuthError::BindingMismatch);
        }
        credential_generation(
            self.key_source.as_ref(),
            &OpenRouterCredentialRequest::responses(physical),
        )
    }

    /// Resolves the Responses Bearer key at the admitted physical-request fence.
    pub(crate) fn responses_credential(
        &self,
        physical: &ResponsesPhysicalRequestBinding,
        expected: &OpenRouterKeyGeneration,
    ) -> Result<ApiKey, OpenRouterAuthError> {
        if self.key_source.authority() != self.identity.credential_binding() {
            return Err(OpenRouterAuthError::BindingMismatch);
        }
        let request = OpenRouterCredentialRequest::responses(physical);
        let current = credential_generation(self.key_source.as_ref(), &request)?;
        if &current != expected {
            return Err(OpenRouterAuthError::BindingMismatch);
        }
        resolve_key(self.key_source.as_ref(), &request, expected)
    }

    pub(crate) fn revalidate_responses_credential_generation(
        &self,
        physical: &ResponsesPhysicalRequestBinding,
        expected: &OpenRouterKeyGeneration,
    ) -> Result<(), OpenRouterAuthError> {
        if &self.responses_credential_generation_grant(physical)? != expected {
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
            .field("clock", &"<injected clock>")
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

impl From<plakous_auth::api_key::ApiKeyError> for OpenRouterAuthError {
    fn from(error: plakous_auth::api_key::ApiKeyError) -> Self {
        match error {
            plakous_auth::api_key::ApiKeyError::BindingMismatch => Self::BindingMismatch,
            _ => Self::CredentialSource,
        }
    }
}
