//! Protocol, route, authentication, and route-scoped target configuration.

use std::fmt;
#[cfg(feature = "native-auth")]
use std::future::Future;
#[cfg(feature = "native-auth")]
use std::pin::Pin;
use std::sync::Arc;
#[cfg(feature = "native-auth")]
use std::time::Instant;

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
pub const OPENAI_RESPONSES_DIALECT_REVISION: u32 = 6;

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
pub const OPENAI_PLATFORM_RESPONSES_PATH: &str = "/v1/responses";
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
pub const OPENAI_CODEX_MODELS_PATH: &str = "/backend-api/codex/models";
/// Exact reference-client release presented to the authenticated catalog.
///
/// The backend versions the returned model projection from this value. It is
/// therefore compatibility identity, not Plak's package version or a minimum
/// version floor.
pub const OPENAI_CODEX_REFERENCE_CLIENT_VERSION: &str = "0.160.1";
/// Official Codex revision defining the pinned catalog contract.
pub const OPENAI_CODEX_CATALOG_SOURCE_REVISION: &str = "a956835d020762cb2b570053af06f643a11c0ecc";
/// Immutable native compatibility profile accepted by this revision.
pub const OPENAI_CODEX_COMPATIBILITY_PROFILE_ID: &str = "openai.codex.compat.2026-07-31.r1";
/// Official Codex source revision pinned by the native compatibility profile.
pub const OPENAI_CODEX_SOURCE_REVISION: &str = "c4f2746c43a3ab87c2978657d7f2c274546e97fe";
/// Native Codex route conformance revision.
pub const OPENAI_CODEX_CONFORMANCE_REVISION: &str = "plak-openai-codex-compat-2026-07-31-r1";
/// Version of the conservative O1 Platform wire-conformance evidence.
pub const OPENAI_O1_CONFORMANCE_REVISION: &str = "plak-openai-o1-platform-wire-v1";

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
            | ResponsesFeature::HttpSse => ConformanceStatus::Supported,
            ResponsesFeature::HostedTools
            | ResponsesFeature::Approvals
            | ResponsesFeature::Compaction
            | ResponsesFeature::ImageInput
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

/// Serializable, secret-free `OpenAI` Platform route configuration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct OpenAiPlatformRouteConfig {
    origin: EndpointOrigin,
    credential: CredentialReference,
    storage: PlatformStoragePolicy,
    project: PlatformProjectPolicy,
    #[serde(skip)]
    local_fixture: bool,
}

impl OpenAiPlatformRouteConfig {
    /// Creates first-party Platform route configuration with an explicit credential reference.
    ///
    /// # Errors
    ///
    /// Returns an error unless `origin` is exactly the official `OpenAI` Platform origin.
    pub fn new(
        origin: EndpointOrigin,
        credential: CredentialReference,
        storage: PlatformStoragePolicy,
        project: PlatformProjectPolicy,
    ) -> Result<Self, ConfigError> {
        if origin.as_str() != OPENAI_PLATFORM_ORIGIN {
            return Err(ConfigError::NonOfficialPlatformOrigin);
        }
        Ok(Self {
            origin,
            credential,
            storage,
            project,
            local_fixture: false,
        })
    }

    /// Creates an HTTPS-loopback-only route for deterministic local transport evidence.
    #[doc(hidden)]
    pub fn new_local_fixture(
        origin: EndpointOrigin,
        credential: CredentialReference,
        storage: PlatformStoragePolicy,
        project: PlatformProjectPolicy,
    ) -> Result<Self, ConfigError> {
        if !local_fixture_origin(&origin) {
            return Err(ConfigError::NonLocalFixtureOrigin);
        }
        Ok(Self {
            origin,
            credential,
            storage,
            project,
            local_fixture: true,
        })
    }

    /// Resolves configuration with an application-owned credential source.
    #[must_use]
    pub fn resolve(self, source: Arc<dyn PlatformCredentialSource>) -> OpenAiRoute {
        OpenAiRoute::new(
            OPENAI_PLATFORM_ROUTE_ID,
            ResponsesDialect::platform(),
            self.origin,
            match self.storage {
                PlatformStoragePolicy::StoreResponses => StorageMode::ProviderStored,
                PlatformStoragePolicy::Stateless => StorageMode::Stateless,
            },
            OpenAiRouteConformance::platform_o1(),
            RouteAuth::Platform {
                reference: self.credential,
                source,
                project: self.project,
                local_fixture: self.local_fixture,
            },
        )
    }

    /// Returns the exact non-secret account binding produced by this
    /// credential reference on the official Platform route.
    ///
    /// This is intended for an offline paid-campaign approval. It does not
    /// inspect or resolve credential bytes.
    #[must_use]
    pub fn account_binding(&self) -> Digest {
        Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[
                b"openai-platform-credential-reference",
                self.origin.as_str().as_bytes(),
                self.credential.as_str().as_bytes(),
                match &self.project {
                    PlatformProjectPolicy::DefaultProject => b"default".as_slice(),
                    PlatformProjectPolicy::Project(project) => project.as_str().as_bytes(),
                },
            ],
        )
        .unwrap_or_else(|error| unreachable!("binding digest version is valid: {error}"))
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenAiPlatformRouteConfigWire {
    origin: EndpointOrigin,
    credential: CredentialReference,
    storage: PlatformStoragePolicy,
    project: PlatformProjectPolicy,
}

impl<'de> Deserialize<'de> for OpenAiPlatformRouteConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = OpenAiPlatformRouteConfigWire::deserialize(deserializer)?;
        Self::new(wire.origin, wire.credential, wire.storage, wire.project)
            .map_err(serde::de::Error::custom)
    }
}

