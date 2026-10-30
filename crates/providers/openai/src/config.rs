//! Protocol, route, authentication, and route-scoped target configuration.

mod codex;
mod error;
mod platform;
mod route;
mod target;

pub use codex::CodexSubscriptionRouteConfig;
pub use error::{AuthError, ConfigError};
pub use platform::OpenAiPlatformRouteConfig;

use std::fmt;
use std::sync::Arc;
#[cfg(feature = "native-auth")]
use std::time::Instant;
#[cfg(feature = "native-auth")]
use std::{future::Future, pin::Pin};

#[cfg(feature = "native-auth")]
use plakous_auth::CredentialFence;
#[cfg(feature = "native-auth")]
use plakous_auth::subscription_limit::SubscriptionRequestPermit;
#[cfg(feature = "native-auth")]
use plakous_model::Timestamp;
#[cfg(feature = "native-auth")]
use plakous_model::{CancellationToken, RuntimeModelAuthority, RuntimeModelAuthorityError};
use plakous_model::{
    Capability, Clock, Digest, DigestDomain, FactSource, ModelAdapterProfileId, ModelId,
    ModelRequest, ProtocolId, ResolvedModelDescriptor, RouteId, Support, SystemClock,
    ValidatedRequestPlan,
};
use plakous_responses_dialect::CredentialGenerationFingerprint;
#[cfg(feature = "native-auth")]
use plakous_responses_dialect::ResponsesPhysicalRequestBinding;
use secrecy::{ExposeSecret as _, SecretString as SecrecyString};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use url::Url;

use crate::manifest::validate_reviewed_platform_request;
#[cfg(feature = "native-auth")]
use crate::manifest::validate_unscoped_adapter_request;
use crate::manifest::{ReviewedPlatformManifestWitness, validate_reviewed_platform_witness};
#[cfg(feature = "native-auth")]
use crate::{CodexSubscriptionCredentialManager, NativeCodexAuthError, NativeCodexAuthorization};

/// Semantic revision of the thin OpenAI Responses adapter.
pub const OPENAI_RESPONSES_ADAPTER_REVISION: &str = "plak-openai-responses-v6";
/// Numeric closed-data revision supplied to the shared Responses host.
pub(crate) const OPENAI_RESPONSES_DIALECT_REVISION: u32 = 6;

/// The stable `OpenAI` Platform route identifier.
pub const OPENAI_PLATFORM_ROUTE_ID: &str = "openai.platform";
/// The stable direct ChatGPT/Codex subscription route identifier.
pub const OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID: &str = "openai.codex_subscription";
/// The stable documented `OpenAI` Responses protocol identifier.
pub const OPENAI_RESPONSES_PROTOCOL_ID: &str = "openai.responses";
/// The only origin authorized for the first-party `OpenAI` Platform route.
pub const OPENAI_PLATFORM_ORIGIN: &str = "https://api.openai.com/";
/// The only origin authorized for direct ChatGPT/Codex subscription requests.
pub const OPENAI_CHATGPT_ORIGIN: &str = "https://chatgpt.com/";
/// The fixed Responses resource path for the first-party Platform route.
pub(crate) const OPENAI_PLATFORM_RESPONSES_PATH: &str = "/v1/responses";
/// Exact API-key authentication profile selected for Platform requests.
pub const OPENAI_PLATFORM_AUTH_PROFILE_ID: &str = "openai.platform.api_key";
/// Revision of the reviewed Platform API-key header contract.
pub const OPENAI_PLATFORM_AUTH_REVISION: &str = "openai-platform-api-key-headers-v1";
/// Authority/billing class for separately billed Platform API-key requests.
pub const OPENAI_PLATFORM_AUTHORITY_CLASS: &str = "paid-api-key";
/// Closed non-secret credential selection used by the Platform configuration compiler.
pub const OPENAI_PLATFORM_CREDENTIAL_SELECTION: &str = "primary";
/// The separate direct Codex product Responses dialect identifier.
pub const OPENAI_CODEX_RESPONSES_PROTOCOL_ID: &str = "openai.codex.responses";
/// The fixed Responses resource path for the direct Codex product route.
pub const OPENAI_CODEX_RESPONSES_PATH: &str = "/backend-api/codex/responses";
/// Exact Codex include selector used to retain replayable reasoning material.
pub const OPENAI_CODEX_ENCRYPTED_REASONING_INCLUDE: &str = "reasoning.encrypted_content";
/// The fixed authenticated model-catalog path for the direct Codex route.
#[cfg(feature = "native-auth")]
pub(crate) const OPENAI_CODEX_MODELS_PATH: &str = "/backend-api/codex/models";
/// Exact reference-client release presented to the authenticated catalog.
///
/// The backend versions the returned model projection from this value. It is
/// therefore compatibility identity, not Plak's package version or a minimum
/// version floor.
pub const OPENAI_CODEX_REFERENCE_CLIENT_VERSION: &str = "0.162.1";
/// Official Codex revision defining the pinned catalog contract.
#[cfg(feature = "native-auth")]
pub(crate) const OPENAI_CODEX_CATALOG_SOURCE_REVISION: &str =
    "092d3acd6bec3e3a14bdc7e7a2810ab628ab759d";
