//! Explicit Anthropic Platform route, model, profile, and secret configuration.

use std::fmt;
use std::sync::Arc;

pub use plakous_auth::api_key::ApiKey;
use plakous_auth::api_key::ExposeApiKey as _;
use plakous_auth::api_key::{
    ApiKeyAuthorityToken, ApiKeyError, ApiKeyRequest, ApiKeySource, CredentialProviderTag,
    provider_key_binding,
};
use plakous_auth::custody::{CredentialField, KeyHeaderForm};
use plakous_messages::{MessagesAttempt, MessagesAttemptBinding};
use plakous_model::{
    Clock, Digest, DigestDomain, ModelAdapterProfileId, ModelFamilyId, ModelId, ProtocolId,
    ResolvedModelDescriptor, RouteId, SessionLineage, SystemClock, ValidatedRequestPlan,
};

use crate::{
    AnthropicError, AnthropicErrorKind, AnthropicErrorPhase, AnthropicMessagesDialect,
    FeatureMatrixEntry, dialect::MESSAGES_FEATURE_MATRIX_V1,
};

/// The only HTTPS origin authorized by [`AnthropicPlatformRoute`].
pub const ANTHROPIC_PLATFORM_ORIGIN: &str = "https://api.anthropic.com";
/// The only request path authorized by the stable Messages transport.
pub const ANTHROPIC_MESSAGES_PATH: &str = "/v1/messages";

const MAX_ACCOUNT_IDENTITY_BYTES: usize = 256;

/// Opaque non-secret account identity used to isolate prepared operations.
///
/// The caller supplies a stable, non-secret account or project label. Only a
/// domain-separated digest is retained, and formatting never exposes even
/// that digest. This type deliberately does not implement serialization.
#[derive(Clone)]
pub struct AnthropicAccountIdentity(AccountIdentityInner);

#[derive(Clone)]
enum AccountIdentityInner {
    Label(Digest),
    KeyAuthority(ApiKeyAuthorityToken),
}

impl AnthropicAccountIdentity {
    /// Derives an opaque account identity from a stable non-secret label.
    ///
    /// # Errors
    ///
    /// Returns an invalid-configuration error when the label is empty,
    /// contains control bytes, exceeds 256 bytes, or cannot be digested.
    pub fn new(label: impl AsRef<str>) -> Result<Self, AnthropicError> {
        let label = label.as_ref();
        if label.is_empty()
            || label.len() > MAX_ACCOUNT_IDENTITY_BYTES
            || label.bytes().any(|byte| byte.is_ascii_control())
        {
            return Err(AnthropicError::new(
                AnthropicErrorKind::InvalidConfiguration,
                AnthropicErrorPhase::Prepare,
                "Anthropic account identity must be a bounded non-empty non-secret label without control bytes",
            ));
        }
        digest_identity(b"anthropic.platform.account.v1", label.as_bytes())
            .map(AccountIdentityInner::Label)
            .map(Self)
    }

    fn from_key(key: &ApiKey) -> Self {
        Self(AccountIdentityInner::KeyAuthority(key.authority().clone()))
    }

    /// Returns the stable non-secret digest for a label-derived account identity.
    ///
    /// Key-authority identities intentionally return `None`; paid campaigns
    /// must use an explicit stable binding, such as a digest-derived key label,
    /// so approval identity survives only the intended credential authority.
    #[must_use]
    pub const fn stable_binding(&self) -> Option<Digest> {
        match &self.0 {
            AccountIdentityInner::Label(digest) => Some(*digest),
            AccountIdentityInner::KeyAuthority(_) => None,
        }
    }
}

impl PartialEq for AnthropicAccountIdentity {
    fn eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (AccountIdentityInner::Label(left), AccountIdentityInner::Label(right)) => {
                left == right
            }
            (
                AccountIdentityInner::KeyAuthority(left),
                AccountIdentityInner::KeyAuthority(right),
            ) => left == right,
            (AccountIdentityInner::Label(_), AccountIdentityInner::KeyAuthority(_))
            | (AccountIdentityInner::KeyAuthority(_), AccountIdentityInner::Label(_)) => false,
        }
    }
}

impl Eq for AnthropicAccountIdentity {}

impl fmt::Debug for AnthropicAccountIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AnthropicAccountIdentity(<redacted>)")
    }
}

/// Opaque identity of the immutable API-key authority for a route.
///
/// The identity is a non-linkable clone token created with the key. Clones of
/// one key retain it, while independently wrapping the same text creates a new
/// authority. Neither the key nor the token is serializable or formatted.
#[derive(Clone)]
pub struct AnthropicCredentialAuthority(ApiKeyAuthorityToken);

impl AnthropicCredentialAuthority {
    fn from_key(key: &ApiKey) -> Self {
        Self(key.authority().clone())
    }

    fn fresh() -> Self {
        Self(ApiKeyAuthorityToken::fresh())
    }
}

impl PartialEq for AnthropicCredentialAuthority {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for AnthropicCredentialAuthority {}

impl fmt::Debug for AnthropicCredentialAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AnthropicCredentialAuthority(<redacted>)")
    }
}

fn digest_identity(domain: &[u8], value: &[u8]) -> Result<Digest, AnthropicError> {
    Digest::sha256(DigestDomain::ModelBinding, 2, &[domain, value]).map_err(|_| {
        AnthropicError::new(
            AnthropicErrorKind::InvalidConfiguration,
            AnthropicErrorPhase::Prepare,
            "failed to derive an Anthropic credential binding",
        )
    })
}