/// Serializable, secret-free direct Codex subscription route configuration.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CodexSubscriptionRouteConfig {
    origin: EndpointOrigin,
}

impl CodexSubscriptionRouteConfig {
    /// Creates a disabled-by-default direct subscription route configuration.
    #[must_use]
    pub const fn new(origin: EndpointOrigin) -> Self {
        Self { origin }
    }

    /// Reports that configuration alone is not routable.
    #[must_use]
    pub const fn routability(&self) -> Routability {
        Routability::NonRoutable
    }

    /// Attempts to resolve without an injected OAuth source and fails closed.
    ///
    /// # Errors
    ///
    /// Always returns [`ConfigError::SubscriptionOAuthRequired`].
    pub fn resolve_disabled(&self) -> Result<OpenAiRoute, ConfigError> {
        Err(ConfigError::SubscriptionOAuthRequired)
    }

    /// Resolves an official-origin route with an account-bound Codex authority.
    ///
    /// # Errors
    ///
    /// Returns an error unless the configured origin is exactly the official
    /// `ChatGPT` origin. No credential source is called during resolution.
    /// Resolves an official-origin route directly from one explicit Codex store.
    ///
    /// The public facade exposes this constructor instead of the lower-level
    /// credential source, keeping raw current-access generations out of the
    /// facade API.
    ///
    /// # Errors
    ///
    /// Returns a redaction-safe store/source error or the same origin error as
    /// [`Self::resolve`].
    /// Resolves the immutable native compatibility route with a Plak-owned manager.
    ///
    /// # Errors
    ///
    /// Rejects every origin except the pinned `ChatGPT` origin and rejects a
    /// manager whose exact compatibility notice is no longer accepted/current.
    #[cfg(feature = "native-auth")]
    pub(crate) fn resolve_native(
        self,
        manager: Arc<CodexSubscriptionCredentialManager>,
    ) -> Result<OpenAiRoute, ConfigError> {
        if self.origin.as_str() != OPENAI_CHATGPT_ORIGIN {
            return Err(ConfigError::NonOfficialCodexSubscriptionOrigin);
        }
        manager
            .validate_route_resolution()
            .map_err(|_| ConfigError::SubscriptionCredentialUnavailable)?;
        let authority: Arc<dyn NativeCodexCredentialAuthority> = manager;
        Ok(OpenAiRoute::new(
            OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID,
            ResponsesDialect::codex_subscription(),
            self.origin,
            StorageMode::Stateless,
            OpenAiRouteConformance::codex_compatibility(),
            RouteAuth::NativeCodexSubscription { authority },
        ))
    }
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

    pub(crate) fn bind_platform_generation(
        mut self,
        grant: OpenAiApiKeyGenerationGrant,
    ) -> Result<Self, ConfigError> {
        let bind = |source: Digest| {
            Digest::sha256(
                DigestDomain::ModelBinding,
                2,
                &[
                    b"openai-platform-key-owned-binding",
                    source.bytes(),
                    grant.fingerprint().as_bytes(),
                ],
            )
            .map_err(|_| ConfigError::LocalFixtureContext)
        };
        self.account_binding = bind(self.account_binding)?;
        self.credential_authority_binding = bind(self.credential_authority_binding)?;
        Ok(self)
    }

    /// Returns the immutable route identity.
    #[must_use]
    pub const fn id(&self) -> &RouteId {
        &self.id
    }

    /// Returns the route's explicit protocol dialect.
    #[must_use]
    pub const fn dialect(&self) -> &ResponsesDialect {
        &self.dialect
    }

    /// Returns the exact origin to which credentials are bound.
    #[must_use]
    pub const fn origin(&self) -> &EndpointOrigin {
        &self.origin
    }

    /// Returns the effective storage mode.
    #[must_use]
    pub const fn storage(&self) -> StorageMode {
        self.storage
    }