/// Immutable native compatibility profile accepted by this revision.
pub const OPENAI_CODEX_COMPATIBILITY_PROFILE_ID: &str = "openai.codex.compat.2026-07-31.r1";
/// Official Codex source revision pinned by the native compatibility profile.
pub const OPENAI_CODEX_SOURCE_REVISION: &str = "c4f2746c43a3ab87c2978657d7f2c274546e97fe";
/// Native Codex route conformance revision.
#[cfg(feature = "native-auth")]
pub(crate) const OPENAI_CODEX_CONFORMANCE_REVISION: &str = "plak-openai-codex-compat-2026-07-31-r1";
/// Version of the conservative O1 Platform wire-conformance evidence.
pub(crate) const OPENAI_O1_CONFORMANCE_REVISION: &str = "plak-openai-o1-platform-wire-v1";

/// A harness-relevant Responses feature with an independent support claim.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum ResponsesFeature {
    /// HTTP Server-Sent Events response transport.
    HttpSse,
    /// Ordered assistant message output.
    MessageOutput,
    /// Assistant refusal output.
    RefusalOutput,
    /// Opaque or summarized reasoning output and exact replay.
    ReasoningOutput,
    /// Caller-advertised function tools and calls.
    ClientFunctionCalls,
    /// Free-form custom tool definitions and calls.
    CustomToolCalls,
    /// Multiple independently indexed client function calls.
    ParallelFunctionCalls,
    /// Provider-hosted programmatic callers or containers.
    ProgrammaticCallers,
    /// Provider-hosted tools other than function-caller lineage.
    HostedTools,
    /// Provider approval request and response items.
    Approvals,
    /// Provider compaction items.
    Compaction,
    /// Typed annotations and citations.
    Citations,
    /// URL or inline image input.
    ImageInput,
    /// Schema-constrained response output.
    StructuredOutput,
    /// Exact full output-item replay.
    FullOutputReplay,
    /// Stored-response ID continuation.
    PreviousResponseId,
    /// Stable incomplete-response terminal grammar.
    IncompleteResponse,
    /// Prompt or prefix caching controls.
    PromptCaching,
    /// Provider-native option namespaces.
    NativeOptions,
    /// Persistent Responses WebSocket transport.
    WebSocket,
    /// Durable provider Conversation objects.
    Conversations,
    /// Newly observed output items retained opaquely without a semantic claim.
    UnknownOutputItems,
}

/// Conservative support level in a versioned route matrix.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConformanceStatus {
    /// Covered by this revision's deterministic offline conformance tests.
    Supported,
    /// Deliberately outside the declared O1 adapter surface.
    Excluded,
    /// No positive or negative semantic conformance claim exists yet.
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConformanceScope {
    PlatformO1,
    #[cfg(feature = "native-auth")]
    CodexCompatibility,
}

/// Versioned, route-specific feature claims that constrain target construction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpenAiRouteConformance {
    revision: &'static str,
    scope: ConformanceScope,
}

impl OpenAiRouteConformance {
    const fn platform_o1() -> Self {
        Self {
            revision: OPENAI_O1_CONFORMANCE_REVISION,
            scope: ConformanceScope::PlatformO1,
        }
    }

    #[cfg(feature = "native-auth")]
    const fn codex_compatibility() -> Self {
        Self {
            revision: OPENAI_CODEX_CONFORMANCE_REVISION,
            scope: ConformanceScope::CodexCompatibility,
        }
    }