/// Fixed transport selected by the offline Messages protocol boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum AnthropicMessagesTransport {
    /// HTTPS request with an incrementally decoded server-sent event body.
    HttpsSse,
}

impl AnthropicMessagesTransport {
    /// Returns the fixed HTTP method.
    #[must_use]
    pub const fn method(self) -> &'static str {
        match self {
            Self::HttpsSse => "POST",
        }
    }

    /// Returns the fixed endpoint path.
    #[must_use]
    pub const fn path(self) -> &'static str {
        match self {
            Self::HttpsSse => ANTHROPIC_MESSAGES_PATH,
        }
    }
}

/// Exact Anthropic Platform audience supplied only by the selected integration.
pub struct AnthropicApiKeyRequest(());

impl AnthropicApiKeyRequest {
    /// Names the official Platform credential audience.
    #[must_use]
    pub(crate) const fn platform() -> Self {
        Self(())
    }
}

impl ApiKeyRequest for AnthropicApiKeyRequest {
    fn provider(&self) -> CredentialProviderTag {
        CredentialProviderTag::AnthropicPlatform
    }

    fn origin(&self) -> &str {
        ANTHROPIC_PLATFORM_ORIGIN
    }
}

/// Caller-owned API-key source for the exact Anthropic request context.
pub type AnthropicKeySource = dyn ApiKeySource<AnthropicApiKeyRequest>;

/// Source-owned Anthropic Platform generation authority.
pub(crate) type AnthropicKeyGeneration = plakous_auth::api_key::ApiKeyGenerationGrant;

/// The source's current Platform generation, without reading a key.
pub(crate) fn platform_grant(
    source: &AnthropicKeySource,
) -> Result<AnthropicKeyGeneration, AnthropicError> {
    let grant = source
        .generation_grant(&AnthropicApiKeyRequest::platform())
        .map_err(api_key_error)?;
    if grant.provider() != CredentialProviderTag::AnthropicPlatform {
        return Err(api_key_error(ApiKeyError::BindingMismatch));
    }
    Ok(grant)
}

/// The raw `x-api-key` field of `grant`, from the key's custodian.
pub(crate) fn platform_key_field(
    source: &AnthropicKeySource,
    grant: &AnthropicKeyGeneration,
) -> Result<CredentialField, AnthropicError> {
    source
        .credential_field(
            &AnthropicApiKeyRequest::platform(),
            grant,
            KeyHeaderForm::Raw,
        )
        .map_err(api_key_error)
}

fn current_api_key(source: &AnthropicKeySource) -> Result<ApiKey, AnthropicError> {
    let grant = platform_grant(source)?;
    source
        .api_key(&AnthropicApiKeyRequest::platform(), &grant)
        .map_err(api_key_error)
}

pub(crate) fn api_key_error(error: ApiKeyError) -> AnthropicError {
    let kind = match error {
        ApiKeyError::InvalidKey | ApiKeyError::InvalidGenerationBinding => {
            AnthropicErrorKind::InvalidConfiguration
        }
        ApiKeyError::BindingMismatch | ApiKeyError::Unavailable => {
            AnthropicErrorKind::Authentication
        }
    };
    AnthropicError::new(kind, AnthropicErrorPhase::Prepare, error.to_string())
}

/// What a route's credential is.
#[derive(Clone)]
enum RouteCredential {
    /// A key or OAuth token this process holds.
    Key(Arc<ApiKey>),
    /// A key source's admitted Platform generation; the source's custodian
    /// supplies the field and the key binding, so a key a credential
    /// gateway holds binds and dispatches exactly as a local one.
    Source(Arc<SourceCredential>),
}

struct SourceCredential {
    source: Arc<AnthropicKeySource>,
    grant: AnthropicKeyGeneration,
    binding: Digest,
}

/// Resolved API-key authentication bound to the official Platform origin.
///
/// Clone shares the non-`Clone` key via [`Arc`] without duplicating secret bytes.
#[derive(Clone)]
pub struct AnthropicApiKeyAuth {
    credential: RouteCredential,
    audience_origin: &'static str,
    credential_authority: AnthropicCredentialAuthority,
    /// A credential custodian's exact `authorization` value, which replaces
    /// the key's on a Claude OAuth route whose login a custodian holds.
    custodied: Option<Arc<secrecy::SecretSlice<u8>>>,
}

impl AnthropicApiKeyAuth {
    /// Binds a key to the official Anthropic Platform origin.
    #[must_use]
    pub fn platform(key: ApiKey) -> Self {
        let credential_authority = AnthropicCredentialAuthority::from_key(&key);
        Self {
            credential: RouteCredential::Key(Arc::new(key)),
            audience_origin: ANTHROPIC_PLATFORM_ORIGIN,
            credential_authority,
            custodied: None,
        }
    }

    fn from_source(credential: SourceCredential) -> Self {
        Self {
            credential: RouteCredential::Source(Arc::new(credential)),
            audience_origin: ANTHROPIC_PLATFORM_ORIGIN,
            credential_authority: AnthropicCredentialAuthority::fresh(),
            custodied: None,
        }
    }

    /// Returns the credential audience without exposing the credential.
    #[must_use]
    pub const fn audience_origin(&self) -> &'static str {
        self.audience_origin
    }

    /// Returns the opaque identity of the configured credential authority.
    #[must_use]
    pub const fn credential_authority(&self) -> &AnthropicCredentialAuthority {
        &self.credential_authority
    }
}

impl fmt::Debug for AnthropicApiKeyAuth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AnthropicApiKeyAuth")
            .field("audience_origin", &self.audience_origin)
            .field("credential_authority", &self.credential_authority)
            .field("credential", &"<redacted>")
            .field("custodied", &self.custodied.is_some())
            .finish()
    }
}