    /// Returns the fixed transport resource path for this route.
    #[must_use]
    pub const fn responses_path(&self) -> &'static str {
        match &self.auth {
            RouteAuth::Platform { .. } => OPENAI_PLATFORM_RESPONSES_PATH,
            #[cfg(feature = "native-auth")]
            RouteAuth::NativeCodexSubscription { .. } => OPENAI_CODEX_RESPONSES_PATH,
        }
    }

    /// Returns the explicit Platform project-header disposition.
    #[must_use]
    pub const fn platform_project(&self) -> Option<&PlatformProjectPolicy> {
        match &self.auth {
            RouteAuth::Platform { project, .. } => Some(project),
            #[cfg(feature = "native-auth")]
            RouteAuth::NativeCodexSubscription { .. } => None,
        }
    }

    /// Returns the versioned route-specific conformance matrix.
    #[must_use]
    pub const fn conformance(&self) -> OpenAiRouteConformance {
        self.conformance
    }

    /// Reports whether this resolved route can dispatch.
    #[must_use]
    pub const fn routability(&self) -> Routability {
        Routability::Routable
    }

    /// Returns the route's credential/billing access mode.
    #[must_use]
    pub const fn access_mode(&self) -> AccessMode {
        match &self.auth {
            RouteAuth::Platform {
                local_fixture: true,
                ..
            } => AccessMode::LocalFixture,
            RouteAuth::Platform { .. } => AccessMode::PlatformApi,
            #[cfg(feature = "native-auth")]
            RouteAuth::NativeCodexSubscription { .. } => AccessMode::CodexSubscription,
        }
    }

    pub(crate) const fn is_local_platform_fixture(&self) -> bool {
        matches!(
            &self.auth,
            RouteAuth::Platform {
                local_fixture: true,
                ..
            }
        )
    }

    pub(crate) fn platform_generation_grant(
        &self,
    ) -> Result<OpenAiApiKeyGenerationGrant, AuthError> {
        match &self.auth {
            RouteAuth::Platform {
                reference, source, ..
            } => source.generation_grant(reference, &self.origin),
            #[cfg(feature = "native-auth")]
            RouteAuth::NativeCodexSubscription { .. } => Err(AuthError::BindingMismatch),
        }
    }

    pub(crate) fn revalidate_platform_generation(
        &self,
        expected: OpenAiApiKeyGenerationGrant,
    ) -> Result<(), AuthError> {
        (self.platform_generation_grant()? == expected)
            .then_some(())
            .ok_or(AuthError::BindingMismatch)
    }

    pub(crate) fn platform_bearer_header(
        &self,
        expected: OpenAiApiKeyGenerationGrant,
    ) -> Result<plakous_http::HeaderValue, AuthError> {
        let (reference, source, local_fixture) = match &self.auth {
            RouteAuth::Platform {
                reference,
                source,
                local_fixture,
                ..
            } => (reference, source, local_fixture),
            #[cfg(feature = "native-auth")]
            RouteAuth::NativeCodexSubscription { .. } => return Err(AuthError::BindingMismatch),
        };
        if *local_fixture {
            return Err(AuthError::BindingMismatch);
        }
        let key = source.api_key_for_generation(reference, &self.origin, expected)?;
        let mut bytes = b"Bearer ".to_vec();
        bytes.extend_from_slice(key.expose().as_bytes());
        plakous_http::sensitive_header_value(bytes).map_err(|_| AuthError::SourceFailure)
    }

    #[cfg(feature = "native-auth")]
    pub(crate) fn is_native_codex(&self) -> bool {
        matches!(&self.auth, RouteAuth::NativeCodexSubscription { .. })
    }

    /// Computes a non-secret account/manager binding for replay isolation.
    ///
    /// # Errors
    ///
    /// Returns an error only if the domain-separated digest cannot be created.
    pub fn account_binding(&self) -> Result<Digest, ConfigError> {
        Ok(self.account_binding)
    }

    /// Computes a non-secret binding for the exact credential authority.
    ///
    /// This is deliberately distinct from [`Self::account_binding`]: replacing
    /// an application resolver or OAuth manager cannot preserve an operation
    /// binding merely because it selects the same account label.
    ///
    /// # Errors
    ///
    /// Returns an error only if the domain-separated digest cannot be created.
    pub fn credential_authority_binding(&self) -> Result<Digest, ConfigError> {
        Ok(self.credential_authority_binding)
    }

    #[cfg(feature = "native-auth")]
    pub(crate) async fn native_codex_preflight(
        &self,
        request: NativeCodexPreflightRequest,
    ) -> Result<NativeCodexAuthorization, AuthError> {
        request.target().validate_route(self)?;
        match &self.auth {
            RouteAuth::NativeCodexSubscription { authority } => authority
                .preflight(request)
                .await
                .map_err(|_| AuthError::SourceFailure),
            RouteAuth::Platform { .. } => Err(AuthError::BindingMismatch),
        }
    }

    /// Forces one claimed refresh through the native Codex authority after an
    /// HTTP 401, so the single re-dispatch carries the renewed grant.
    #[cfg(feature = "native-auth")]
    pub(crate) async fn native_codex_refresh_after_rejection(
        &self,
        rejected: &CredentialFence,
        selection_epoch: Digest,
    ) -> Result<(), AuthError> {
        match &self.auth {
            RouteAuth::NativeCodexSubscription { authority } => {
                // The claimed exchange runs as its own task: a dispatch
                // cancelled (Esc) or past its deadline mid-exchange does not
                // abandon a started claim; it completes or fails within its
                // 30 s refresh deadline and records its outcome.
                let authority = Arc::clone(authority);
                let rejected = rejected.clone();
                tokio::spawn(async move {
                    authority
                        .refresh_after_rejection(&rejected, selection_epoch)
                        .await
                })
                .await
                .map_err(|_| AuthError::SourceFailure)?
            }
            RouteAuth::Platform { .. } => Err(AuthError::BindingMismatch),
        }
    }

    #[cfg(feature = "native-auth")]
    pub(crate) fn acquire_native_codex_inference(
        &self,
        input_token_upper_bound: u64,
    ) -> Result<Box<dyn SubscriptionRequestPermit>, AuthError> {
        match &self.auth {
            RouteAuth::NativeCodexSubscription { authority } => {
                authority.acquire_inference(input_token_upper_bound)
            }
            RouteAuth::Platform { .. } => Err(AuthError::BindingMismatch),
        }
    }

    #[cfg(feature = "native-auth")]
    pub(crate) fn bind_native_codex_authorization(
        &self,
        authorization: &mut NativeCodexAuthorization,
        physical_request: ResponsesPhysicalRequestBinding,
    ) -> Result<(), AuthError> {
        authorization.target().validate_route(self)?;
        match &self.auth {
            RouteAuth::NativeCodexSubscription { authority } => {
                authority.bind(authorization, physical_request)
            }
            RouteAuth::Platform { .. } => Err(AuthError::BindingMismatch),
        }
    }
}