    /// Returns the immutable evidence revision.
    #[must_use]
    pub const fn revision(self) -> &'static str {
        self.revision
    }

    /// Returns the explicit status for one feature.
    #[must_use]
    pub const fn feature(self, feature: ResponsesFeature) -> ConformanceStatus {
        #[cfg(feature = "native-auth")]
        if matches!(self.scope, ConformanceScope::CodexCompatibility) {
            return match feature {
                ResponsesFeature::HttpSse
                | ResponsesFeature::MessageOutput
                | ResponsesFeature::ReasoningOutput
                | ResponsesFeature::ClientFunctionCalls
                | ResponsesFeature::CustomToolCalls
                // Full coding tools declare Parallel concurrency; Codex Responses
                // admits parallel function calls on the same wire surface as tools.
                | ResponsesFeature::ParallelFunctionCalls
                | ResponsesFeature::FullOutputReplay
                | ResponsesFeature::IncompleteResponse
                // User `input_image`; the catalog row's `ImageInput` fact
                // gates each model.
                | ResponsesFeature::ImageInput
                // Codex OAuth remote compact is the exact
                // `plakous.compaction` native-option namespace, not a blanket
                // native-options bag. Platform API-key still excludes this.
                | ResponsesFeature::NativeOptions => ConformanceStatus::Supported,
                _ => ConformanceStatus::Excluded,
            };
        }
        match feature {
            ResponsesFeature::MessageOutput
            | ResponsesFeature::RefusalOutput
            | ResponsesFeature::ReasoningOutput
            | ResponsesFeature::ClientFunctionCalls
            | ResponsesFeature::CustomToolCalls
            | ResponsesFeature::ParallelFunctionCalls
            | ResponsesFeature::ProgrammaticCallers
            | ResponsesFeature::Citations
            | ResponsesFeature::StructuredOutput
            | ResponsesFeature::FullOutputReplay
            | ResponsesFeature::PreviousResponseId
            | ResponsesFeature::IncompleteResponse
            | ResponsesFeature::ImageInput
            | ResponsesFeature::HttpSse => ConformanceStatus::Supported,
            ResponsesFeature::HostedTools
            | ResponsesFeature::Approvals
            | ResponsesFeature::Compaction
            | ResponsesFeature::PromptCaching
            | ResponsesFeature::NativeOptions
            | ResponsesFeature::WebSocket
            | ResponsesFeature::Conversations => ConformanceStatus::Excluded,
            ResponsesFeature::UnknownOutputItems => ConformanceStatus::Unknown,
        }
    }

    pub(crate) fn supports(self, feature: ResponsesFeature) -> bool {
        self.feature(feature) == ConformanceStatus::Supported
    }

    // The offline SSE fixtures establish the codec grammar, not live Platform
    // transport support. This evidence is accepted only by the explicitly
    // non-routable offline target constructor.
    const fn has_fixture_codec_evidence(self, feature: ResponsesFeature) -> bool {
        matches!(self.scope, ConformanceScope::PlatformO1)
            && matches!(feature, ResponsesFeature::HttpSse)
    }
}

/// A versioned Responses wire dialect, independent of route and model names.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponsesDialect {
    protocol: ProtocolId,
    revision: String,
}

impl ResponsesDialect {
    /// Returns the documented Platform Responses dialect targeted by this adapter.
    #[must_use]
    pub fn platform() -> Self {
        Self {
            protocol: ProtocolId::new(OPENAI_RESPONSES_PROTOCOL_ID)
                .unwrap_or_else(|error| unreachable!("static protocol ID is valid: {error}")),
            revision: "2025-03-11".to_owned(),
        }
    }

    /// Returns the independently versioned direct Codex product dialect.
    #[must_use]
    pub fn codex_subscription() -> Self {
        Self {
            protocol: ProtocolId::new(OPENAI_CODEX_RESPONSES_PROTOCOL_ID)
                .unwrap_or_else(|error| unreachable!("static protocol ID is valid: {error}")),
            revision: "codex-product-v1".to_owned(),
        }
    }

    /// Creates an explicit Responses-shaped dialect for offline conformance work.
    ///
    /// Construction declares grammar identity only. It grants no route support
    /// claim and supplies no credential.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid protocol ID or empty revision.
    pub fn explicit(
        protocol: ProtocolId,
        revision: impl Into<String>,
    ) -> Result<Self, ConfigError> {
        let revision = revision.into();
        if revision.is_empty() {
            return Err(ConfigError::EmptyField("dialect revision"));
        }
        Ok(Self { protocol, revision })
    }

    /// Returns the stable protocol-dialect identity.
    #[must_use]
    pub const fn protocol(&self) -> &ProtocolId {
        &self.protocol
    }

    /// Returns the targeted wire revision.
    #[must_use]
    pub fn revision(&self) -> &str {
        &self.revision
    }
}

/// A validated HTTPS endpoint origin with no path, query, fragment, or user info.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct EndpointOrigin(Url);