/// Opaque authorization bound to one admitted physical Messages attempt.
///
/// This value exposes only non-secret binding metadata. Raw-key access is
/// reserved to the crate-owned transport and revalidates the exact attempt and
/// route again immediately before use.
#[derive(Clone)]
pub struct AnthropicAttemptAuthorization {
    auth: AnthropicApiKeyAuth,
    binding: MessagesAttemptBinding,
}

impl AnthropicAttemptAuthorization {
    fn new(auth: AnthropicApiKeyAuth, binding: MessagesAttemptBinding) -> Self {
        Self { auth, binding }
    }

    /// Returns the exact prepared operation and physical attempt identity.
    #[must_use]
    pub const fn attempt_binding(&self) -> &MessagesAttemptBinding {
        &self.binding
    }

    /// Revalidates this authorization against an admitted exact attempt.
    ///
    /// # Errors
    ///
    /// Returns a binding error for a prepared-only or foreign attempt.
    pub fn validate_for_attempt(&self, attempt: &MessagesAttempt) -> Result<(), AnthropicError> {
        attempt
            .ensure_service_admitted()
            .map_err(|_| credential_binding_error())?;
        if self.binding != *attempt.binding() {
            return Err(credential_binding_error());
        }
        Ok(())
    }

    /// The credential field for an admitted exact attempt: `Bearer` for
    /// Claude OAuth, the raw key for the Platform API.
    pub(crate) fn credential_for(
        &self,
        attempt: &MessagesAttempt,
        target: &AnthropicMessagesTargetBinding,
        route: &AnthropicPlatformRoute,
    ) -> Result<CredentialField, AnthropicError> {
        self.validate_for_attempt(attempt)?;
        target.validate_route(route)?;
        if self.auth.audience_origin != route.auth.audience_origin
            || self.auth.credential_authority != route.auth.credential_authority
        {
            return Err(credential_binding_error());
        }
        if let Some(field) = &self.auth.custodied {
            use secrecy::ExposeSecret as _;
            return Ok(CredentialField::exact(field.expose_secret().to_vec()));
        }
        match &self.auth.credential {
            RouteCredential::Key(key) => {
                let form = if route.is_claude_oauth() {
                    KeyHeaderForm::Bearer
                } else {
                    KeyHeaderForm::Raw
                };
                Ok(form.field(key))
            }
            RouteCredential::Source(held) if !route.is_claude_oauth() => {
                platform_key_field(held.source.as_ref(), &held.grant)
            }
            RouteCredential::Source(_) => Err(credential_binding_error()),
        }
    }
}

impl fmt::Debug for AnthropicAttemptAuthorization {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AnthropicAttemptAuthorization")
            .field("auth", &self.auth)
            .field("binding", &self.binding)
            .finish()
    }
}

fn credential_binding_error() -> AnthropicError {
    AnthropicError::new(
        AnthropicErrorKind::BindingMismatch,
        AnthropicErrorPhase::Prepare,
        "Anthropic credential does not match the admitted exact attempt binding",
    )
}

/// Immutable official Anthropic Platform Messages route.
#[derive(Clone)]
pub struct AnthropicPlatformRoute {
    id: RouteId,
    origin: &'static str,
    account: AnthropicAccountIdentity,
    transport: AnthropicMessagesTransport,
    auth: AnthropicApiKeyAuth,
}

impl AnthropicPlatformRoute {
    fn replay_account_binding(&self) -> Result<Option<Digest>, AnthropicError> {
        let Some(account) = self.account.stable_binding() else {
            return Ok(None);
        };
        if self.is_claude_oauth() {
            return Ok(Some(account));
        }
        let mut scope = account.identity_bytes();
        scope.extend_from_slice(&self.credential_binding()?.identity_bytes());
        digest_identity(b"anthropic.platform.replay-account.v1", &scope).map(Some)
    }

    /// The digest of the route's key bytes a session binds to
    /// ([`plakous_auth::api_key::provider_key_binding`]): computed over a
    /// held key, or the one a key source's custodian reported.
    pub(crate) fn credential_binding(&self) -> Result<Digest, AnthropicError> {
        match &self.auth.credential {
            RouteCredential::Key(key) => {
                provider_key_binding(CredentialProviderTag::AnthropicPlatform, key).ok_or_else(
                    || {
                        AnthropicError::new(
                            AnthropicErrorKind::InvalidConfiguration,
                            AnthropicErrorPhase::Prepare,
                            "failed to derive an Anthropic credential binding",
                        )
                    },
                )
            }
            RouteCredential::Source(held) => Ok(held.binding),
        }
    }

    /// Refuses unless `source` still holds the route's exact key: the same
    /// text for a held key, the same admitted generation for a source's.
    pub(crate) fn validate_current_key(
        &self,
        source: &AnthropicKeySource,
    ) -> Result<(), AnthropicError> {
        if self.is_claude_oauth() {
            return Err(credential_binding_error());
        }
        let current = match &self.auth.credential {
            RouteCredential::Key(key) => {
                current_api_key(source)?.expose_secret() == key.expose_secret()
            }
            RouteCredential::Source(held) => platform_grant(source)? == held.grant,
        };
        if !current {
            return Err(credential_binding_error());
        }
        Ok(())
    }

    /// The route's raw key field after checking it is still the source's
    /// current key: the one credential path of an isolated side request.
    pub(crate) fn current_key_credential(
        &self,
        source: &AnthropicKeySource,
    ) -> Result<CredentialField, AnthropicError> {
        self.validate_current_key(source)?;
        match &self.auth.credential {
            RouteCredential::Key(key) => Ok(KeyHeaderForm::Raw.field(key)),
            RouteCredential::Source(held) => platform_key_field(source, &held.grant),
        }
    }

