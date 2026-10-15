//! Explicit Anthropic Platform route, model, profile, and secret configuration.

use std::fmt;
use std::sync::Arc;

use plakous_messages::{MessagesAttempt, MessagesAttemptBinding};
use plakous_model::{
    Clock, Digest, DigestDomain, ModelAdapterProfileId, ModelFamilyId, ModelId, ProtocolId,
    ResolvedModelDescriptor, RouteId, SessionLineage, SystemClock, ValidatedRequestPlan,
};
use secrecy::{ExposeSecret as _, SecretString as SecrecyString};

use crate::{
    AnthropicError, AnthropicErrorKind, AnthropicErrorPhase, AnthropicMessagesDialect,
    FeatureMatrixEntry, dialect::MESSAGES_FEATURE_MATRIX_V1,
};

/// The only HTTPS origin authorized by [`AnthropicPlatformRoute`].
pub const ANTHROPIC_PLATFORM_ORIGIN: &str = "https://api.anthropic.com";
/// The only request path authorized by the stable Messages transport.
pub const ANTHROPIC_MESSAGES_PATH: &str = "/v1/messages";

const MAX_ACCOUNT_IDENTITY_BYTES: usize = 256;
const MAX_API_KEY_BYTES: usize = 64 * 1024;

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
    KeyAuthority(Arc<()>),
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

    fn from_key(key: &AnthropicApiKey) -> Self {
        Self(AccountIdentityInner::KeyAuthority(
            key.credential_authority.clone(),
        ))
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
            ) => Arc::ptr_eq(left, right),
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
pub struct AnthropicCredentialAuthority(Arc<()>);

impl AnthropicCredentialAuthority {
    fn from_key(key: &AnthropicApiKey) -> Self {
        Self(key.credential_authority.clone())
    }
}

impl PartialEq for AnthropicCredentialAuthority {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
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

/// A non-serializable Anthropic API key whose formatting is always redacted.
///
/// Non-`Clone`: pass by reference or move ownership. Authority
/// identity is still opaque via an internal token for generation grants.
pub struct AnthropicApiKey {
    value: SecrecyString,
    credential_authority: Arc<()>,
}

impl AnthropicApiKey {
    /// Validates and wraps an API key without inspecting environment variables.
    ///
    /// # Errors
    ///
    /// Returns an invalid-configuration error for an empty key or control bytes.
    pub fn new(value: impl Into<String>) -> Result<Self, AnthropicError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_API_KEY_BYTES
            || value.bytes().any(|byte| byte.is_ascii_control())
        {
            return Err(AnthropicError::new(
                AnthropicErrorKind::InvalidConfiguration,
                AnthropicErrorPhase::Prepare,
                "Anthropic API key must be bounded, non-empty, and contain no control bytes",
            ));
        }
        Ok(Self {
            value: SecrecyString::from(value),
            credential_authority: Arc::new(()),
        })
    }

    #[cfg(feature = "native-auth")]
    fn with_retained_authority(
        value: impl Into<String>,
        credential_authority: Arc<()>,
    ) -> Result<Self, AnthropicError> {
        let key = Self::new(value)?;
        Ok(Self {
            value: key.value,
            credential_authority,
        })
    }

    /// A non-secret fingerprint of the key text: the first 16 hex digits of a
    /// domain-separated SHA-256, enough to key a cache per key. It does not
    /// reveal the key, but anyone holding a candidate key can check it
    /// against the fingerprint, so it stays in owner-only files.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        let mut hex = plakous_model::sha256_hex(&[
            b"anthropic.api-key-fingerprint.v1",
            self.expose().as_bytes(),
        ]);
        hex.truncate(16);
        hex
    }

    pub(crate) fn expose(&self) -> &str {
        self.value.expose_secret()
    }
}

impl fmt::Debug for AnthropicApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AnthropicApiKey(<redacted>)")
    }
}

impl fmt::Display for AnthropicApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

/// Resolves the currently selected API key at preparation and dispatch fences.
/// Implementations must return sanitized errors and must not fall back to OAuth.
pub trait AnthropicApiKeySource: Send + Sync + 'static {
    /// Reads the current key without changing the selected account.
    fn current_key(&self) -> Result<AnthropicApiKey, AnthropicError>;
}

impl AnthropicApiKeySource for AnthropicApiKey {
    fn current_key(&self) -> Result<AnthropicApiKey, AnthropicError> {
        Self::new(self.expose().to_owned())
    }
}