impl EndpointOrigin {
    /// Parses a credential-safe HTTPS origin.
    ///
    /// # Errors
    ///
    /// Returns an error unless `value` is an absolute HTTPS origin without
    /// embedded credentials or non-root URL components.
    pub fn parse(value: &str) -> Result<Self, ConfigError> {
        let url = Url::parse(value).map_err(|_| ConfigError::InvalidOrigin)?;
        let root_path = url.path().is_empty() || url.path() == "/";
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || !root_path
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(ConfigError::InvalidOrigin);
        }
        Ok(Self(url))
    }

    /// Returns the normalized origin text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for EndpointOrigin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_str().fmt(formatter)
    }
}

impl fmt::Display for EndpointOrigin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_str().fmt(formatter)
    }
}

impl Serialize for EndpointOrigin {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for EndpointOrigin {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

/// A secret string whose diagnostics never expose its contents.
///
/// Backed by [`secrecy::SecretString`] and zeroized on drop. This wrapper is
/// deliberately non-`Clone` and provides no public secret-exposure method so
/// Platform bearers stay awkward to inspect and cannot cross a serialization
/// boundary.
///
/// ```compile_fail
/// use plakous_openai::SecretString;
///
/// fn requires_serialize<T: serde::Serialize>() {}
/// requires_serialize::<SecretString>();
/// ```
///
/// ```compile_fail
/// use plakous_openai::SecretString;
///
/// let secret = SecretString::new("synthetic-sentinel")?;
/// let duplicate = secret.clone();
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// ```compile_fail
/// use plakous_openai::SecretString;
///
/// let secret = SecretString::new("synthetic-sentinel")?;
/// let raw = secret.expose_secret();
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct SecretString(SecrecyString);

impl SecretString {
    /// Wraps a non-empty secret.
    ///
    /// # Errors
    ///
    /// Returns an error when the supplied secret is empty.
    pub fn new(value: impl Into<String>) -> Result<Self, ConfigError> {
        let value = value.into();
        if value.is_empty() {
            return Err(ConfigError::EmptyField("secret"));
        }
        Ok(Self(SecrecyString::from(value)))
    }

    pub(crate) fn expose(&self) -> &str {
        self.0.expose_secret()
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretString(<redacted>)")
    }
}

/// A closed, serializable reference to an application-owned Platform credential.
#[derive(Clone, Eq, PartialEq)]
pub struct CredentialReference {
    id: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CredentialReferenceWire {
    kind: CredentialReferenceKind,
    id: String,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum CredentialReferenceKind {
    Application,
}

impl CredentialReference {
    /// Creates a non-empty application-owned credential reference.
    ///
    /// # Errors
    ///
    /// Returns an error when the reference is empty.
    pub fn application(value: impl Into<String>) -> Result<Self, ConfigError> {
        let id = value.into();
        if id.is_empty() {
            return Err(ConfigError::EmptyField("credential reference"));
        }
        if id.len() > 256
            || !id.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
            })
        {
            return Err(ConfigError::InvalidCredentialReference);
        }
        Ok(Self { id })
    }

    /// Returns the reference identifier, never a credential value.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.id
    }
}

/// Caller-injected in-memory Platform API-key source bound to one exact
/// application credential reference and the official Platform origin.
pub struct StaticPlatformCredentialSource {
    key: SecretString,
    generation: Option<OpenAiApiKeyGenerationGrant>,
    reference: CredentialReference,
    authority: CredentialAuthorityId,
}

impl StaticPlatformCredentialSource {
    /// Creates one exact in-memory source without exposing the key through
    /// cloning, formatting, or serialization.
    #[must_use]
    pub fn new(
        key: SecretString,
        reference: CredentialReference,
        authority: CredentialAuthorityId,
    ) -> Self {
        let generation = OpenAiApiKeyGenerationGrant::from_secret(&key).ok();
        Self {
            key,
            generation,
            reference,
            authority,
        }
    }
}

impl PlatformCredentialSource for StaticPlatformCredentialSource {
    fn credential_authority(&self) -> &CredentialAuthorityId {
        &self.authority
    }

    fn api_key(
        &self,
        reference: &CredentialReference,
        origin: &EndpointOrigin,
    ) -> Result<SecretString, AuthError> {
        if reference != &self.reference || origin.as_str() != OPENAI_PLATFORM_ORIGIN {
            return Err(AuthError::BindingMismatch);
        }
        SecretString::new(self.key.expose()).map_err(|_| AuthError::SourceFailure)
    }