    /// The official API-key route over `source`'s current generation: its
    /// credential binding is the source's key binding and every request's
    /// `x-api-key` the source's raw field, so a key a credential gateway
    /// holds binds and dispatches byte for byte as the same key held here.
    /// Await [`ApiKeySource::prepare`] first.
    pub(crate) fn for_source(
        source: Arc<AnthropicKeySource>,
        account: AnthropicAccountIdentity,
    ) -> Result<Self, AnthropicError> {
        let grant = platform_grant(source.as_ref())?;
        let binding = source
            .key_binding(&AnthropicApiKeyRequest::platform(), &grant)
            .map_err(api_key_error)?;
        Ok(Self {
            id: RouteId::new("anthropic.platform").map_err(|error| {
                AnthropicError::new(
                    AnthropicErrorKind::InvalidConfiguration,
                    AnthropicErrorPhase::Prepare,
                    error.to_string(),
                )
            })?,
            origin: ANTHROPIC_PLATFORM_ORIGIN,
            account,
            transport: AnthropicMessagesTransport::HttpsSse,
            auth: AnthropicApiKeyAuth::from_source(SourceCredential {
                source,
                grant,
                binding,
            }),
        })
    }

    /// Creates the official API-key route. Custom origins require a distinct
    /// route type and explicit credential binding; this type cannot be mutated.
    /// Cloning one [`ApiKey`] preserves its authority identity;
    /// independently wrapping even identical key text creates a new authority
    /// and therefore requires newly prepared requests. This constructor uses a
    /// conservative key-scoped account identity; use [`Self::for_account`] when
    /// a stable non-secret account/project label is available.
    ///
    /// # Errors
    ///
    /// Returns an invalid-configuration error if the stable route identity
    /// cannot be constructed.
    pub fn new(key: ApiKey) -> Result<Self, AnthropicError> {
        let account = AnthropicAccountIdentity::from_key(&key);
        Self::for_account(key, account)
    }

    /// Creates the official route with an explicit stable account identity.
    ///
    /// `account` must come from a non-secret account/project label. This is the
    /// constructor to use when multiple API keys are known to share one account.
    /// The credential authority remains key-derived, so changing keys still
    /// requires a newly prepared operation even when the account is unchanged.
    /// [`Self::new`] instead derives a conservative key-scoped account identity.
    ///
    /// # Errors
    ///
    /// Returns an invalid-configuration error if a stable route or credential
    /// authority identity cannot be constructed.
    pub fn for_account(
        key: ApiKey,
        account: AnthropicAccountIdentity,
    ) -> Result<Self, AnthropicError> {
        Ok(Self {
            id: RouteId::new("anthropic.platform").map_err(|error| {
                AnthropicError::new(
                    AnthropicErrorKind::InvalidConfiguration,
                    AnthropicErrorPhase::Prepare,
                    error.to_string(),
                )
            })?,
            origin: ANTHROPIC_PLATFORM_ORIGIN,
            account,
            transport: AnthropicMessagesTransport::HttpsSse,
            auth: AnthropicApiKeyAuth::platform(key),
        })
    }

    /// Creates the Claude Code OAuth Messages route (Bearer, isolated from Console keys).
    ///
    /// `token` is the OAuth access token stored in the same secret container as
    /// an API key; the HTTP layer emits `Authorization: Bearer` for this route.
    ///
    /// # Errors
    ///
    /// Returns an invalid-configuration error if the stable route identity
    /// cannot be constructed.
    pub fn claude_oauth(
        token: ApiKey,
        account: AnthropicAccountIdentity,
    ) -> Result<Self, AnthropicError> {
        Ok(Self {
            id: RouteId::new(crate::CLAUDE_OAUTH_ROUTE_ID).map_err(|error| {
                AnthropicError::new(
                    AnthropicErrorKind::InvalidConfiguration,
                    AnthropicErrorPhase::Prepare,
                    error.to_string(),
                )
            })?,
            origin: crate::oauth::CLAUDE_OAUTH_MESSAGES_ORIGIN,
            account,
            transport: AnthropicMessagesTransport::HttpsSse,
            auth: AnthropicApiKeyAuth::platform(token),
        })
    }

    /// Returns whether this route is Claude Code OAuth rather than Console API-key.
    #[must_use]
    pub fn is_claude_oauth(&self) -> bool {
        self.id.as_str() == crate::CLAUDE_OAUTH_ROUTE_ID
    }

    /// Replaces the OAuth credential (the stored access token, or a
    /// custodian's exact `authorization` value) without rotating the
    /// prepared credential authority.
    ///
    /// # Errors
    ///
    /// Returns an invalid-configuration error when this is not the Claude OAuth
    /// route or the token is empty/control-bearing.
    #[cfg(feature = "native-auth")]
    pub(crate) fn with_oauth_access(
        &self,
        access: crate::native_auth::ClaudeOAuthAccess,
    ) -> Result<Self, AnthropicError> {
        if !self.is_claude_oauth() {
            return Err(AnthropicError::new(
                AnthropicErrorKind::InvalidConfiguration,
                AnthropicErrorPhase::Prepare,
                "OAuth access-token rotation requires the Claude OAuth route",
            ));
        }
        // A custodied route keeps a fixed key text only for its credential
        // authority; the custodian's field is what the request sends.
        let (token, custodied) = match access {
            crate::native_auth::ClaudeOAuthAccess::Token(token) => (token, None),
            crate::native_auth::ClaudeOAuthAccess::Custodied(field) => (
                "plak-custodied-claude-login".to_owned(),
                Some(Arc::new(secrecy::SecretSlice::from(field.to_vec()))),
            ),
        };
        let RouteCredential::Key(held) = &self.auth.credential else {
            return Err(credential_binding_error());
        };
        let key = held.renew_with(token).map_err(api_key_error)?;
        let credential_authority = AnthropicCredentialAuthority::from_key(&key);
        Ok(Self {
            id: self.id.clone(),
            origin: self.origin,
            account: self.account.clone(),
            transport: self.transport,
            auth: AnthropicApiKeyAuth {
                credential: RouteCredential::Key(Arc::new(key)),
                audience_origin: self.auth.audience_origin,
                credential_authority,
                custodied,
            },
        })
    }