/// Resolved API-key authentication bound to the official Platform origin.
///
/// Clone shares the non-`Clone` key via [`Arc`] without duplicating secret bytes.
#[derive(Clone)]
pub struct AnthropicApiKeyAuth {
    key: Arc<AnthropicApiKey>,
    audience_origin: &'static str,
    credential_authority: AnthropicCredentialAuthority,
}

impl AnthropicApiKeyAuth {
    /// Binds a key to the official Anthropic Platform origin.
    #[must_use]
    pub fn platform(key: AnthropicApiKey) -> Self {
        let credential_authority = AnthropicCredentialAuthority::from_key(&key);
        Self {
            key: Arc::new(key),
            audience_origin: ANTHROPIC_PLATFORM_ORIGIN,
            credential_authority,
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
            .field("key", &"<redacted>")
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

    #[allow(
        dead_code,
        reason = "raw access is reserved for the deferred crate-owned production transport"
    )]
    pub(crate) fn expose_for<T>(
        &self,
        attempt: &MessagesAttempt,
        target: &AnthropicMessagesTargetBinding,
        route: &AnthropicPlatformRoute,
        use_key: impl FnOnce(&str) -> T,
    ) -> Result<T, AnthropicError> {
        self.validate_for_attempt(attempt)?;
        target.validate_route(route)?;
        if self.auth.audience_origin != route.auth.audience_origin
            || self.auth.credential_authority != route.auth.credential_authority
        {
            return Err(credential_binding_error());
        }
        Ok(use_key(self.auth.key.expose()))
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

    pub(crate) fn credential_binding(&self) -> Result<Digest, AnthropicError> {
        digest_identity(
            b"anthropic.platform.credential.v1",
            self.auth.key.expose().as_bytes(),
        )
    }

    pub(crate) fn validate_current_key(
        &self,
        source: &dyn AnthropicApiKeySource,
    ) -> Result<(), AnthropicError> {
        let key = source.current_key()?;
        if self.is_claude_oauth() || key.expose() != self.auth.key.expose() {
            return Err(credential_binding_error());
        }
        Ok(())
    }

    /// Creates the official API-key route. Custom origins require a distinct
    /// route type and explicit credential binding; this type cannot be mutated.
    /// Cloning one [`AnthropicApiKey`] preserves its authority identity;
    /// independently wrapping even identical key text creates a new authority
    /// and therefore requires newly prepared requests. This constructor uses a
    /// conservative key-scoped account identity; use [`Self::for_account`] when
    /// a stable non-secret account/project label is available.
    ///
    /// # Errors
    ///
    /// Returns an invalid-configuration error if the stable route identity
    /// cannot be constructed.
    pub fn new(key: AnthropicApiKey) -> Result<Self, AnthropicError> {
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
        key: AnthropicApiKey,
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
        token: AnthropicApiKey,
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

    /// Replaces the OAuth access token without rotating the prepared credential authority.
    ///
    /// # Errors
    ///
    /// Returns an invalid-configuration error when this is not the Claude OAuth
    /// route or the token is empty/control-bearing.
    #[cfg(feature = "native-auth")]
    pub(crate) fn with_oauth_access_token(
        &self,
        token: impl Into<String>,
    ) -> Result<Self, AnthropicError> {
        if !self.is_claude_oauth() {
            return Err(AnthropicError::new(
                AnthropicErrorKind::InvalidConfiguration,
                AnthropicErrorPhase::Prepare,
                "OAuth access-token rotation requires the Claude OAuth route",
            ));
        }
        let key = AnthropicApiKey::with_retained_authority(
            token,
            Arc::clone(&self.auth.key.credential_authority),
        )?;
        let credential_authority = AnthropicCredentialAuthority::from_key(&key);
        Ok(Self {
            id: self.id.clone(),
            origin: self.origin,
            account: self.account.clone(),
            transport: self.transport,
            auth: AnthropicApiKeyAuth {
                key: Arc::new(key),
                audience_origin: self.auth.audience_origin,
                credential_authority,
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
    ReviewedAdmission {
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

    #[allow(
        clippy::too_many_arguments,
        reason = "the trusted constructor keeps every exact target identity, bounded admission fact, and its clock adjacent"
    )]
    pub(crate) fn new_reviewed_admission(
        dialect: AnthropicMessagesDialect,
        route: AnthropicPlatformRoute,
        model: AnthropicModel,
        profile: ClaudeProfile,
        descriptor: ResolvedModelDescriptor,
        revision: &'static str,
        reviewed_at: plakous_model::Timestamp,
        expires_at: plakous_model::Timestamp,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, AnthropicError> {
        let mut resolved = Self::new(dialect, route, model, profile, descriptor)?;
        resolved.resolution_trust = AnthropicResolutionTrust::ReviewedAdmission {
            revision,
            reviewed_at,
            expires_at,
        };
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
            AnthropicResolutionTrust::ReviewedAdmission {
                reviewed_at,
                expires_at,
                ..
            } if reviewed_at <= now && now < expires_at => Ok(()),
            AnthropicResolutionTrust::ReviewedAdmission { .. } => Err(AnthropicError::new(
                AnthropicErrorKind::InvalidConfiguration,
                AnthropicErrorPhase::Prepare,
                "Anthropic reviewed admission is expired or not yet valid",
            )),
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
mod tests {
    use super::*;

    #[test]
    fn credential_debug_and_display_are_redacted() -> Result<(), AnthropicError> {
        let sentinel = "sk-ant-sentinel-never-print";
        // AnthropicApiKey is intentionally non-`Clone` (secrecy hygiene):
        // independently wrapping the same text creates a new authority token.
        let key = AnthropicApiKey::new(sentinel)?;
        assert!(!format!("{key:?} {key}").contains(sentinel));
        let route = AnthropicPlatformRoute::new(key)?;
        assert!(!format!("{route:?}").contains(sentinel));

        let independently_wrapped = AnthropicPlatformRoute::new(AnthropicApiKey::new(sentinel)?)?;
        assert_ne!(route.account(), independently_wrapped.account());
        assert_ne!(
            route.auth().credential_authority(),
            independently_wrapped.auth().credential_authority()
        );
        Ok(())
    }

    #[test]
    fn api_replay_account_tracks_key_generation_without_changing_the_selection_label()
    -> Result<(), AnthropicError> {
        let route = |key| {
            AnthropicPlatformRoute::for_account(
                AnthropicApiKey::new(key)?,
                AnthropicAccountIdentity::new("stable-account")?,
            )
        };
        let first = route("offline-one")?;
        let reconstructed = route("offline-one")?;
        let rotated = route("offline-two")?;
        assert_eq!(first.account(), rotated.account());
        assert_eq!(
            first.replay_account_binding()?,
            reconstructed.replay_account_binding()?
        );
        assert_ne!(
            first.replay_account_binding()?,
            rotated.replay_account_binding()?
        );
        Ok(())
    }

    #[test]
    fn credential_authority_is_exact_and_independent_of_stable_account_identity()
    -> Result<(), AnthropicError> {
        let account = AnthropicAccountIdentity::new("stable-account")?;
        let first = AnthropicPlatformRoute::for_account(
            AnthropicApiKey::new("sk-ant-generation-one")?,
            account.clone(),
        )?;
        let rotated = AnthropicPlatformRoute::for_account(
            AnthropicApiKey::new("sk-ant-generation-two")?,
            account,
        )?;
        assert_eq!(first.account(), rotated.account());
        assert_ne!(
            first.auth().credential_authority(),
            rotated.auth().credential_authority()
        );
        Ok(())
    }

    #[cfg(feature = "native-auth")]
    #[test]
    fn oauth_access_token_rotation_retains_prepared_credential_authority()
    -> Result<(), AnthropicError> {
        let route = AnthropicPlatformRoute::claude_oauth(
            AnthropicApiKey::new("oauth-access-token-one")?,
            AnthropicAccountIdentity::new("grant-fixture-authority")?,
        )?;
        let rotated = route.with_oauth_access_token("oauth-access-token-two")?;
        assert!(rotated.is_claude_oauth());
        assert_eq!(route.id(), rotated.id());
        assert_eq!(route.account(), rotated.account());
        assert_eq!(
            route.auth().credential_authority(),
            rotated.auth().credential_authority()
        );
        assert!(Arc::ptr_eq(
            &route.auth.key.credential_authority,
            &rotated.auth.key.credential_authority
        ));
        assert_ne!(route.auth.key.expose(), rotated.auth.key.expose());
        Ok(())
    }

    #[cfg(feature = "native-auth")]
    #[test]
    fn oauth_access_token_rotation_rejects_platform_and_empty_tokens() -> Result<(), AnthropicError>
    {
        let platform = AnthropicPlatformRoute::new(AnthropicApiKey::new("sk-ant-platform")?)?;
        assert!(
            platform
                .with_oauth_access_token("oauth-access-token")
                .is_err()
        );
        let oauth = AnthropicPlatformRoute::claude_oauth(
            AnthropicApiKey::new("oauth-access-token-one")?,
            AnthropicAccountIdentity::new("grant-fixture-authority")?,
        )?;
        assert!(oauth.with_oauth_access_token("").is_err());
        assert!(oauth.with_oauth_access_token("oauth\naccess").is_err());
        Ok(())
    }
}