    fn generation_grant(
        &self,
        reference: &CredentialReference,
        origin: &EndpointOrigin,
    ) -> Result<OpenAiApiKeyGenerationGrant, AuthError> {
        if reference != &self.reference || origin.as_str() != OPENAI_PLATFORM_ORIGIN {
            return Err(AuthError::BindingMismatch);
        }
        self.generation.ok_or(AuthError::SourceFailure)
    }

    fn api_key_for_generation(
        &self,
        reference: &CredentialReference,
        origin: &EndpointOrigin,
        expected: OpenAiApiKeyGenerationGrant,
    ) -> Result<SecretString, AuthError> {
        let current = self.generation_grant(reference, origin)?;
        if current != expected {
            return Err(AuthError::BindingMismatch);
        }
        SecretString::new(self.key.expose()).map_err(|_| AuthError::SourceFailure)
    }
}

impl fmt::Debug for StaticPlatformCredentialSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StaticPlatformCredentialSource")
            .field("key", &"<redacted>")
            .field("reference", &"<redacted>")
            .field("authority", &"<redacted>")
            .finish()
    }
}

impl fmt::Debug for CredentialReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CredentialReference(<redacted>)")
    }
}

impl Serialize for CredentialReference {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        CredentialReferenceWire {
            kind: CredentialReferenceKind::Application,
            id: self.id.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CredentialReference {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = CredentialReferenceWire::deserialize(deserializer)?;
        match wire.kind {
            CredentialReferenceKind::Application => Self::application(wire.id),
        }
        .map_err(serde::de::Error::custom)
    }
}

/// A stable, non-secret identity for an application-owned credential authority.
///
/// This identifies the resolver or credential manager, not a bearer credential
/// or account label. Diagnostics redact the identifier even though callers must
/// classify it as non-secret before construction.
#[derive(Clone, Eq, PartialEq)]
pub struct CredentialAuthorityId(String);

impl CredentialAuthorityId {
    /// Creates a bounded application-owned credential-authority identity.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty, oversized, or unsafe identifier.
    pub fn new(value: impl Into<String>) -> Result<Self, ConfigError> {
        let value = value.into();
        if value.is_empty() {
            return Err(ConfigError::EmptyField("credential authority"));
        }
        if value.len() > 256
            || !value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
            })
        {
            return Err(ConfigError::InvalidCredentialAuthority);
        }
        Ok(Self(value))
    }

    /// Returns the stable non-secret application identity.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for CredentialAuthorityId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CredentialAuthorityId(<redacted>)")
    }
}

/// Non-secret authority for one exact current `OpenAI` API-key generation.
///
/// The grant retains only a provider-tagged digest. Public credential-source
/// implementations can construct one from a generation-unique nonce or fence;
/// the raw binding is hashed and never retained.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct OpenAiApiKeyGenerationGrant(CredentialGenerationFingerprint);

impl OpenAiApiKeyGenerationGrant {
    /// Constructs a grant from canonical bytes unique to one API-key generation.
    ///
    /// # Errors
    ///
    /// Rejects an empty or oversized binding.
    pub fn from_generation_binding(binding: &[u8]) -> Result<Self, ConfigError> {
        mint_openai_generation(binding)
            .map(Self)
            .map_err(|_| ConfigError::InvalidCredentialGenerationBinding)
    }

    fn from_secret(secret: &SecretString) -> Result<Self, AuthError> {
        mint_openai_generation(secret.expose().as_bytes())
            .map(Self)
            .map_err(|_| AuthError::SourceFailure)
    }

    pub(crate) const fn fingerprint(self) -> CredentialGenerationFingerprint {
        self.0
    }
}

pub(crate) fn mint_openai_generation(
    binding: &[u8],
) -> Result<
    CredentialGenerationFingerprint,
    plakous_responses_dialect::CredentialGenerationFingerprintError,
> {
    CredentialGenerationFingerprint::mint_exact(
        plakous_responses_dialect::CredentialProviderTag::OpenAiPlatform,
        binding,
    )
}

impl fmt::Debug for OpenAiApiKeyGenerationGrant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OpenAiApiKeyGenerationGrant(<redacted fingerprint>)")
    }
}