    /// Returns the stable route identity.
    #[must_use]
    pub const fn id(&self) -> &RouteId {
        &self.id
    }

    /// Returns the immutable authenticated origin.
    #[must_use]
    pub const fn origin(&self) -> &'static str {
        self.origin
    }

    /// Returns the opaque account binding for this route.
    #[must_use]
    pub const fn account(&self) -> &AnthropicAccountIdentity {
        &self.account
    }

    /// Returns the fixed transport binding for this route.
    #[must_use]
    pub const fn transport(&self) -> AnthropicMessagesTransport {
        self.transport
    }

    /// Returns redacted authentication configuration.
    #[must_use]
    pub const fn auth(&self) -> &AnthropicApiKeyAuth {
        &self.auth
    }

    /// Resolves API-key authorization only after exact target validation and
    /// private Messages service admission.
    pub fn authorization(
        &self,
        attempt: &MessagesAttempt,
        target: &AnthropicMessagesTargetBinding,
    ) -> Result<AnthropicAttemptAuthorization, AnthropicError> {
        attempt
            .ensure_service_admitted()
            .map_err(|_| credential_binding_error())?;
        target.validate_route(self)?;
        Ok(AnthropicAttemptAuthorization::new(
            self.auth.clone(),
            attempt.binding().clone(),
        ))
    }
}

impl fmt::Debug for AnthropicPlatformRoute {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AnthropicPlatformRoute")
            .field("id", &self.id)
            .field("origin", &self.origin)
            .field("account", &self.account)
            .field("transport", &self.transport)
            .field("auth", &self.auth)
            .finish()
    }
}

/// Stable thinking support declared for one exact model configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ThinkingSupport {
    /// Thinking controls and signed replay are unavailable.
    Disabled,
    /// Adaptive thinking with an exact provider effort is supported.
    Adaptive,
    /// Manual token-budget thinking is supported.
    Manual {
        /// Provider minimum thinking budget.
        minimum_budget_tokens: u32,
    },
}

/// Explicit remote model identity and adapter-relevant constraints.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnthropicModel {
    remote_id: ModelId,
    family: ModelFamilyId,
    maximum_output_tokens: u32,
    thinking: ThinkingSupport,
}

impl AnthropicModel {
    /// Creates a model declaration from explicit catalog or application facts.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero output limit.
    pub fn new(
        remote_id: ModelId,
        family: ModelFamilyId,
        maximum_output_tokens: u32,
        thinking: ThinkingSupport,
    ) -> Result<Self, AnthropicError> {
        if maximum_output_tokens == 0 {
            return Err(AnthropicError::new(
                AnthropicErrorKind::InvalidConfiguration,
                AnthropicErrorPhase::Prepare,
                "Anthropic model maximum output tokens must be nonzero",
            ));
        }
        Ok(Self {
            remote_id,
            family,
            maximum_output_tokens,
            thinking,
        })
    }

    /// Returns the exact remote model ID used on the wire and for replay scope.
    #[must_use]
    pub const fn remote_id(&self) -> &ModelId {
        &self.remote_id
    }

    /// Returns the declared model family.
    #[must_use]
    pub const fn family(&self) -> &ModelFamilyId {
        &self.family
    }

    /// Returns the configured output-token ceiling.
    #[must_use]
    pub const fn maximum_output_tokens(&self) -> u32 {
        self.maximum_output_tokens
    }

    /// Returns the exact thinking capability declaration.
    #[must_use]
    pub const fn thinking(&self) -> ThinkingSupport {
        self.thinking
    }
}

/// Frozen Claude harness-profile identity used by request bindings.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaudeProfile {
    id: ModelAdapterProfileId,
}

impl ClaudeProfile {
    /// Creates a named, immutable profile identity.
    #[must_use]
    pub const fn new(id: ModelAdapterProfileId) -> Self {
        Self { id }
    }

    /// Returns the profile identity.
    #[must_use]
    pub const fn id(&self) -> &ModelAdapterProfileId {
        &self.id
    }
}

/// Explicit protocol + route + model + profile binding used for translation.
#[derive(Clone)]
pub struct ResolvedAnthropicMessages {
    dialect: AnthropicMessagesDialect,
    route: AnthropicPlatformRoute,
    model: AnthropicModel,
    profile: ClaudeProfile,
    protocol_id: ProtocolId,
    descriptor: ResolvedModelDescriptor,
    resolution_trust: AnthropicResolutionTrust,
    reviewed_admission_grant: Option<AnthropicReviewedAdmissionGrant>,
    clock: Arc<dyn Clock>,
    lineage: SessionLineage,
}

/// Clone-preserving, non-forgeable proof that the crate reviewed this target.
#[derive(Clone)]
struct AnthropicReviewedAdmissionGrant(Arc<()>);