impl fmt::Debug for OpenAiRoute {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiRoute")
            .field("id", &self.id)
            .field("dialect", &self.dialect)
            .field("origin", &self.origin)
            .field("storage", &self.storage)
            .field("conformance_revision", &self.conformance.revision())
            .field("account_binding", &"<redacted digest>")
            .field("credential_authority_binding", &"<redacted digest>")
            .field("auth", &"<redacted>")
            .finish()
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

impl fmt::Debug for OpenAiTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiTarget")
            .field("route", &self.route)
            .field("descriptor", &self.descriptor)
            .field("routability", &self.routability)
            .field("admission", &self.admission)
            .field("clock", &"<injected clock>")
            .finish()
    }
}

impl OpenAiTarget {
    /// A caller-created descriptor cannot be promoted into a routable target.
    ///
    /// Production Platform targets are issued only through
    /// [`crate::OpenAiPlatformModelBuilder`]. Offline codec targets remain
    /// available through [`Self::new_offline_codec`].
    ///
    /// ```compile_fail
    /// # use plakous_openai::{OpenAiRoute, OpenAiTarget};
    /// # use plakous_model::ResolvedModelDescriptor;
    /// # fn bypass(route: OpenAiRoute, descriptor: ResolvedModelDescriptor) {
    /// let _ = OpenAiTarget::new(route, descriptor);
    /// # }
    /// ```
    pub(crate) fn new_platform_manifest_target(
        route: OpenAiRoute,
        descriptor: ResolvedModelDescriptor,
        witness: ReviewedPlatformManifestWitness,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, ConfigError> {
        if !matches!(
            route.access_mode(),
            AccessMode::PlatformApi | AccessMode::LocalFixture
        ) {
            return Err(ConfigError::PlatformTargetRequiresManifestWitness);
        }
        if descriptor.protocol() != route.dialect().protocol() {
            return Err(ConfigError::ProtocolMismatch);
        }
        if descriptor.route() != route.id() {
            return Err(ConfigError::RouteMismatch);
        }
        let target = Self {
            route,
            descriptor,
            routability: Routability::Routable,
            admission: TargetAdmission::ReviewedPlatform(witness),
            clock,
        };
        target.validate_current_route_evidence()?;
        Ok(target)
    }

    #[cfg(feature = "native-auth")]
    pub(crate) fn new_native_catalog_target(
        route: OpenAiRoute,
        descriptor: ResolvedModelDescriptor,
        witness: NativeCodexCatalogWitness,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, ConfigError> {
        if !route.is_native_codex() {
            return Err(ConfigError::NativeTargetRequiresCatalogWitness);
        }
        if descriptor.protocol() != route.dialect().protocol() {
            return Err(ConfigError::ProtocolMismatch);
        }
        if descriptor.route() != route.id() {
            return Err(ConfigError::RouteMismatch);
        }
        validate_conformance_claims(route.conformance(), &descriptor, clock.now())?;
        match &route.auth {
            RouteAuth::NativeCodexSubscription { authority } => authority
                .validate_catalog_witness(&descriptor, &witness)
                .map_err(|_| ConfigError::NativeTargetRequiresCatalogWitness)?,
            RouteAuth::Platform { .. } => {
                return Err(ConfigError::NativeTargetRequiresCatalogWitness);
            }
        }
        Ok(Self {
            route,
            descriptor,
            routability: Routability::Routable,
            admission: TargetAdmission::NativeCatalog(witness),
            clock,
        })
    }

    /// Binds fixture-backed codec evidence without creating a routable target.
    ///
    /// This constructor exists only for deterministic request and stream
    /// conformance work. Every positive descriptor fact must be labelled
    /// [`FactSource::TestFixture`], and the resulting target remains explicitly
    /// [`Routability::NonRoutable`] even when its underlying route has credentials.
    ///
    /// # Errors
    ///
    /// Returns an error for a foreign protocol or route, a non-fixture positive
    /// claim, an excluded feature, or an unknown feature without codec evidence.
    pub fn new_offline_codec(
        route: OpenAiRoute,
        descriptor: ResolvedModelDescriptor,
    ) -> Result<Self, ConfigError> {
        Self::new_offline_codec_with_clock(route, descriptor, Arc::new(SystemClock))
    }

    /// Binds fixture-backed codec evidence to an explicit deterministic clock.
    ///
    /// The resulting target is always non-routable.
    ///
    /// # Errors
    ///
    /// Returns the same validation failures as [`Self::new_offline_codec`].
    pub fn new_offline_codec_with_clock(
        route: OpenAiRoute,
        descriptor: ResolvedModelDescriptor,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, ConfigError> {
        if descriptor.protocol() != route.dialect().protocol() {
            return Err(ConfigError::ProtocolMismatch);
        }
        if descriptor.route() != route.id() {
            return Err(ConfigError::RouteMismatch);
        }
        validate_offline_codec_claims(route.conformance(), &descriptor)?;
        Ok(Self {
            route,
            descriptor,
            routability: Routability::NonRoutable,
            admission: TargetAdmission::OfflineCodec,
            clock,
        })
    }

    /// Returns the resolved route.
    #[must_use]
    pub const fn route(&self) -> &OpenAiRoute {
        &self.route
    }

    #[cfg(feature = "native-auth")]
    pub(crate) fn validate_route(&self, route: &OpenAiRoute) -> Result<(), AuthError> {
        if self.route.id() != route.id()
            || self.route.origin() != route.origin()
            || self.route.responses_path() != route.responses_path()
            || self.route.dialect() != route.dialect()
            || self.route.storage() != route.storage()
            || self.route.account_binding != route.account_binding
            || self.route.credential_authority_binding != route.credential_authority_binding
        {
            return Err(AuthError::BindingMismatch);
        }
        Ok(())
    }

    #[cfg(feature = "native-auth")]
    pub(crate) const fn route_id(&self) -> &RouteId {
        self.route.id()
    }

    pub(crate) const fn account_binding(&self) -> Digest {
        self.route.account_binding
    }

    pub(crate) const fn credential_authority_binding(&self) -> Digest {
        self.route.credential_authority_binding
    }
    /// Returns the route-scoped model identity.
    #[must_use]
    pub const fn model(&self) -> &ModelId {
        self.descriptor.requested_model()
    }

    /// Returns the independently selected harness profile.
    #[must_use]
    pub const fn profile(&self) -> &ModelAdapterProfileId {
        self.descriptor.profile()
    }

    /// Reports whether this exact target may be used for live dispatch.
    #[must_use]
    pub const fn routability(&self) -> Routability {
        self.routability
    }

    /// Returns the complete resolved descriptor.
    #[must_use]
    pub const fn descriptor(&self) -> &ResolvedModelDescriptor {
        &self.descriptor
    }

    #[cfg(feature = "native-auth")]
    pub(crate) const fn codex_responses_lite(&self) -> bool {
        match &self.admission {
            TargetAdmission::NativeCatalog(witness) => witness.responses_lite(),
            _ => false,
        }
    }

    pub(crate) fn validate_current_route_evidence(&self) -> Result<(), ConfigError> {
        match &self.admission {
            TargetAdmission::ReviewedPlatform(witness) => {
                validate_reviewed_platform_witness(
                    &self.route,
                    &self.descriptor,
                    witness,
                    self.clock.now(),
                )?;
                validate_feature_conformance(self.route.conformance(), &self.descriptor)
            }
            #[cfg(feature = "native-auth")]
            TargetAdmission::NativeCatalog(witness) => {
                match &self.route.auth {
                    RouteAuth::NativeCodexSubscription { authority } => authority
                        .validate_catalog_witness(&self.descriptor, witness)
                        .map_err(|_| ConfigError::NativeTargetRequiresCatalogWitness)?,
                    RouteAuth::Platform { .. } => {
                        return Err(ConfigError::NativeTargetRequiresCatalogWitness);
                    }
                }
                validate_conformance_claims(
                    self.route.conformance(),
                    &self.descriptor,
                    self.clock.now(),
                )
            }
            TargetAdmission::OfflineCodec => Err(ConfigError::NonRoutableTarget),
        }
    }

    #[cfg(feature = "native-auth")]
    pub(crate) async fn revalidate_native_runtime_authority(
        &self,
        checked_at: Timestamp,
    ) -> Result<RuntimeModelAuthority, RuntimeModelAuthorityError> {
        let (
            RouteAuth::NativeCodexSubscription { authority },
            TargetAdmission::NativeCatalog(witness),
        ) = (&self.route.auth, &self.admission)
        else {
            return Err(RuntimeModelAuthorityError::Unavailable);
        };
        if !self.route.is_native_codex()
            || self.descriptor.protocol() != self.route.dialect().protocol()
            || self.descriptor.route() != self.route.id()
            || self.descriptor.profile().as_str() != OPENAI_CODEX_COMPATIBILITY_PROFILE_ID
        {
            return Err(RuntimeModelAuthorityError::Invalid);
        }
        authority
            .revalidate_runtime_authority(&self.descriptor, witness, checked_at)
            .await
    }

    pub(crate) fn validate_request_intent(
        &self,
        request: &ModelRequest,
    ) -> Result<(), ConfigError> {
        match &self.admission {
            TargetAdmission::ReviewedPlatform(witness) => {
                validate_reviewed_platform_request(&self.descriptor, witness, request)
            }
            #[cfg(feature = "native-auth")]
            TargetAdmission::NativeCatalog(_) => validate_unscoped_adapter_request(request),
            TargetAdmission::OfflineCodec => Err(ConfigError::NonRoutableTarget),
        }
    }

    pub(crate) fn clock(&self) -> Arc<dyn Clock> {
        Arc::clone(&self.clock)
    }

    /// The admitting catalog's observation and expiry.
    #[cfg(feature = "native-auth")]
    pub(crate) const fn native_codex_catalog_window(&self) -> Option<(Timestamp, Timestamp)> {
        match &self.admission {
            TargetAdmission::NativeCatalog(witness) => {
                Some((witness.observed_at(), witness.expires_at()))
            }
            _ => None,
        }
    }

    #[cfg(feature = "native-auth")]
    pub(crate) fn native_codex_credential_selection(&self) -> Option<(&CredentialFence, Digest)> {
        match &self.admission {
            TargetAdmission::NativeCatalog(witness) => Some((
                witness.credential_fence(),
                witness.credential_selection_epoch(),
            )),
            _ => None,
        }
    }

    /// Verifies that a plan was validated for this exact immutable binding.
    ///
    /// # Errors
    ///
    /// Returns an error for a foreign route/model/profile plan.
    pub fn validate_plan(&self, plan: &ValidatedRequestPlan) -> Result<(), ConfigError> {
        self.descriptor
            .validate_plan_binding(plan)
            .map_err(|_| ConfigError::ForeignPlan)
    }
}

#[cfg(feature = "native-auth")]
fn validate_conformance_claims(
    conformance: OpenAiRouteConformance,
    descriptor: &ResolvedModelDescriptor,
    now: Timestamp,
) -> Result<(), ConfigError> {
    let native_support = native_option_namespace_support(descriptor)?;
    for (capability, support) in descriptor.capabilities().features() {
        if support.support() == Support::Unsupported {
            continue;
        }
        if support.provenance().source() == FactSource::TestFixture {
            return Err(ConfigError::FixtureClaimRequiresOfflineTarget);
        }
        let pinned_codex_parallel = descriptor.profile().as_str()
            == OPENAI_CODEX_COMPATIBILITY_PROFILE_ID
            && *capability == Capability::ParallelTools
            && support.provenance().source() == FactSource::BuiltIn
            && support.provenance().expires_at().is_none();
        if support.provenance().source() != FactSource::RouteEvidence && !pinned_codex_parallel {
            return Err(ConfigError::RoutableCapabilityRequiresRouteEvidence);
        }
        if support.provenance().is_expired(now) {
            return Err(ConfigError::RoutableCapabilityEvidenceExpired);
        }
    }
    for support in &native_support {
        if support.support() == Support::Unsupported {
            continue;
        }
        if support.provenance().source() == FactSource::TestFixture {
            return Err(ConfigError::FixtureClaimRequiresOfflineTarget);
        }
        if support.provenance().source() != FactSource::RouteEvidence {
            return Err(ConfigError::RoutableCapabilityRequiresRouteEvidence);
        }
        if support.provenance().is_expired(now) {
            return Err(ConfigError::RoutableCapabilityEvidenceExpired);
        }
    }
    validate_feature_conformance(conformance, descriptor)
}

fn validate_feature_conformance(
    conformance: OpenAiRouteConformance,
    descriptor: &ResolvedModelDescriptor,
) -> Result<(), ConfigError> {
    let native_support = native_option_namespace_support(descriptor)?;
    descriptor
        .capabilities()
        .get(Capability::Streaming)
        .filter(|support| support.support() != Support::Unsupported)
        .ok_or(ConfigError::StreamingConformanceRequired)?;
    for (&capability, support) in descriptor.capabilities().features() {
        if support.support() == Support::Unsupported {
            continue;
        }
        let feature = capability_feature(capability)?;
        match conformance.feature(feature) {
            ConformanceStatus::Supported => {}
            ConformanceStatus::Excluded => return Err(ConfigError::ExcludedConformanceClaim),
            ConformanceStatus::Unknown => return Err(ConfigError::UnknownConformanceClaim),
        }
    }
    for support in native_support {
        if support.support() == Support::Unsupported {
            continue;
        }
        match conformance.feature(ResponsesFeature::NativeOptions) {
            ConformanceStatus::Supported => {}
            ConformanceStatus::Excluded => return Err(ConfigError::ExcludedConformanceClaim),
            ConformanceStatus::Unknown => return Err(ConfigError::UnknownConformanceClaim),
        }
    }
    Ok(())
}

fn validate_offline_codec_claims(
    conformance: OpenAiRouteConformance,
    descriptor: &ResolvedModelDescriptor,
) -> Result<(), ConfigError> {
    for (&capability, support) in descriptor.capabilities().features() {
        if support.support() == Support::Unsupported {
            continue;
        }
        if support.provenance().source() != FactSource::TestFixture {
            return Err(ConfigError::NonFixtureOfflineClaim);
        }
        let feature = capability_feature(capability)?;
        match conformance.feature(feature) {
            ConformanceStatus::Supported => {}
            ConformanceStatus::Excluded => return Err(ConfigError::ExcludedConformanceClaim),
            ConformanceStatus::Unknown if conformance.has_fixture_codec_evidence(feature) => {}
            ConformanceStatus::Unknown => return Err(ConfigError::UnknownConformanceClaim),
        }
    }
    for support in native_option_namespace_support(descriptor)? {
        if support.support() == Support::Unsupported {
            continue;
        }
        if support.provenance().source() != FactSource::TestFixture {
            return Err(ConfigError::NonFixtureOfflineClaim);
        }
        match conformance.feature(ResponsesFeature::NativeOptions) {
            ConformanceStatus::Supported => {}
            ConformanceStatus::Excluded => return Err(ConfigError::ExcludedConformanceClaim),
            ConformanceStatus::Unknown
                if conformance.has_fixture_codec_evidence(ResponsesFeature::NativeOptions) => {}
            ConformanceStatus::Unknown => return Err(ConfigError::UnknownConformanceClaim),
        }
    }
    Ok(())
}

fn native_option_namespace_support(
    descriptor: &ResolvedModelDescriptor,
) -> Result<Vec<plakous_model::FeatureSupport>, ConfigError> {
    let capabilities =
        serde_json::to_value(descriptor.capabilities()).map_err(|_| ConfigError::BindingDigest)?;
    let namespaces = capabilities
        .get("native_option_namespaces")
        .and_then(serde_json::Value::as_object)
        .ok_or(ConfigError::BindingDigest)?;
    namespaces
        .values()
        .cloned()
        .map(|support| serde_json::from_value(support).map_err(|_| ConfigError::BindingDigest))
        .collect()
}

fn capability_feature(capability: Capability) -> Result<ResponsesFeature, ConfigError> {
    match capability {
        Capability::Streaming => Ok(ResponsesFeature::HttpSse),
        Capability::Reasoning => Ok(ResponsesFeature::ReasoningOutput),
        Capability::Tools => Ok(ResponsesFeature::ClientFunctionCalls),
        Capability::ParallelTools => Ok(ResponsesFeature::ParallelFunctionCalls),
        Capability::StructuredOutput => Ok(ResponsesFeature::StructuredOutput),
        Capability::Continuation => Ok(ResponsesFeature::PreviousResponseId),
        Capability::Caching => Ok(ResponsesFeature::PromptCaching),
        Capability::NativeOptions => Ok(ResponsesFeature::NativeOptions),
        Capability::HostedProgrammaticToolCalling => Ok(ResponsesFeature::ProgrammaticCallers),
        _ => Err(ConfigError::UnknownConformanceClaim),
    }
}

fn local_fixture_origin(origin: &EndpointOrigin) -> bool {
    let Ok(url) = Url::parse(origin.as_str()) else {
        return false;
    };
    url.scheme() == "https"
        && url.port().is_some_and(|port| port != 0)
        && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1"))
}

/// A route or binding configuration failure.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum ConfigError {
    /// A required configuration value was empty.
    #[error("{0} cannot be empty")]
    EmptyField(&'static str),
    /// An endpoint was not a credential-safe HTTPS origin.
    #[error("endpoint must be an HTTPS origin without credentials, path, query, or fragment")]
    InvalidOrigin,
    /// The first-party Platform route was assigned a non-OpenAI origin.
    #[error("first-party OpenAI Platform route requires the official API origin")]
    NonOfficialPlatformOrigin,
    /// The direct subscription route was assigned a non-ChatGPT origin.
    #[error("direct Codex subscription route requires the official ChatGPT origin")]
    NonOfficialCodexSubscriptionOrigin,
    /// A credential reference used an unsupported or unsafe shape.
    #[error("credential reference must be a bounded application identifier")]
    InvalidCredentialReference,
    /// A credential authority used an unsupported or unsafe shape.
    #[error("credential authority must be a bounded application identifier")]
    InvalidCredentialAuthority,
    /// A public API-key generation binding was empty or too large.
    #[error("credential generation binding must be nonempty and bounded")]
    InvalidCredentialGenerationBinding,
    /// An explicit project identifier was empty, oversized, or non-ASCII.
    #[error("OpenAI project identifier must be nonempty, bounded, ASCII text")]
    InvalidProjectId,
    /// A local execution fixture was not exact HTTPS loopback with an explicit port.
    #[error("OpenAI local fixture origin must be HTTPS loopback with an explicit port")]
    NonLocalFixtureOrigin,
    /// Fixture-only capacity or measurement authority could not be sealed.
    #[error("OpenAI local fixture context authority is invalid")]
    LocalFixtureContext,
    /// The direct subscription route lacks explicit Plakous-authorized OAuth.
    #[error(
        "direct Codex subscription route requires an explicitly injected Plakous-authorized OAuth source"
    )]
    SubscriptionOAuthRequired,
    /// The selected owner store could not produce a current source binding.
    #[error("direct Codex subscription credential source is unavailable")]
    SubscriptionCredentialUnavailable,
    /// The model descriptor names a different protocol dialect.
    #[error("model descriptor protocol does not match route dialect")]
    ProtocolMismatch,
    /// The model descriptor names a different immutable route.
    #[error("model descriptor route does not match selected route")]
    RouteMismatch,
    /// The request plan belongs to another binding.
    #[error("request plan belongs to another protocol/route/model/profile binding")]
    ForeignPlan,
    /// A route-account binding digest could not be created.
    #[error("route account binding digest failed")]
    BindingDigest,
    /// A descriptor claimed a feature explicitly excluded by route evidence.
    #[error("model descriptor claims a feature excluded by route conformance")]
    ExcludedConformanceClaim,
    /// A descriptor claimed a feature with no positive route evidence.
    #[error("model descriptor claims a feature unknown to route conformance")]
    UnknownConformanceClaim,
    /// An offline codec target contained a positive fact not backed by a fixture.
    #[error("offline codec targets require test-fixture provenance for every positive feature")]
    NonFixtureOfflineClaim,
    /// A fixture-backed capability was passed to the ordinary routable constructor.
    #[error("test-fixture capability claims require an explicit non-routable offline codec target")]
    FixtureClaimRequiresOfflineTarget,
    /// A routable positive capability lacked provider-route evidence.
    #[error("routable OpenAI capabilities require route-specific evidence")]
    RoutableCapabilityRequiresRouteEvidence,
    /// A routable positive capability's route evidence had expired.
    #[error("routable OpenAI capability evidence has expired")]
    RoutableCapabilityEvidenceExpired,
    /// A routable target omitted positive streaming support required by the codec.
    #[error("routable OpenAI targets require positive HTTP/SSE streaming conformance")]
    StreamingConformanceRequired,
    /// A native subscription target was not issued by its private catalog resolver.
    #[error("native Codex targets require an exact resolver-issued catalog witness")]
    NativeTargetRequiresCatalogWitness,
    /// A production Platform target was not issued by the exact reviewed manifest.
    #[error("OpenAI Platform targets require an exact reviewed-manifest witness")]
    PlatformTargetRequiresManifestWitness,
    /// No exact reviewed model/response/family/profile row matched the request.
    #[error("OpenAI Platform model tuple is not present in the reviewed manifest")]
    UnknownPlatformManifestTuple,
    /// The fixed reviewed-manifest compatibility window is not current.
    #[error("OpenAI Platform reviewed-model evidence is outside its compatibility window")]
    PlatformManifestEvidenceExpired,
    /// A retained reviewed-manifest witness no longer matches its descriptor.
    #[error("OpenAI Platform reviewed-model witness does not match the descriptor")]
    PlatformManifestBinding,
    /// A caller requested more output than the exact reviewed model row allows.
    #[error("max output tokens exceed the exact reviewed model limit")]
    MaximumOutputTokensExceeded,
    /// The adapter cannot faithfully lower the requested tool definition form.
    #[error("requested tool form is not supported by the admitted OpenAI adapter subset")]
    UnsupportedToolForm,
    /// The requested tool choice does not name an exact advertised function tool.
    #[error("tool choice is incompatible with the exact advertised function tools")]
    InvalidToolChoice,
    /// Portable reasoning token budgets cannot be lowered exactly.
    #[error("reasoning token budgets are not supported by the OpenAI adapter")]
    UnsupportedReasoningBudget,
    /// The neutral reasoning effort is not in the admitted portable subset.
    #[error("reasoning effort is not supported by the admitted OpenAI adapter subset")]
    UnsupportedReasoningEffort,
    /// Offline codec evidence cannot be used for live route validation.
    #[error("offline OpenAI codec targets are non-routable")]
    NonRoutableTarget,
}

/// A redaction-safe credential resolution error.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum AuthError {
    /// No current credential exists for the configured source.
    #[error("configured credential is unavailable")]
    Unavailable,
    /// The credential is not authorized for the route origin or audience.
    #[error("credential is not authorized for the configured route binding")]
    BindingMismatch,
    /// Credential resolution or exposure did not belong to an active physical request.
    #[error("subscription authorization requires an active physical request")]
    NoActivePhysicalRequest,
    /// This physical request already spent its single credential-resolution allowance.
    #[error("subscription authorization was already resolved for this physical request")]
    AuthorizationAlreadyResolved,
    /// Refresh or source access failed without exposing secret details.
    #[error("credential source failed")]
    SourceFailure,
    /// The turn-scoped inference input bound rejected this request body.
    #[error("subscription inference input bound was exceeded")]
    InferenceInputLimit,
    /// Inference class, thread scope, or classified allowance was unavailable.
    #[error("subscription inference reserved {reserved} of allowance {allowance}")]
    InferenceClassLimit {
        /// Reservations already charged, including abandoned permits.
        reserved: u32,
        /// Immutable inference-class allowance.
        allowance: u32,
    },
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