/// An application-owned, refresh-capable source for Platform API credentials.
pub trait PlatformCredentialSource: Send + Sync + 'static {
    /// Returns the stable non-secret identity of this credential authority.
    ///
    /// Separate resolver instances or stores that can select different
    /// credentials must use different identities. The identifier is digested
    /// before it enters a prepared operation binding.
    fn credential_authority(&self) -> &CredentialAuthorityId;

    /// Resolves an origin-bound API key for one request.
    ///
    /// # Errors
    ///
    /// Returns a redaction-safe authorization error when resolution fails.
    fn api_key(
        &self,
        reference: &CredentialReference,
        origin: &EndpointOrigin,
    ) -> Result<SecretString, AuthError>;

    /// Returns a non-secret lease for the exact API-key generation that would
    /// be resolved now.
    ///
    /// The compatibility default fails closed without calling [`Self::api_key`].
    /// A source must override this method with a generation nonce, fence, or
    /// other non-secret grant to make paid dispatch eligible. Secret material
    /// may be resolved only later through [`Self::api_key_for_generation`],
    /// after the paid authority has reserved the exact admitted request.
    ///
    /// # Errors
    ///
    /// Returns a redaction-safe source or binding error.
    fn generation_grant(
        &self,
        _reference: &CredentialReference,
        _origin: &EndpointOrigin,
    ) -> Result<OpenAiApiKeyGenerationGrant, AuthError> {
        Err(AuthError::SourceFailure)
    }

    /// Resolves a bearer only if it still matches an admitted generation.
    ///
    /// The compatibility default resolves once, derives its non-secret grant,
    /// and rejects a generation swap before returning the bearer.
    ///
    /// # Errors
    ///
    /// Returns a redaction-safe source or binding error.
    fn api_key_for_generation(
        &self,
        reference: &CredentialReference,
        origin: &EndpointOrigin,
        expected: OpenAiApiKeyGenerationGrant,
    ) -> Result<SecretString, AuthError> {
        let key = self.api_key(reference, origin)?;
        if OpenAiApiKeyGenerationGrant::from_secret(&key)? != expected {
            return Err(AuthError::BindingMismatch);
        }
        Ok(key)
    }
}

/// Explicit Platform response-retention policy.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformStoragePolicy {
    /// Request provider-stored Responses.
    StoreResponses,
    /// Send `store=false` and retain required replay locally.
    Stateless,
}

/// Explicit `OpenAI-Project` header disposition for Platform API-key requests.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformProjectPolicy {
    /// Deliberately omit `OpenAI-Project` and use the API key's default project.
    DefaultProject,
    /// Emit the exact bounded project identifier.
    Project(OpenAiProjectId),
}

/// Bounded, non-secret `OpenAI` project identifier.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct OpenAiProjectId(String);

impl OpenAiProjectId {
    /// Creates an exact project identifier suitable for the `OpenAI-Project` header.
    pub fn new(value: impl Into<String>) -> Result<Self, ConfigError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 256
            || !value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':')
            })
        {
            return Err(ConfigError::InvalidProjectId);
        }
        Ok(Self(value))
    }

    /// Returns the exact header value.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for OpenAiProjectId {
    type Error = ConfigError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<OpenAiProjectId> for String {
    fn from(value: OpenAiProjectId) -> Self {
        value.0
    }
}

#[cfg(all(test, feature = "native-auth"))]
pub(crate) fn native_codex_transport_conformance_route(
    origin: EndpointOrigin,
    manager: Arc<CodexSubscriptionCredentialManager>,
) -> Result<OpenAiRoute, ConfigError> {
    manager
        .validate_route_resolution()
        .map_err(|_| ConfigError::SubscriptionCredentialUnavailable)?;
    let authority: Arc<dyn NativeCodexCredentialAuthority> = manager;
    Ok(OpenAiRoute::new(
        OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID,
        ResponsesDialect::codex_subscription(),
        origin,
        StorageMode::Stateless,
        OpenAiRouteConformance::codex_compatibility(),
        RouteAuth::NativeCodexSubscription { authority },
    ))
}

/// Whether a route can dispatch an inference request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Routability {
    /// All explicit authorization dependencies are installed.
    Routable,
    /// Dispatch is forbidden until an authorization dependency is injected.
    NonRoutable,
}

/// The effective provider storage behavior for a route.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StorageMode {
    /// The Platform route may retain a stored Response.
    ProviderStored,
    /// The route sends `store=false` and relies on canonical local replay.
    Stateless,
}

/// Credential and billing access mode bound into continuation state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessMode {
    /// Separately billed Platform API access.
    PlatformApi,
    /// Sealed HTTPS loopback fixture; never production access.
    LocalFixture,
    /// Direct subscription allowance access.
    CodexSubscription,
}