impl AnthropicReviewedAdmissionGrant {
    fn new() -> Self {
        Self(Arc::new(()))
    }
}

impl PartialEq for AnthropicReviewedAdmissionGrant {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for AnthropicReviewedAdmissionGrant {}

impl fmt::Debug for AnthropicReviewedAdmissionGrant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AnthropicReviewedAdmissionGrant(<redacted>)")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AnthropicResolutionTrust {
    CallerAuthored,
    /// Documented API admission: it never expires on the calendar.
    ReviewedApiAdmission {
        revision: &'static str,
        reviewed_at: plakous_model::Timestamp,
    },
    /// A subscription compatibility profile, current from its review until
    /// its exclusive expiry.
    #[allow(
        dead_code,
        reason = "constructed only by the native-auth Claude OAuth target"
    )]
    ReviewedSubscriptionAdmission {
        revision: &'static str,
        reviewed_at: plakous_model::Timestamp,
        expires_at: plakous_model::Timestamp,
    },
}

impl ResolvedAnthropicMessages {
    /// Freezes all request-affecting Anthropic identities.
    ///
    /// # Errors
    ///
    /// Returns an invalid-configuration error if the stable protocol identity
    /// cannot be constructed.
    pub fn new(
        dialect: AnthropicMessagesDialect,
        route: AnthropicPlatformRoute,
        model: AnthropicModel,
        profile: ClaudeProfile,
        descriptor: ResolvedModelDescriptor,
    ) -> Result<Self, AnthropicError> {
        let protocol_id = ProtocolId::new("anthropic.messages").map_err(|error| {
            AnthropicError::new(
                AnthropicErrorKind::InvalidConfiguration,
                AnthropicErrorPhase::Prepare,
                error.to_string(),
            )
        })?;
        if descriptor.protocol() != &protocol_id
            || descriptor.route() != route.id()
            || descriptor.requested_model() != model.remote_id()
            || descriptor.model_family() != Some(model.family())
            || descriptor.profile() != profile.id()
        {
            return Err(AnthropicError::new(
                AnthropicErrorKind::InvalidConfiguration,
                AnthropicErrorPhase::Prepare,
                "resolved Anthropic protocol/route/model/family/profile does not match its descriptor",
            ));
        }
        Ok(Self {
            dialect,
            route,
            model,
            profile,
            protocol_id,
            descriptor,
            resolution_trust: AnthropicResolutionTrust::CallerAuthored,
            reviewed_admission_grant: None,
            clock: Arc::new(SystemClock),
            lineage: SessionLineage::Root,
        })
    }

    /// Records where the consumer placed this session in its agent topology.
    ///
    /// Only the Claude OAuth dialect reads this (its billing block carries
    /// `cc_is_subagent=true` for spawned children); it never enters a
    /// request or configuration identity.
    #[must_use]
    pub fn with_session_lineage(mut self, lineage: SessionLineage) -> Self {
        self.lineage = lineage;
        self
    }

    /// Returns the consumer-declared session lineage.
    #[must_use]
    pub const fn session_lineage(&self) -> SessionLineage {
        self.lineage
    }

    /// Binds a trusted target to its reviewed admission (API or
    /// subscription) and the clock every admission check reads.
    pub(crate) fn new_reviewed_admission(
        dialect: AnthropicMessagesDialect,
        route: AnthropicPlatformRoute,
        model: AnthropicModel,
        profile: ClaudeProfile,
        descriptor: ResolvedModelDescriptor,
        trust: AnthropicResolutionTrust,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, AnthropicError> {
        if trust == AnthropicResolutionTrust::CallerAuthored {
            return Err(AnthropicError::new(
                AnthropicErrorKind::InvalidConfiguration,
                AnthropicErrorPhase::Prepare,
                "a reviewed admission requires a reviewed trust",
            ));
        }
        let mut resolved = Self::new(dialect, route, model, profile, descriptor)?;
        resolved.resolution_trust = trust;
        resolved.reviewed_admission_grant = Some(AnthropicReviewedAdmissionGrant::new());
        resolved.clock = clock;
        Ok(resolved)
    }

    /// Returns the protocol identity.
    #[must_use]
    pub const fn protocol_id(&self) -> &ProtocolId {
        &self.protocol_id
    }

    /// Returns the Messages dialect.
    #[must_use]
    pub const fn dialect(&self) -> &AnthropicMessagesDialect {
        &self.dialect
    }

    /// Returns the immutable Platform route.
    #[must_use]
    pub const fn route(&self) -> &AnthropicPlatformRoute {
        &self.route
    }

    /// Returns the exact model declaration.
    #[must_use]
    pub const fn model(&self) -> &AnthropicModel {
        &self.model
    }

    /// Returns the frozen profile identity.
    #[must_use]
    pub const fn profile(&self) -> &ClaudeProfile {
        &self.profile
    }

    /// Returns the exact neutral descriptor whose binding digest accepts plans.
    #[must_use]
    pub const fn descriptor(&self) -> &ResolvedModelDescriptor {
        &self.descriptor
    }

    pub(crate) fn with_context_envelope(mut self, context: plakous_model::ContextContract) -> Self {
        self.descriptor = self.descriptor.with_context_contract(context);
        self
    }

    pub(crate) const fn resolution_trust(&self) -> AnthropicResolutionTrust {
        self.resolution_trust
    }

    pub(crate) fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    pub(crate) fn ensure_current_admission(
        &self,
        now: plakous_model::Timestamp,
    ) -> Result<(), AnthropicError> {
        match self.resolution_trust {
            AnthropicResolutionTrust::ReviewedApiAdmission { .. } => Ok(()),
            AnthropicResolutionTrust::ReviewedSubscriptionAdmission {
                reviewed_at,
                expires_at,
                ..
            } if reviewed_at <= now && now < expires_at => Ok(()),
            AnthropicResolutionTrust::ReviewedSubscriptionAdmission { .. } => {
                Err(AnthropicError::new(
                    AnthropicErrorKind::InvalidConfiguration,
                    AnthropicErrorPhase::Prepare,
                    "Anthropic reviewed admission is expired or not yet valid",
                ))
            }
            AnthropicResolutionTrust::CallerAuthored => Err(AnthropicError::new(
                AnthropicErrorKind::InvalidConfiguration,
                AnthropicErrorPhase::Prepare,
                "caller-authored Anthropic configuration is not a reviewed production admission",
            )),
        }
    }

    /// Validates that a prepared plan belongs to this exact resolved descriptor.
    ///
    /// # Errors
    ///
    /// Returns an invalid-request error when any protocol, route, model,
    /// profile, or capability fact differs.
    pub fn validate_plan(&self, plan: &ValidatedRequestPlan) -> Result<(), AnthropicError> {
        self.descriptor
            .validate_plan_binding(plan)
            .map_err(|error| {
                AnthropicError::new(
                    AnthropicErrorKind::InvalidRequest,
                    AnthropicErrorPhase::Prepare,
                    format!("foreign validated request plan: {error}"),
                )
            })
    }

    /// Returns the exact provider model identity used for replay compatibility.
    ///
    /// A provider-reported identity takes precedence over the requested alias.
    #[must_use]
    pub fn replay_target_model(&self) -> &ModelId {
        self.descriptor
            .reported_model()
            .unwrap_or_else(|| self.descriptor.requested_model())
    }

    /// Returns codec evidence scoped to this exact route, model, and dialect.
    #[must_use]
    pub fn feature_evidence(&self) -> ScopedFeatureEvidence<'_> {
        ScopedFeatureEvidence {
            route: self.route.id(),
            model: self.replay_target_model(),
            dialect: &self.dialect,
            entries: MESSAGES_FEATURE_MATRIX_V1,
        }
    }
}

impl fmt::Debug for ResolvedAnthropicMessages {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedAnthropicMessages")
            .field("protocol_id", &self.protocol_id)
            .field("dialect", &self.dialect)
            .field("route", &self.route)
            .field("model", &self.model)
            .field("profile", &self.profile)
            .field("descriptor", &self.descriptor)
            .field("resolution_trust", &self.resolution_trust)
            .field(
                "reviewed_admission_grant",
                &self.reviewed_admission_grant.as_ref().map(|_| "<redacted>"),
            )
            .field("clock", &"<clock>")
            .field("lineage", &self.lineage)
            .finish()
    }
}

/// Exact immutable, non-secret target identity retained by prepared requests.
///
/// Account and credential authority are opaque in-memory equality tokens. The
/// type deliberately does not implement serialization, and its `Debug` output
/// redacts both identities. Route origin, auth audience, HTTP method, endpoint
/// path, stream transport, dialect, protocol, model configuration, profile,
/// and descriptor binding are all frozen independently.
#[derive(Clone, Eq, PartialEq)]
pub struct AnthropicMessagesTargetBinding {
    route_id: RouteId,
    origin: &'static str,
    auth_audience: &'static str,
    account: AnthropicAccountIdentity,
    replay_account_binding: Option<Digest>,
    credential_authority: AnthropicCredentialAuthority,
    transport: AnthropicMessagesTransport,
    dialect: AnthropicMessagesDialect,
    protocol: ProtocolId,
    model: AnthropicModel,
    expected_reported_model: Option<ModelId>,
    profile: ModelAdapterProfileId,
    descriptor_binding: Digest,
    semantic_descriptor_binding: Digest,
    reviewed_admission_grant: Option<AnthropicReviewedAdmissionGrant>,
    lineage: SessionLineage,
}

impl AnthropicMessagesTargetBinding {
    pub(crate) fn from_resolved(
        resolved: &ResolvedAnthropicMessages,
    ) -> Result<Self, AnthropicError> {
        let descriptor_binding = resolved.descriptor.binding_digest().map_err(|_| {
            AnthropicError::new(
                AnthropicErrorKind::InvalidConfiguration,
                AnthropicErrorPhase::Prepare,
                "failed to bind the resolved Anthropic descriptor",
            )
        })?;
        Ok(Self {
            route_id: resolved.route.id.clone(),
            origin: resolved.route.origin,
            auth_audience: resolved.route.auth.audience_origin,
            account: resolved.route.account.clone(),
            replay_account_binding: resolved.route.replay_account_binding()?,
            credential_authority: resolved.route.auth.credential_authority.clone(),
            transport: resolved.route.transport,
            dialect: resolved.dialect.clone(),
            protocol: resolved.protocol_id.clone(),
            model: resolved.model.clone(),
            expected_reported_model: resolved.descriptor.reported_model().cloned(),
            profile: resolved.profile.id.clone(),
            descriptor_binding,
            semantic_descriptor_binding: resolved.descriptor.semantic_binding_digest().map_err(
                |_| {
                    AnthropicError::new(
                        AnthropicErrorKind::InvalidConfiguration,
                        AnthropicErrorPhase::Prepare,
                        "failed to bind the Anthropic descriptor replay semantics",
                    )
                },
            )?,
            reviewed_admission_grant: resolved.reviewed_admission_grant.clone(),
            lineage: resolved.lineage,
        })
    }

    /// Returns the consumer-declared session lineage.
    #[must_use]
    pub const fn session_lineage(&self) -> SessionLineage {
        self.lineage
    }