#[derive(Clone)]
enum RouteAuth {
    Platform {
        reference: CredentialReference,
        source: Arc<dyn PlatformCredentialSource>,
        project: PlatformProjectPolicy,
        local_fixture: bool,
    },
    #[cfg(feature = "native-auth")]
    NativeCodexSubscription {
        authority: Arc<dyn NativeCodexCredentialAuthority>,
    },
}

#[cfg(feature = "native-auth")]
pub(crate) type NativeCodexPreflight<'a> = Pin<
    Box<dyn Future<Output = Result<NativeCodexAuthorization, NativeCodexAuthError>> + Send + 'a>,
>;

/// Forced claimed refresh run after the provider rejected an access token.
#[cfg(feature = "native-auth")]
pub(crate) type NativeCodexForcedRefresh<'a> =
    Pin<Box<dyn Future<Output = Result<(), AuthError>> + Send + 'a>>;

#[cfg(feature = "native-auth")]
pub(crate) type NativeCodexRuntimeAuthority<'a> = Pin<
    Box<dyn Future<Output = Result<RuntimeModelAuthority, RuntimeModelAuthorityError>> + Send + 'a>,
>;

/// Exact shared-host generation bound to one asynchronous native credential lookup.
#[cfg(feature = "native-auth")]
#[derive(Clone)]
pub(crate) struct NativeCodexPreflightRequest {
    target: OpenAiTarget,
    physical: ResponsesPhysicalRequestBinding,
    deadline: Instant,
    cancellation: CancellationToken,
}

#[cfg(feature = "native-auth")]
impl NativeCodexPreflightRequest {
    pub(crate) fn new(
        target: OpenAiTarget,
        physical: ResponsesPhysicalRequestBinding,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            target,
            physical,
            deadline,
            cancellation,
        }
    }

    pub(crate) const fn target(&self) -> &OpenAiTarget {
        &self.target
    }

    pub(crate) const fn physical(&self) -> &ResponsesPhysicalRequestBinding {
        &self.physical
    }

    pub(crate) const fn deadline(&self) -> Instant {
        self.deadline
    }

    pub(crate) const fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }
}

/// Resolver-issued witness for one exact native catalog descriptor.
///
/// The type and constructor are crate-private so application code cannot turn
/// an arbitrary model/profile/capability descriptor into a live native target.
#[cfg(feature = "native-auth")]
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct NativeCodexCatalogWitness {
    binding: Digest,
    credential_fence: CredentialFence,
    credential_selection_epoch: Digest,
    observed_at: Timestamp,
    expires_at: Timestamp,
    responses_lite: bool,
}

#[cfg(feature = "native-auth")]
impl NativeCodexCatalogWitness {
    pub(crate) const fn new(
        binding: Digest,
        credential_fence: CredentialFence,
        credential_selection_epoch: Digest,
        observed_at: Timestamp,
        expires_at: Timestamp,
        responses_lite: bool,
    ) -> Self {
        Self {
            binding,
            credential_fence,
            credential_selection_epoch,
            observed_at,
            expires_at,
            responses_lite,
        }
    }

    pub(crate) const fn binding(&self) -> Digest {
        self.binding
    }

    pub(crate) const fn credential_fence(&self) -> &CredentialFence {
        &self.credential_fence
    }

    pub(crate) const fn credential_selection_epoch(&self) -> Digest {
        self.credential_selection_epoch
    }

    pub(crate) const fn observed_at(&self) -> Timestamp {
        self.observed_at
    }

    pub(crate) const fn expires_at(&self) -> Timestamp {
        self.expires_at
    }

    pub(crate) const fn responses_lite(&self) -> bool {
        self.responses_lite
    }
}

#[cfg(feature = "native-auth")]
impl fmt::Debug for NativeCodexCatalogWitness {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NativeCodexCatalogWitness(<redacted exact credential fence>)")
    }
}