    /// Returns the stable route family identity.
    #[must_use]
    pub const fn route_id(&self) -> &RouteId {
        &self.route_id
    }

    /// Returns the exact authenticated origin.
    #[must_use]
    pub const fn origin(&self) -> &'static str {
        self.origin
    }

    /// Returns the exact credential audience.
    #[must_use]
    pub const fn auth_audience(&self) -> &'static str {
        self.auth_audience
    }

    /// Returns the opaque account identity.
    #[must_use]
    pub const fn account(&self) -> &AnthropicAccountIdentity {
        &self.account
    }

    /// Returns the opaque credential-authority identity.
    #[must_use]
    pub const fn credential_authority(&self) -> &AnthropicCredentialAuthority {
        &self.credential_authority
    }

    /// Returns the exact HTTPS/SSE transport, including its method and path.
    #[must_use]
    pub const fn transport(&self) -> AnthropicMessagesTransport {
        self.transport
    }

    /// Returns the exact Messages dialect and beta selection.
    #[must_use]
    pub const fn dialect(&self) -> &AnthropicMessagesDialect {
        &self.dialect
    }

    /// Returns the exact neutral protocol identity.
    #[must_use]
    pub const fn protocol(&self) -> &ProtocolId {
        &self.protocol
    }

    /// Returns the complete immutable requested-model configuration.
    #[must_use]
    pub const fn model(&self) -> &AnthropicModel {
        &self.model
    }

    /// Returns an exact provider-reported model required by the descriptor.
    ///
    /// `None` means the descriptor deliberately permits the provider to report
    /// a concrete revision distinct from the requested alias.
    #[must_use]
    pub const fn expected_reported_model(&self) -> Option<&ModelId> {
        self.expected_reported_model.as_ref()
    }

    /// Returns the frozen harness profile.
    #[must_use]
    pub const fn profile(&self) -> &ModelAdapterProfileId {
        &self.profile
    }

    /// Returns the exact neutral descriptor binding digest.
    #[must_use]
    pub const fn descriptor_binding(&self) -> Digest {
        self.descriptor_binding
    }

    pub(crate) const fn replay_account_binding(&self) -> Option<Digest> {
        self.replay_account_binding
    }

    pub(crate) const fn semantic_descriptor_binding(&self) -> Digest {
        self.semantic_descriptor_binding
    }

    /// Returns the provider model identity used for replay validation.
    #[must_use]
    pub fn replay_model(&self) -> &ModelId {
        self.expected_reported_model
            .as_ref()
            .unwrap_or_else(|| self.model.remote_id())
    }

    /// Verifies that a secret-bearing route captured by a transport is the
    /// exact route authority frozen into this credential-free binding.
    ///
    /// # Errors
    ///
    /// Returns an invalid-request error before dispatch if the route ID,
    /// origin, audience, account, credential authority, or transport differs.
    pub fn validate_route(&self, route: &AnthropicPlatformRoute) -> Result<(), AnthropicError> {
        if self.route_id != route.id
            || self.origin != route.origin
            || self.auth_audience != route.auth.audience_origin
            || self.account != route.account
            || self.credential_authority != route.auth.credential_authority
            || self.transport != route.transport
        {
            return Err(AnthropicError::new(
                AnthropicErrorKind::InvalidRequest,
                AnthropicErrorPhase::Prepare,
                "Anthropic transport route does not match the prepared target binding",
            ));
        }
        Ok(())
    }
}

impl fmt::Debug for AnthropicMessagesTargetBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AnthropicMessagesTargetBinding")
            .field("route_id", &self.route_id)
            .field("origin", &self.origin)
            .field("auth_audience", &self.auth_audience)
            .field("account", &self.account)
            .field(
                "replay_account_binding",
                &self.replay_account_binding.map(|_| "<redacted>"),
            )
            .field("credential_authority", &self.credential_authority)
            .field("transport", &self.transport)
            .field("method", &self.transport.method())
            .field("path", &self.transport.path())
            .field("dialect", &self.dialect)
            .field("protocol", &self.protocol)
            .field("model", &self.model)
            .field("expected_reported_model", &self.expected_reported_model)
            .field("profile", &self.profile)
            .field("descriptor_binding", &self.descriptor_binding)
            .field(
                "semantic_descriptor_binding",
                &self.semantic_descriptor_binding,
            )
            .field(
                "reviewed_admission_grant",
                &self.reviewed_admission_grant.as_ref().map(|_| "<redacted>"),
            )
            .field("lineage", &self.lineage)
            .finish()
    }
}

/// Codec evidence explicitly scoped to one route, model, and dialect binding.
#[derive(Clone, Copy, Debug)]
pub struct ScopedFeatureEvidence<'a> {
    route: &'a RouteId,
    model: &'a ModelId,
    dialect: &'a AnthropicMessagesDialect,
    entries: &'static [FeatureMatrixEntry],
}

impl<'a> ScopedFeatureEvidence<'a> {
    /// Returns the exact route scope.
    #[must_use]
    pub const fn route(&self) -> &'a RouteId {
        self.route
    }

    /// Returns the exact provider model scope.
    #[must_use]
    pub const fn model(&self) -> &'a ModelId {
        self.model
    }

    /// Returns the exact API-version and beta scope.
    #[must_use]
    pub const fn dialect(&self) -> &'a AnthropicMessagesDialect {
        self.dialect
    }

    /// Returns parser/preservation evidence, never an advertised live claim.
    #[must_use]
    pub const fn entries(&self) -> &'static [FeatureMatrixEntry] {
        self.entries
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests;