/// Private credential-authority seam retained only by an admitted native route.
#[cfg(feature = "native-auth")]
pub(crate) trait NativeCodexCredentialAuthority: Send + Sync + 'static {
    fn route_account_binding(&self) -> Digest;
    fn authority_binding(&self) -> Digest;
    fn validate_catalog_witness(
        &self,
        descriptor: &ResolvedModelDescriptor,
        witness: &NativeCodexCatalogWitness,
    ) -> Result<(), AuthError>;
    fn revalidate_runtime_authority<'a>(
        &'a self,
        descriptor: &'a ResolvedModelDescriptor,
        witness: &'a NativeCodexCatalogWitness,
        checked_at: Timestamp,
    ) -> NativeCodexRuntimeAuthority<'a>;
    fn preflight(&self, request: NativeCodexPreflightRequest) -> NativeCodexPreflight<'_>;
    /// Forces one claimed refresh after the provider answered an inference
    /// dispatch with HTTP 401, bound to the rejected receipt's account and
    /// login selection: it never claims, rotates or quarantines another login.
    fn refresh_after_rejection<'a>(
        &'a self,
        rejected: &'a CredentialFence,
        selection_epoch: Digest,
    ) -> NativeCodexForcedRefresh<'a>;
    fn acquire_inference(
        &self,
        input_token_upper_bound: u64,
    ) -> Result<Box<dyn SubscriptionRequestPermit>, AuthError>;
    fn bind(
        &self,
        authorization: &mut NativeCodexAuthorization,
        physical_request: ResponsesPhysicalRequestBinding,
    ) -> Result<(), AuthError>;
}

/// An immutable resolved `OpenAI` route. It contains no fallback route.
#[derive(Clone)]
pub struct OpenAiRoute {
    id: RouteId,
    dialect: ResponsesDialect,
    origin: EndpointOrigin,
    storage: StorageMode,
    conformance: OpenAiRouteConformance,
    auth: RouteAuth,
    account_binding: Digest,
    credential_authority_binding: Digest,
}

impl OpenAiRoute {
    fn new(
        id: &str,
        dialect: ResponsesDialect,
        origin: EndpointOrigin,
        storage: StorageMode,
        conformance: OpenAiRouteConformance,
        auth: RouteAuth,
    ) -> Self {
        let account_binding = account_binding(&origin, &auth);
        let credential_authority_binding = credential_authority_binding(&auth);
        Self {
            id: RouteId::new(id)
                .unwrap_or_else(|error| unreachable!("static route ID is valid: {error}")),
            dialect,
            origin,
            storage,
            conformance,
            auth,
            account_binding,
            credential_authority_binding,
        }
    }
}

fn account_binding(origin: &EndpointOrigin, auth: &RouteAuth) -> Digest {
    let result = match auth {
        RouteAuth::Platform {
            reference, project, ..
        } => Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[
                b"openai-platform-credential-reference",
                origin.as_str().as_bytes(),
                reference.as_str().as_bytes(),
                match project {
                    PlatformProjectPolicy::DefaultProject => b"default".as_slice(),
                    PlatformProjectPolicy::Project(project) => project.as_str().as_bytes(),
                },
            ],
        ),
        #[cfg(feature = "native-auth")]
        RouteAuth::NativeCodexSubscription { authority } => Ok(authority.route_account_binding()),
    };
    result.unwrap_or_else(|error| unreachable!("binding digest version is valid: {error}"))
}

fn credential_authority_binding(auth: &RouteAuth) -> Digest {
    let result = match auth {
        RouteAuth::Platform { source, .. } => Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[
                b"openai-platform-credential-authority",
                source.credential_authority().as_str().as_bytes(),
            ],
        ),
        #[cfg(feature = "native-auth")]
        RouteAuth::NativeCodexSubscription { authority } => Ok(authority.authority_binding()),
    };
    result.unwrap_or_else(|error| unreachable!("binding digest version is valid: {error}"))
}

/// One immutable protocol, route, model, and harness-profile binding.
#[derive(Clone)]
pub struct OpenAiTarget {
    route: OpenAiRoute,
    descriptor: ResolvedModelDescriptor,
    routability: Routability,
    admission: TargetAdmission,
    clock: Arc<dyn Clock>,
}

#[derive(Clone, Debug)]
enum TargetAdmission {
    ReviewedPlatform(ReviewedPlatformManifestWitness),
    #[cfg(feature = "native-auth")]
    NativeCatalog(NativeCodexCatalogWitness),
    OfflineCodec,
}

#[cfg(all(test, feature = "native-auth"))]
mod tests {
    use super::{ConformanceStatus, OpenAiRouteConformance, ResponsesFeature};

    #[test]
    fn codex_compatibility_admits_native_compaction_options() {
        let conformance = OpenAiRouteConformance::codex_compatibility();
        assert_eq!(
            conformance.feature(ResponsesFeature::NativeOptions),
            ConformanceStatus::Supported
        );
        assert_eq!(
            conformance.feature(ResponsesFeature::Compaction),
            ConformanceStatus::Excluded
        );
        let platform = OpenAiRouteConformance::platform_o1();
        assert_eq!(
            platform.feature(ResponsesFeature::NativeOptions),
            ConformanceStatus::Excluded
        );
    }
}
