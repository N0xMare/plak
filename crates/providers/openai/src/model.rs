//! Thin provider-neutral model construction for the OpenAI Responses dialect.

use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

#[cfg(all(feature = "native-auth", test))]
use std::num::NonZeroU32;

#[cfg(feature = "native-auth")]
use plakous_attempt::LedgerPolicy;
use plakous_model::RuntimeModelAuthorityError as PlatformRuntimeAuthorityError;
#[cfg(feature = "native-auth")]
use plakous_model::{
    Capability, CountCertificateKind, CountObservation, Digest, LanguageModel,
    ModelPreparationContext, ModelPreparationError, ModelPreparationErrorKind, ModelRequest,
    PreparedCacheUse, PreparedCallRecord, PreparedModelCall, PreparedModelProjection,
    PreparedOutputLimit, PreparedTokenProjection, ProtocolId, ReplayKindId, RuntimeModelAuthority,
    RuntimeModelAuthorityError, RuntimeModelAuthorityOperation, Support, ToolPresentationMode,
    ValidatedRequestPlan,
};
#[cfg(all(feature = "native-auth", test))]
use plakous_model::{
    CapacityCertificate, ContextContract, ContextEvidenceClass, ContextEvidenceSource,
    DigestDomain, EvidenceFreshness, MeasurementCertificate, SafeLimit,
};
use plakous_model::{
    CapacityRule, Clock, CompiledProviderConfiguration, ModelAdapterProfileId, ModelFamilyId,
    ModelId, ReasoningEffortDisposition, ResolvedModelDescriptor, ResolvedProviderAuthorityBinding,
    SelectedProviderConfiguration, SystemClock, Timestamp,
};
use plakous_responses::ResponsesStreamLimits;
#[cfg(feature = "native-auth")]
use plakous_responses::{ResponsesSemanticRevisions, ResponsesStorageMode};
#[cfg(feature = "native-auth")]
use plakous_responses_dialect::{
    PreparedResponsesModelExecution, ResponsesArtifactReplayBridge, ResponsesAttemptDialectData,
    ResponsesAttemptService, ResponsesAttemptTarget, ResponsesAuthSlots,
    ResponsesAuthorizationAdapter, ResponsesCanonicalDialectData, ResponsesProviderErrorPolicy,
    ResponsesReplayKind, ResponsesReplayProjection, ResponsesRequestAuthority,
    ResponsesResponseLimits, ResponsesStreamDialectData, canonical_local_tools,
    is_native_compaction_request, prepare_responses_attempt, start_prepared_responses_model_call,
};
use plakous_responses_dialect::{
    ResponsesProviderConfigurationError, ResponsesRequestPolicy, ResponsesStoragePolicy,
    compile_provider_configuration,
};
use thiserror::Error;

use crate::OPENAI_RESPONSES_ADAPTER_REVISION;
#[cfg(feature = "native-auth")]
use crate::config::OPENAI_RESPONSES_DIALECT_REVISION;
use crate::manifest::admit_reviewed_platform_model;
#[cfg(feature = "native-auth")]
use crate::native_auth::{
    CodexCatalogToolMode, CodexDispatchGuard, CodexModelResolutionEvidence, CodexReceiptRenewal,
    CodexRenewalFloor, CodexResponsesDialect, codex_credential_rejected, codex_responses_preflight,
    revalidate_codex_responses,
};
use crate::{
    ConfigError, CredentialReference, EndpointOrigin, OPENAI_PLATFORM_ORIGIN,
    OpenAiPlatformRouteConfig, OpenAiTarget, PlatformCredentialSource, PlatformStoragePolicy,
    manifest::OPENAI_GPT_6_PLATFORM_PROFILE_ID, manifest::OPENAI_GPT_56_PLATFORM_PROFILE_ID,
};

const DEFAULT_OPENAI_OPERATION_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_OPENAI_EVENT_CHANNEL_CAPACITY: usize = 64;
#[cfg(feature = "native-auth")]
const CODEX_RESPONSES_PREFLIGHT_REVISION: u32 = 1;
#[cfg(feature = "native-auth")]
const CODEX_RESPONSES_FINAL_AUTHORIZATION_REVISION: u32 = 1;
#[cfg(feature = "native-auth")]
const RESPONSES_FAMILY_ID: &str = "responses-family";
#[cfg(feature = "native-auth")]
const RESPONSES_CANONICAL_PROJECTION_REVISION: &str = "responses-canonical-projection-v4";
#[cfg(feature = "native-auth")]
const RESPONSES_EXACT_WIRE_ACCOUNTING_REVISION: &str = "responses-exact-wire-accounting-v3";
#[cfg(feature = "native-auth")]
/// Pinned Codex CLI estimates ordinary JSON items as `ceil(utf8_bytes / 4)`.
const CODEX_COMPLETE_INPUT_BYTES_PER_TOKEN: u64 = 4;
#[cfg(all(feature = "native-auth", test))]
const TEST_MEASUREMENT_CERTIFICATE_ID: &str = "openai-test-complete-wire-byte-bound";
#[cfg(all(feature = "native-auth", test))]
const TEST_MEASUREMENT_CERTIFICATE_REVISION: &str = "fixture-v1";
#[cfg(all(feature = "native-auth", test))]
const TEST_OUTPUT_HARD_CAP: u32 = 64;

/// Installs exact synthetic context evidence on one test-only resolved descriptor.
#[cfg(all(feature = "native-auth", test))]
pub(crate) fn with_test_context_contract(
    descriptor: ResolvedModelDescriptor,
    observed_at: Timestamp,
) -> Result<ResolvedModelDescriptor, Box<dyn std::error::Error + Send + Sync>> {
    let expires_at = Timestamp::new(observed_at.seconds().saturating_add(3_600), 0)?;
    let freshness = EvidenceFreshness::new(observed_at, Some(expires_at))?;
    let corpus = Digest::sha256(
        DigestDomain::Blob,
        1,
        &[b"openai-test-complete-wire-byte-bound-corpus-v1"],
    )?;
    let capacity = CapacityCertificate::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        "openai-test-capacity",
        "fixture-v1",
        ContextEvidenceSource::ReleaseCertificate,
        CapacityRule::SharedWindowWithOutputCap {
            total: SafeLimit::Exact(10_000_000),
            output: SafeLimit::Exact(u64::from(TEST_OUTPUT_HARD_CAP)),
        },
        None,
        freshness,
    )?;
    let measurement = MeasurementCertificate::standard(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        TEST_MEASUREMENT_CERTIFICATE_ID,
        TEST_MEASUREMENT_CERTIFICATE_REVISION,
        "complete-wire-byte-upper-bound",
        "fixture-v1",
        ContextEvidenceSource::ReleaseCertificate,
        CountCertificateKind::ReleaseVerifiedUpperBound { corpus },
        None,
        freshness,
    )?;
    Ok(descriptor.with_context_contract(ContextContract::new(
        ProtocolId::new(RESPONSES_FAMILY_ID)?,
        RESPONSES_EXACT_WIRE_ACCOUNTING_REVISION,
        RESPONSES_CANONICAL_PROJECTION_REVISION,
        "openai-test-output-reservation-v1",
        capacity,
        vec![measurement],
    )?))
}

/// Immutable logical-operation and shared stream bounds for Codex Responses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StandardOpenAiHttpConfig {
    operation_timeout: Duration,
    event_channel_capacity: usize,
    responses_limits: ResponsesStreamLimits,
}

impl Default for StandardOpenAiHttpConfig {
    fn default() -> Self {
        Self {
            operation_timeout: DEFAULT_OPENAI_OPERATION_TIMEOUT,
            event_channel_capacity: DEFAULT_OPENAI_EVENT_CHANNEL_CAPACITY,
            responses_limits: ResponsesStreamLimits::default(),
        }
    }
}

impl StandardOpenAiHttpConfig {
    /// Creates explicit nonzero operation, delivery, and family decoder bounds.
    pub fn new(
        operation_timeout: Duration,
        event_channel_capacity: usize,
        responses_limits: ResponsesStreamLimits,
    ) -> Result<Self, StandardOpenAiHttpBuildError> {
        if operation_timeout.is_zero() || event_channel_capacity == 0 {
            return Err(StandardOpenAiHttpBuildError::InvalidPolicy);
        }
        Ok(Self {
            operation_timeout,
            event_channel_capacity,
            responses_limits,
        })
    }

    /// Returns the dispatch timeout: it bounds preflight, retry waits and each
    /// exchange up to its response head, never a streaming body.
    #[must_use]
    pub const fn operation_timeout(self) -> Duration {
        self.operation_timeout
    }

    /// Returns the bounded live family-event channel capacity.
    #[must_use]
    pub const fn event_channel_capacity(self) -> usize {
        self.event_channel_capacity
    }

    /// Returns the shared family stream bounds.
    #[must_use]
    pub const fn responses_limits(self) -> ResponsesStreamLimits {
        self.responses_limits
    }
}

/// Failure to construct a strict shared-host OpenAI model.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum StandardOpenAiHttpBuildError {
    /// A duration or channel bound was zero.
    #[error("OpenAI shared-host policy bounds must be nonzero")]
    InvalidPolicy,
    /// The selected target is not an admitted Codex subscription target.
    #[error("OpenAI shared-host execution requires an admitted Codex target")]
    UnsupportedRoute,
    /// A shared Responses construction fact was invalid.
    #[error("OpenAI shared Responses construction failed")]
    SharedBinding,
}

/// Closed literal model selection admitted by the reviewed Platform table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenAiPlatformModelFacts {
    /// `gpt-6-sol`.
    Gpt6Sol,
    /// `gpt-6-luna`.
    Gpt6Luna,
    /// `gpt-5.6-sol`.
    Gpt56Sol,
    /// `gpt-5.6-terra`.
    Gpt56Terra,
    /// `gpt-5.6-luna`.
    Gpt56Luna,
}

impl OpenAiPlatformModelFacts {
    /// Every model the reviewed Platform table admits, in listing order.
    pub const ALL: [Self; 5] = [
        Self::Gpt6Luna,
        Self::Gpt6Sol,
        Self::Gpt56Luna,
        Self::Gpt56Sol,
        Self::Gpt56Terra,
    ];

    /// The row for one exact literal model id.
    #[must_use]
    pub fn from_model_id(model: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|facts| facts.model_id() == model)
    }

    /// Input window in tokens. `GET /v1/models` reports none; this is the
    /// `context_window` the authenticated Codex catalog reports for the same
    /// model literals (observed 2026-09-23), the vendor's own value for them.
    #[must_use]
    pub const fn context_window(self) -> u64 {
        272_000
    }

    /// Maximum output tokens from the reviewed Platform row.
    #[must_use]
    pub const fn max_output_tokens(self) -> u32 {
        crate::manifest::OPENAI_PLATFORM_MAX_OUTPUT_TOKENS
    }

    /// The reviewed Platform harness profile this row runs under.
    #[must_use]
    pub const fn profile_id(self) -> &'static str {
        match self {
            Self::Gpt6Sol | Self::Gpt6Luna => OPENAI_GPT_6_PLATFORM_PROFILE_ID,
            Self::Gpt56Sol | Self::Gpt56Terra | Self::Gpt56Luna => {
                OPENAI_GPT_56_PLATFORM_PROFILE_ID
            }
        }
    }

    /// Returns the requested model.
    #[must_use]
    pub const fn model_id(self) -> &'static str {
        match self {
            Self::Gpt6Sol => "gpt-6-sol",
            Self::Gpt6Luna => "gpt-6-luna",
            Self::Gpt56Sol => "gpt-5.6-sol",
            Self::Gpt56Terra => "gpt-5.6-terra",
            Self::Gpt56Luna => "gpt-5.6-luna",
        }
    }

    fn identity(self) -> (ModelId, ModelFamilyId, ModelAdapterProfileId) {
        let model = ModelId::new(self.model_id())
            .unwrap_or_else(|error| unreachable!("static model ID is valid: {error}"));
        let family_id = match self {
            Self::Gpt6Sol | Self::Gpt6Luna => "gpt-6",
            Self::Gpt56Sol | Self::Gpt56Terra | Self::Gpt56Luna => "gpt-5.6",
        };
        let profile_id = self.profile_id();
        let family = ModelFamilyId::new(family_id)
            .unwrap_or_else(|error| unreachable!("static family ID is valid: {error}"));
        let profile = ModelAdapterProfileId::new(profile_id)
            .unwrap_or_else(|error| unreachable!("static profile ID is valid: {error}"));
        (model, family, profile)
    }
}

/// Failure to construct or bind exact Platform execution.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum OpenAiPlatformResolveError {
    /// Retained route facts were invalid.
    #[error("OpenAI Platform route facts are invalid")]
    Configuration(#[from] ConfigError),
    /// Fresh runtime authority could not be validated.
    #[error("OpenAI Platform runtime authority is invalid")]
    Runtime(#[from] PlatformRuntimeAuthorityError),
    /// Configured execution outputs disagree.
    #[error("OpenAI Platform configured binding is invalid")]
    Binding(#[from] plakous_model::ProviderConfigurationError),
    /// Complete execution construction failed.
    #[error("OpenAI Platform execution construction failed")]
    Execution(#[from] crate::OpenAiPlatformBuildError),
}

/// Builder for exact Platform model, credential and context authority.
#[derive(Clone)]
pub struct OpenAiPlatformModelBuilder {
    credential: CredentialReference,
    source: Arc<dyn PlatformCredentialSource>,
    facts: OpenAiPlatformModelFacts,
    context_envelope: Option<std::num::NonZeroU64>,
    storage: PlatformStoragePolicy,
    project: crate::PlatformProjectPolicy,
    http: StandardOpenAiHttpConfig,
    clock: Arc<dyn Clock>,
    /// Opaque bytes the product admitted this session under. Appended to
    /// semantic evidence; empty leaves the previous identity unchanged.
    admitted_facts: Vec<u8>,
}

impl OpenAiPlatformModelBuilder {
    /// Captures exact official-Platform facts before constructing an executor.
    #[must_use]
    pub fn new(
        credential: CredentialReference,
        source: Arc<dyn PlatformCredentialSource>,
        facts: OpenAiPlatformModelFacts,
        storage: PlatformStoragePolicy,
        project: crate::PlatformProjectPolicy,
    ) -> Self {
        Self {
            credential,
            source,
            facts,
            context_envelope: None,
            storage,
            project,
            http: StandardOpenAiHttpConfig::default(),
            clock: Arc::new(SystemClock),
            admitted_facts: Vec::new(),
        }
    }

    /// Records the product's admitted-fact binding. The bytes are opaque here.
    #[must_use]
    pub fn admitted_facts(mut self, facts: Vec<u8>) -> Self {
        self.admitted_facts = facts;
        self
    }

    /// Declares a custom complete-wire input envelope; this is not a provider capacity guarantee.
    #[must_use]
    pub const fn context_envelope(mut self, maximum: std::num::NonZeroU64) -> Self {
        self.context_envelope = Some(maximum);
        self
    }

    /// Replaces retained transport policy facts.
    #[must_use]
    pub const fn http_config(mut self, http: StandardOpenAiHttpConfig) -> Self {
        self.http = http;
        self
    }

    /// Replaces the retained resolution clock.
    #[must_use]
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Seals immutable Platform configuration authority before execution.
    ///
    /// The result retains exact route, model, project, storage, and HTTP
    /// bounds for configuration compilation. It has no transport client,
    /// credential-exposure operation, or `LanguageModel` implementation.
    pub fn seal(self) -> Result<OpenAiPlatformConfigurationAuthority, ConfigError> {
        let Self {
            credential,
            source,
            facts,
            context_envelope,
            storage,
            project,
            http,
            clock,
            admitted_facts,
        } = self;
        let origin = EndpointOrigin::parse(OPENAI_PLATFORM_ORIGIN)?;
        let route =
            OpenAiPlatformRouteConfig::new(origin, credential, storage, project)?.resolve(source);
        let (model, family, profile) = facts.identity();
        let admission = admit_reviewed_platform_model(
            &route,
            &model,
            &model,
            Some(&family),
            &profile,
            clock.now(),
        )?;
        let admission = if let Some(maximum) = context_envelope {
            crate::manifest::with_context_envelope(admission, &route, maximum)?
        } else {
            admission
        };
        let target = OpenAiTarget::new_platform_manifest_target(
            route,
            admission.descriptor,
            admission.witness,
            clock,
        )?;
        Ok(OpenAiPlatformConfigurationAuthority {
            target,
            http,
            admitted_facts,
        })
    }

    /// Seals explicit fixture authority restricted to exact HTTPS loopback TLS.
    #[doc(hidden)]
    pub fn seal_local_fixture(
        self,
        origin: EndpointOrigin,
    ) -> Result<OpenAiPlatformConfigurationAuthority, ConfigError> {
        let Self {
            credential,
            source,
            facts,
            context_envelope,
            storage,
            project,
            http,
            clock,
            admitted_facts,
        } = self;
        let route =
            OpenAiPlatformRouteConfig::new_local_fixture(origin, credential, storage, project)?
                .resolve(source);
        let (model, family, profile) = facts.identity();
        let admission = admit_reviewed_platform_model(
            &route,
            &model,
            &model,
            Some(&family),
            &profile,
            clock.now(),
        )?;
        let admission = if let Some(maximum) = context_envelope {
            crate::manifest::with_context_envelope(admission, &route, maximum)?
        } else {
            admission
        };
        let target = OpenAiTarget::new_platform_manifest_target(
            route,
            admission.descriptor,
            admission.witness,
            clock,
        )?;
        Ok(OpenAiPlatformConfigurationAuthority {
            target,
            http,
            admitted_facts,
        })
    }

    /// Constructs exact production Platform execution with the selected behavior.
    pub fn resolve(
        self,
        selected: SelectedProviderConfiguration,
    ) -> Result<Arc<crate::OpenAiPlatformModel>, OpenAiPlatformResolveError> {
        Ok(Arc::new(crate::OpenAiPlatformModel::new(
            self.seal()?,
            selected,
        )?))
    }
}

impl std::fmt::Debug for OpenAiPlatformModelBuilder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenAiPlatformModelBuilder")
            .field("credential", &"<redacted reference>")
            .field("source", &"<redacted credential authority>")
            .field("facts", &self.facts)
            .field("context_envelope", &self.context_envelope)
            .field("storage", &self.storage)
            .field("project", &self.project)
            .field("http", &self.http)
            .field("clock", &"<injected>")
            .field("admitted_facts", &self.admitted_facts)
            .finish()
    }
}

/// Sealed exact authority for Platform configuration and execution.
#[derive(Clone)]
pub struct OpenAiPlatformConfigurationAuthority {
    target: OpenAiTarget,
    http: StandardOpenAiHttpConfig,
    admitted_facts: Vec<u8>,
}

impl OpenAiPlatformConfigurationAuthority {
    pub(crate) fn bind_initial_generation(
        self,
        grant: crate::OpenAiApiKeyGenerationGrant,
    ) -> Result<Self, ConfigError> {
        let descriptor = self.descriptor();
        let clock = self.target.clock();
        let route = self
            .target
            .route()
            .clone()
            .bind_platform_generation(grant)?;
        let mut admission = admit_reviewed_platform_model(
            &route,
            descriptor.requested_model(),
            descriptor
                .reported_model()
                .ok_or(ConfigError::LocalFixtureContext)?,
            descriptor.model_family(),
            descriptor.profile(),
            clock.now(),
        )?;
        if let Some(context) = descriptor.context_contract()
            && let CapacityRule::ProviderReservedInput {
                maximum_projected_input,
                ..
            } = context.capacity().rule()
            && let Some(maximum) = std::num::NonZeroU64::new(maximum_projected_input)
        {
            admission = crate::manifest::with_context_envelope(admission, &route, maximum)?;
        }
        Ok(Self {
            target: OpenAiTarget::new_platform_manifest_target(
                route,
                admission.descriptor,
                admission.witness,
                clock,
            )?,
            http: self.http,
            admitted_facts: self.admitted_facts,
        })
    }

    pub(crate) fn admitted_facts(&self) -> &[u8] {
        &self.admitted_facts
    }

    /// Returns the exact reviewed descriptor without granting dispatch.
    #[must_use]
    pub const fn descriptor(&self) -> &ResolvedModelDescriptor {
        self.target.descriptor()
    }

    /// Selects complete behavior on this exact provider-owned route and model tuple.
    pub fn selected_configuration(
        &self,
        behavior: plakous_model::ProviderBehaviorConfiguration,
    ) -> Result<SelectedProviderConfiguration, plakous_model::ProviderConfigurationError> {
        use plakous_model::{
            CredentialSelectionId, ProtocolId, ProviderAuthorityClassId, ProviderProfileRevisionId,
            ProviderRouteSelection, RouteId,
        };
        let identity = || -> Result<ProviderRouteSelection, plakous_model::IdentityError> {
            Ok(ProviderRouteSelection::new(
                ProtocolId::new(crate::OPENAI_RESPONSES_PROTOCOL_ID)?,
                RouteId::new(crate::OPENAI_PLATFORM_ROUTE_ID)?,
                self.descriptor().profile().clone(),
                ProviderProfileRevisionId::new(OPENAI_RESPONSES_ADAPTER_REVISION)?,
                ModelAdapterProfileId::new(crate::OPENAI_PLATFORM_AUTH_PROFILE_ID)?,
                ProviderProfileRevisionId::new(crate::OPENAI_PLATFORM_AUTH_REVISION)?,
                ProviderAuthorityClassId::new(crate::OPENAI_PLATFORM_AUTHORITY_CLASS)?,
                CredentialSelectionId::new(crate::OPENAI_PLATFORM_CREDENTIAL_SELECTION)?,
                self.descriptor().requested_model().clone(),
            ))
        };
        SelectedProviderConfiguration::new(
            identity().map_err(|_| plakous_model::ProviderConfigurationError::Identity)?,
            behavior,
        )
    }

    /// Returns the exact local operation/decoder bounds.
    #[must_use]
    pub const fn http_config(&self) -> StandardOpenAiHttpConfig {
        self.http
    }

    /// Returns the explicit Platform project-header disposition.
    #[must_use]
    pub fn project(&self) -> &crate::PlatformProjectPolicy {
        match self.target.route().platform_project() {
            Some(project) => project,
            None => unreachable!("sealed Platform authority always has a Platform project policy"),
        }
    }

    pub(crate) const fn target(&self) -> &OpenAiTarget {
        &self.target
    }

    pub(crate) const fn is_local_fixture(&self) -> bool {
        self.target.route().is_local_platform_fixture()
    }
}

impl std::fmt::Debug for OpenAiPlatformConfigurationAuthority {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenAiPlatformConfigurationAuthority")
            .field("descriptor", self.target.descriptor())
            .field("project", self.project())
            .field("http", &self.http)
            .field("execution", &"not constructed")
            .field("admitted_facts", &self.admitted_facts)
            .finish()
    }
}

/// Codex subscription model whose execution is supplied only by the shared Responses host.
#[cfg(feature = "native-auth")]
///
/// The admitted descriptor is immutable. The credential receipt behind it is
/// renewed before dispatch when the shared store refreshed the grant.
pub struct OpenAiLanguageModel {
    target: std::sync::Mutex<Arc<CodexDispatchTarget>>,
    renewal: tokio::sync::Mutex<()>,
    renewal_floor: CodexRenewalFloor,
    receipt_renewal: CodexReceiptRenewal,
    admitted_from: Timestamp,
    admitted_until: Timestamp,
    client: plakous_http::Client,
    config: StandardOpenAiHttpConfig,
    attempt_data: ResponsesAttemptDialectData,
    descriptor: ResolvedModelDescriptor,
}

/// One exact credential receipt bound to the shared Responses host. A renewal
/// replaces the whole target; a prepared call keeps its own.
#[cfg(feature = "native-auth")]
struct CodexDispatchTarget {
    target: OpenAiTarget,
    service: CodexDispatchService,
}

#[cfg(feature = "native-auth")]
type CodexDispatchService = Arc<ResponsesAttemptService<CodexResponsesDialect, CodexDispatchGuard>>;

#[cfg(feature = "native-auth")]
fn codex_authorization() -> Result<
    ResponsesAuthorizationAdapter<CodexResponsesDialect, CodexDispatchGuard>,
    StandardOpenAiHttpBuildError,
> {
    Ok(ResponsesAuthorizationAdapter::new(
        CODEX_RESPONSES_PREFLIGHT_REVISION,
        CODEX_RESPONSES_FINAL_AUTHORIZATION_REVISION,
        codex_responses_preflight,
        revalidate_codex_responses,
    )
    .map_err(|_| StandardOpenAiHttpBuildError::SharedBinding)?
    .with_credential_rejected(codex_credential_rejected)
    .with_transport_failed(CodexDispatchGuard::transport_failed))
}

#[cfg(feature = "native-auth")]
fn codex_dispatch_service(
    target: &OpenAiTarget,
    client: &plakous_http::Client,
    attempt_data: &ResponsesAttemptDialectData,
) -> Result<CodexDispatchService, StandardOpenAiHttpBuildError> {
    let dialect = Arc::new(CodexResponsesDialect::new(target.clone(), client.clone()));
    Ok(Arc::new(
        ResponsesAttemptService::new(
            client.clone(),
            dialect,
            attempt_data.clone(),
            codex_authorization()?,
        )
        .map_err(|_| StandardOpenAiHttpBuildError::SharedBinding)?,
    ))
}

#[cfg(feature = "native-auth")]
impl OpenAiLanguageModel {
    /// Binds an admitted native target to the provider-blind HTTP client.
    pub(crate) fn new_codex(
        target: OpenAiTarget,
        client: plakous_http::Client,
        config: StandardOpenAiHttpConfig,
        receipt_renewal: CodexReceiptRenewal,
    ) -> Result<Self, StandardOpenAiHttpBuildError> {
        if !target.route().is_native_codex() {
            return Err(StandardOpenAiHttpBuildError::UnsupportedRoute);
        }
        if config.operation_timeout.is_zero() || config.event_channel_capacity == 0 {
            return Err(StandardOpenAiHttpBuildError::InvalidPolicy);
        }
        let descriptor = target.descriptor().clone();
        let replay = ResponsesReplayProjection::new(
            descriptor.protocol().clone(),
            descriptor.model_family().cloned(),
            descriptor.requested_model().clone(),
            ReplayKindId::new("responses.output_item")
                .map_err(|_| StandardOpenAiHttpBuildError::SharedBinding)?,
            1,
        )
        .map_err(|_| StandardOpenAiHttpBuildError::SharedBinding)?
        .requiring_encrypted_reasoning_with_optional_status();
        let response_limits = ResponsesResponseLimits::default();
        // Keep the production Codex transport on the exact Codex stream
        // profile.  In particular, the reference client treats the first
        // valid typed terminal as the response-body boundary; buffered
        // transport trailers after it are not a second semantic event.
        let stream = ResponsesStreamDialectData::codex(descriptor.requested_model().as_str());
        let request_policy = codex_request_policy(&descriptor, target.codex_responses_lite());
        let authorization = codex_authorization()?;
        // The descriptor's route evidence is valid only inside the admitting
        // catalog's window; renewal never carries a target out of it.
        let (admitted_from, admitted_until) = target
            .native_codex_catalog_window()
            .ok_or(StandardOpenAiHttpBuildError::UnsupportedRoute)?;
        let revisions =
            codex_semantic_revisions().map_err(|_| StandardOpenAiHttpBuildError::SharedBinding)?;
        let attempt_target = ResponsesAttemptTarget::from_admitted_target(
            &descriptor,
            target
                .route()
                .account_binding()
                .map_err(|_| StandardOpenAiHttpBuildError::SharedBinding)?,
            revisions,
        )
        .map_err(|_| StandardOpenAiHttpBuildError::SharedBinding)?;
        let attempt_data = ResponsesAttemptDialectData::new(
            OPENAI_RESPONSES_DIALECT_REVISION,
            attempt_target,
            &client,
            &authorization,
            LedgerPolicy::OPENAI_STRICT,
            ResponsesAuthSlots::Secondary,
            request_policy,
            stream,
            config.responses_limits,
            replay,
            ResponsesStorageMode::Stateless,
            ResponsesProviderErrorPolicy::OpenAiCompatible,
            response_limits,
            Some(plakous_http::HeaderName::from_static("x-request-id")),
            true,
        )
        .map_err(|_| StandardOpenAiHttpBuildError::SharedBinding)?;
        let service = codex_dispatch_service(&target, &client, &attempt_data)?;
        Ok(Self {
            target: std::sync::Mutex::new(Arc::new(CodexDispatchTarget { target, service })),
            renewal: tokio::sync::Mutex::new(()),
            renewal_floor: CodexRenewalFloor::default(),
            receipt_renewal,
            admitted_from,
            admitted_until,
            client,
            config,
            attempt_data,
            descriptor,
        })
    }

    fn current_target(&self) -> Result<Arc<CodexDispatchTarget>, RuntimeModelAuthorityError> {
        self.target
            .lock()
            .map(|target| Arc::clone(&target))
            .map_err(|_| RuntimeModelAuthorityError::Unavailable)
    }

    async fn revalidate_target(
        &self,
        target: &CodexDispatchTarget,
        checked_at: Timestamp,
    ) -> Result<RuntimeModelAuthority, RuntimeModelAuthorityError> {
        if checked_at >= self.admitted_until {
            return Err(RuntimeModelAuthorityError::Stale);
        }
        target
            .target
            .revalidate_native_runtime_authority(checked_at)
            .await
    }

    /// Replaces a stale receipt before any dispatch: one claimed refresh when
    /// due and one exact catalog fetch of the same login selection, accepted
    /// only for the same route identity and admitted row. Concurrent callers
    /// note their `checked_at`, wait for one renewal and then check the
    /// receipt it installed, whose observation never postdates the earliest
    /// noted check.
    async fn renew(
        &self,
        stale: Arc<CodexDispatchTarget>,
        checked_at: Timestamp,
    ) -> Result<RuntimeModelAuthority, RuntimeModelAuthorityError> {
        // Outside the admitted window no renewal can help: refuse before any
        // refresh, catalog request or wait.
        if checked_at < self.admitted_from || checked_at >= self.admitted_until {
            return Err(RuntimeModelAuthorityError::Stale);
        }
        self.renewal_floor.note(checked_at);
        let _renewal = self.renewal.lock().await;
        // A renewal that failed while this caller waited cleared the floor.
        self.renewal_floor.note(checked_at);
        let current = self.current_target()?;
        if !Arc::ptr_eq(&current, &stale) {
            return self.revalidate_target(&current, checked_at).await;
        }
        let renewed = self
            .receipt_renewal
            .renew(&stale.target, self.admitted_until, &self.renewal_floor)
            .await;
        self.renewal_floor.clear();
        let target = renewed?;
        let service = codex_dispatch_service(&target, &self.client, &self.attempt_data)
            .map_err(|_| RuntimeModelAuthorityError::Invalid)?;
        let renewed = Arc::new(CodexDispatchTarget { target, service });
        let authority = self.revalidate_target(&renewed, checked_at).await?;
        *self
            .target
            .lock()
            .map_err(|_| RuntimeModelAuthorityError::Unavailable)? = renewed;
        Ok(authority)
    }
}

#[cfg(feature = "native-auth")]
impl LanguageModel for OpenAiLanguageModel {
    fn reachability_probe(&self) -> Option<plakous_model::ReachabilityProbe> {
        let origin = plakous_http::Origin::parse(
            self.current_target().ok()?.target.route().origin().as_str(),
        )
        .ok()?;
        Some(plakous_responses_dialect::bind_reachability_probe(
            self.client.clone(),
            origin,
            self.descriptor.route().clone(),
        ))
    }

    fn revalidate_runtime_authority(
        self: Arc<Self>,
        checked_at: Timestamp,
    ) -> RuntimeModelAuthorityOperation {
        Box::pin(async move {
            let target = self.current_target()?;
            match self.revalidate_target(&target, checked_at).await {
                // The fence moved (Unavailable) or the access token entered
                // its refresh lead (Stale): renew the receipt.
                Err(
                    RuntimeModelAuthorityError::Stale | RuntimeModelAuthorityError::Unavailable,
                ) => self.renew(target, checked_at).await,
                result => result,
            }
        })
    }

    fn prepare_call(
        self: Arc<Self>,
        plan: ValidatedRequestPlan,
        context: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError> {
        let current = self.current_target()?;
        let target = &current.target;
        if let Some(projection) = plan.request().tool_projection() {
            let target = projection.target();
            if target.protocol() != self.descriptor.protocol().as_str()
                || target.integration_profile() != self.descriptor.profile().as_str()
                || target.family()
                    != self
                        .descriptor
                        .model_family()
                        .map_or("", ModelFamilyId::as_str)
                || target.requested_model() != self.descriptor.requested_model().as_str()
                || target.effective_model() != self.descriptor.requested_model().as_str()
            {
                return Err(preparation_rejection(
                    ModelPreparationErrorKind::InvalidConfiguration,
                    "Codex compiled tool projection does not match the admitted exact target",
                ));
            }
        }
        target.validate_current_route_evidence().map_err(|_| {
            preparation_rejection(
                ModelPreparationErrorKind::InvalidConfiguration,
                "Codex route evidence is not current",
            )
        })?;
        target.validate_plan(&plan).map_err(|_| {
            preparation_rejection(
                ModelPreparationErrorKind::InvalidRequest,
                "Codex plan belongs to another exact target",
            )
        })?;
        self.descriptor
            .validate_plan_admission(plan.request(), &plan)?;
        target
            .validate_request_intent(plan.request())
            .map_err(|_| {
                preparation_rejection(
                    ModelPreparationErrorKind::InvalidRequest,
                    "Codex request intent is not admitted",
                )
            })?;
        validate_codex_preparation_authority(target, context.authority(), context.prepared_at())?;
        let cache_affinity = context.cache_affinity().ok_or_else(|| {
            preparation_rejection(
                ModelPreparationErrorKind::InvalidRequest,
                "Codex OAuth requires branch cache affinity",
            )
        })?;
        let affinity_identity = cache_affinity.identity();

        let authority =
            codex_request_authority(target, &plan, Some(affinity_identity)).map_err(|()| {
                preparation_rejection(
                    ModelPreparationErrorKind::InvalidRequest,
                    "Codex request authority could not be constructed",
                )
            })?;
        let replay_bridge = codex_artifact_replay_bridge(
            context.active_family_artifact(),
            plan.request(),
            authority.clone(),
            context.prepared_at(),
        )
        .map_err(|error| {
            preparation_rejection(
                ModelPreparationErrorKind::InvalidRequest,
                format!("Codex active Responses artifact is not reusable: {error}"),
            )
        })?;
        let prepared_attempt = prepare_responses_attempt(
            authority,
            plan.request(),
            replay_bridge
                .as_ref()
                .map(ResponsesArtifactReplayBridge::context),
            &self.attempt_data,
        )
        .map_err(|error| {
            preparation_rejection(
                error.preparation_error_kind(),
                format!("Codex Responses request could not be prepared: {error}"),
            )
        })?;
        let cache_use = match (replay_bridge.as_ref(), context.active_family_artifact()) {
            (Some(_), Some(active)) => PreparedCacheUse::ActiveFamilyArtifactAndRuntimeAffinity {
                artifact: active.integrity(),
                affinity: affinity_identity,
            },
            (None, _) => PreparedCacheUse::RuntimeAffinity {
                affinity: affinity_identity,
            },
            (Some(_), None) => {
                return Err(preparation_rejection(
                    ModelPreparationErrorKind::InvalidConfiguration,
                    "Codex replay bridge omitted its active family artifact",
                ));
            }
        };

        let wire_digest = prepared_attempt.wire_digest();
        let wire_size = u64::try_from(prepared_attempt.wire_size()).map_err(|_| {
            preparation_rejection(
                ModelPreparationErrorKind::ResourceLimit,
                "Codex prepared wire size is not representable",
            )
        })?;
        let count_observation = self
            .descriptor
            .context_contract()
            .and_then(|contract| contract.measurements().first())
            .map(|certificate| {
                let measured_tokens = if matches!(
                    certificate.kind(),
                    CountCertificateKind::ReferenceClientEnvelopeUpperBound { .. }
                ) {
                    codex_reference_client_input_tokens(prepared_attempt.body().as_ref())
                        .ok_or_else(|| {
                            preparation_rejection(
                                ModelPreparationErrorKind::ResourceLimit,
                                "Codex complete-input upper bound is not representable",
                            )
                        })?
                } else {
                    wire_size
                };
                CountObservation::new(
                    certificate.certificate_id(),
                    certificate.revision(),
                    measured_tokens,
                )
                .map_err(|_| {
                    preparation_rejection(
                        ModelPreparationErrorKind::InvalidConfiguration,
                        "Codex test measurement fixture is invalid",
                    )
                })
            })
            .transpose()?;
        let token_projection =
            PreparedTokenProjection::new(wire_digest, wire_size, count_observation)?
                .with_input_parts(plakous_model::ContextInputParts::measure_wire(
                    prepared_attempt.body().as_ref(),
                ))
                .with_wire_input(
                    prepared_attempt.body().as_ref(),
                    Some(plakous_model::CONSERVATIVE_IMAGE_MAX_TOKENS),
                    &context,
                )?;
        let protocol_family = ProtocolId::new(RESPONSES_FAMILY_ID).map_err(|_| {
            preparation_rejection(
                ModelPreparationErrorKind::InvalidConfiguration,
                "Responses family identity is invalid",
            )
        })?;
        let output_limit = prepared_attempt
            .enforced_output_tokens()
            .map(|tokens| {
                PreparedOutputLimit::enforced_wire_maximum(
                    tokens,
                    "max_output_tokens",
                    wire_digest,
                    OPENAI_RESPONSES_ADAPTER_REVISION,
                )
            })
            .transpose()?;
        let output_limit = if self.descriptor.context_contract().is_some_and(|contract| {
            matches!(
                contract.capacity().rule(),
                CapacityRule::ProviderReservedInput { .. }
            )
        }) {
            None
        } else {
            output_limit
        };
        #[cfg(test)]
        let output_limit = match output_limit {
            Some(output_limit) => Some(output_limit),
            None => self
                .descriptor
                .context_contract()
                .filter(|contract| {
                    matches!(
                        contract.capacity().evidence_class(),
                        ContextEvidenceClass::ReleaseVerified
                    )
                })
                .map(|contract| {
                    PreparedOutputLimit::release_verified_hard_cap(
                        NonZeroU32::new(TEST_OUTPUT_HARD_CAP).ok_or_else(|| {
                            preparation_rejection(
                                ModelPreparationErrorKind::InvalidConfiguration,
                                "Codex test output cap is invalid",
                            )
                        })?,
                        contract.capacity().identity(),
                        &self.descriptor,
                        &context,
                        &protocol_family,
                        OPENAI_RESPONSES_ADAPTER_REVISION,
                        RESPONSES_CANONICAL_PROJECTION_REVISION,
                        RESPONSES_EXACT_WIRE_ACCOUNTING_REVISION,
                    )
                    .map_err(ModelPreparationError::from)
                })
                .transpose()?,
        };
        let projection = PreparedModelProjection::new(
            &self.descriptor,
            &plan,
            &context,
            protocol_family,
            OPENAI_RESPONSES_ADAPTER_REVISION,
            RESPONSES_CANONICAL_PROJECTION_REVISION,
            RESPONSES_EXACT_WIRE_ACCOUNTING_REVISION,
            wire_digest,
            wire_size,
            token_projection,
            cache_use,
            output_limit,
        )?;
        let record =
            PreparedCallRecord::for_prepared(&self.descriptor, &plan, projection, &context)?;

        let canonical_tools = canonical_local_tools(plan.request());
        let expected_model = self
            .descriptor
            .reported_model()
            .unwrap_or_else(|| self.descriptor.requested_model())
            .clone();
        let canonical = {
            let canonical = ResponsesCanonicalDialectData::codex(expected_model, canonical_tools)
                .map_err(|_| {
                preparation_rejection(
                    ModelPreparationErrorKind::InvalidConfiguration,
                    "Codex canonical projection facts are invalid",
                )
            })?;
            let canonical = if target.codex_responses_lite() {
                canonical.with_codex_lite_namespace()
            } else {
                canonical
            };
            if is_native_compaction_request(plan.request()) {
                let producing = prepared_attempt.authority().artifact_binding();
                canonical.with_native_compaction(plakous_model::CompactionReplayBinding::new(
                    producing.dialect().clone(),
                    producing.model_family().cloned(),
                    producing.model().clone(),
                    producing.account(),
                ))
            } else {
                canonical
            }
        };
        let artifact_binding = prepared_attempt.authority().artifact_binding().clone();
        let clock = target.clock();
        let content_filter_retry = !is_native_compaction_request(plan.request());
        let prepared_execution = PreparedResponsesModelExecution::new(
            prepared_attempt,
            canonical,
            plakous_responses::CanonicalIdSources::with_clock(Arc::clone(&clock)),
            plakous_responses::CanonicalIdSources::with_clock(Arc::clone(&clock)),
            replay_bridge
                .as_ref()
                .map(|bridge| bridge.hosted_programs().clone())
                .unwrap_or_default(),
            plan,
            artifact_binding,
            Arc::clone(&clock),
            self.descriptor.clone(),
            self.config.event_channel_capacity(),
            None,
        )
        .map_err(|_| {
            preparation_rejection(
                ModelPreparationErrorKind::InvalidConfiguration,
                "Codex prepared model execution binding is invalid",
            )
        })?;
        let prepared_execution = prepared_execution.with_content_filter_retry(content_filter_retry);
        let service = Arc::clone(&current.service);
        let config = self.config;
        Ok(PreparedModelCall::new(record, move |call_context| {
            let deadline = operation_deadline(clock.as_ref(), config, None);
            start_prepared_responses_model_call(prepared_execution, service, call_context, deadline)
        }))
    }
}

/// Token estimate for the exact serialized Codex request body.
///
/// Ordinary items match the pinned Codex client: `ceil(utf8_bytes / 4)`.
/// Native `{type:compaction}` items use the same client's encrypted-reasoning
/// heuristic: `ceil((len*3/4 − 650) / 4)` of `encrypted_content`.
/// Whole-body ceil/4 of ciphertext JSON is not that envelope.
#[cfg(feature = "native-auth")]
pub(crate) fn codex_complete_input_token_upper_bound(wire_size: u64) -> Option<u64> {
    wire_size
        .checked_add(CODEX_COMPLETE_INPUT_BYTES_PER_TOKEN - 1)
        .map(|rounded| rounded / CODEX_COMPLETE_INPUT_BYTES_PER_TOKEN)
}

/// Codex CLI `estimate_reasoning_length`: base64-decode heuristic, then bytes.
#[cfg(feature = "native-auth")]
fn codex_encrypted_item_visible_bytes(encrypted_len: u64) -> Option<u64> {
    encrypted_len
        .checked_mul(3)?
        .checked_div(4)
        .map(|decoded| decoded.saturating_sub(650))
}

/// Reference-client complete-input tokens for one serialized Codex body.
///
/// Compaction items are substituted with [`codex_encrypted_item_visible_bytes`]
/// before the ordinary `ceil(bytes/4)` bound. Missing/unparseable bodies stay
/// on the naive whole-body bound.
#[cfg(feature = "native-auth")]
pub(crate) fn codex_reference_client_input_tokens(body: &[u8]) -> Option<u64> {
    let wire_size = u64::try_from(body.len()).ok()?;
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return codex_complete_input_token_upper_bound(wire_size);
    };
    let Some(input) = value.get("input").and_then(serde_json::Value::as_array) else {
        return codex_complete_input_token_upper_bound(wire_size);
    };
    let mut json_bytes = 0_u64;
    let mut visible_bytes = 0_u64;
    for item in input {
        if item.get("type").and_then(serde_json::Value::as_str) != Some("compaction") {
            continue;
        }
        let Some(encrypted) = item
            .get("encrypted_content")
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        let encoded = serde_json::to_vec(item).ok()?;
        json_bytes = json_bytes.checked_add(u64::try_from(encoded.len()).ok()?)?;
        visible_bytes = visible_bytes.checked_add(codex_encrypted_item_visible_bytes(
            u64::try_from(encrypted.len()).ok()?,
        )?)?;
    }
    let adjusted = wire_size
        .checked_sub(json_bytes)?
        .checked_add(visible_bytes)?;
    codex_complete_input_token_upper_bound(adjusted)
}

#[cfg(feature = "native-auth")]
fn preparation_rejection(
    kind: ModelPreparationErrorKind,
    message: impl AsRef<str>,
) -> ModelPreparationError {
    ModelPreparationError::rejected(kind, message)
}

#[cfg(feature = "native-auth")]
fn validate_codex_preparation_authority(
    target: &OpenAiTarget,
    authority: &RuntimeModelAuthority,
    prepared_at: Timestamp,
) -> Result<(), ModelPreparationError> {
    if authority.observed_at() > prepared_at {
        return Err(preparation_rejection(
            ModelPreparationErrorKind::InvalidConfiguration,
            "Codex runtime authority is newer than preparation time",
        ));
    }
    if authority
        .valid_until()
        .is_some_and(|valid_until| valid_until <= prepared_at)
    {
        return Err(preparation_rejection(
            ModelPreparationErrorKind::InvalidConfiguration,
            "Codex runtime authority expired before preparation",
        ));
    }
    authority.validate(target.descriptor(), prepared_at)?;
    let (credential_fence, _) = target
        .native_codex_credential_selection()
        .ok_or(RuntimeModelAuthorityError::Unavailable)?;
    if authority.account_binding() != target.account_binding()
        || authority.credential_authority_binding() != target.credential_authority_binding()
        || authority.credential_generation() != credential_fence.generation().get()
    {
        return Err(RuntimeModelAuthorityError::ForeignAuthority.into());
    }
    Ok(())
}

#[cfg(feature = "native-auth")]
fn codex_artifact_replay_bridge(
    active: Option<&plakous_model::ActiveFamilyArtifact>,
    request: &ModelRequest,
    authority: ResponsesRequestAuthority,
    now: Timestamp,
) -> Result<Option<ResponsesArtifactReplayBridge>, plakous_responses_dialect::ResponsesRequestError>
{
    let Some(artifact) = active else {
        return Ok(None);
    };
    match ResponsesArtifactReplayBridge::from_active_artifact(
        artifact,
        request,
        authority,
        ResponsesReplayKind::new("responses.output_item", 1),
        None,
        None,
        now,
    ) {
        Ok(bridge) => Ok(Some(bridge)),
        // Not this request's artifact: prepare fresh. See the xAI route for the
        // same rule and the live failures that motivated it.
        Err(error) if error.artifact_is_not_for_this_request() => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(feature = "native-auth")]
fn codex_request_authority(
    target: &OpenAiTarget,
    plan: &ValidatedRequestPlan,
    cache_affinity: Option<Digest>,
) -> Result<ResponsesRequestAuthority, ()> {
    let revisions = codex_semantic_revisions()?;
    ResponsesRequestAuthority::from_admitted_operation(
        target.descriptor(),
        plan,
        plan.request(),
        target.route().account_binding().map_err(|_| ())?,
        revisions,
        cache_affinity,
        ResponsesStoragePolicy::Stateless,
    )
    .map_err(|_| ())
}

#[cfg(feature = "native-auth")]
fn codex_semantic_revisions() -> Result<ResponsesSemanticRevisions, ()> {
    ResponsesSemanticRevisions::new(5, 3, 6).map_err(|_| ())
}

#[cfg(feature = "native-auth")]
/// Returns the exact Codex subscription request policy used by execution for
/// one resolved descriptor and authenticated catalog wire mode.
///
/// Provider admission and the Responses-family configuration compiler consume
/// this same table; callers must not reproduce its wire decisions.
fn codex_request_policy(
    descriptor: &ResolvedModelDescriptor,
    responses_lite: bool,
) -> ResponsesRequestPolicy {
    if responses_lite {
        return ResponsesRequestPolicy::openai_codex_lite(
            crate::OPENAI_CODEX_ENCRYPTED_REASONING_INCLUDE,
        );
    }
    let parallel_tool_calls = descriptor
        .capabilities()
        .get(Capability::ParallelTools)
        .is_some_and(|fact| matches!(fact.support(), Support::Native | Support::Emulated));
    ResponsesRequestPolicy::openai_codex(
        false,
        parallel_tool_calls,
        crate::OPENAI_CODEX_ENCRYPTED_REASONING_INCLUDE,
    )
}

/// Returns the exact request-wire policy for an admitted Platform target.
///
/// The target's explicit storage disposition selects `store`; reviewed Platform
/// rows deliberately exclude image input and parallel function calls in this
/// adapter revision.
pub fn openai_platform_request_policy(
    authority: &OpenAiPlatformConfigurationAuthority,
) -> Result<ResponsesRequestPolicy, ResponsesProviderConfigurationError> {
    let target = &authority.target;
    if !matches!(
        target.route().access_mode(),
        crate::AccessMode::PlatformApi | crate::AccessMode::LocalFixture
    ) {
        return Err(ResponsesProviderConfigurationError::PolicyMismatch(
            "provider_route",
        ));
    }
    let storage = match target.route().storage() {
        crate::StorageMode::ProviderStored => ResponsesStoragePolicy::ProviderStored,
        crate::StorageMode::Stateless => ResponsesStoragePolicy::Stateless,
    };
    Ok(if authority.is_local_fixture() {
        ResponsesRequestPolicy::openai_platform(storage, false, false)
    } else {
        ResponsesRequestPolicy::openai_platform_stateless()
    })
}

/// Compiles a complete Platform selection against the same closed model and
/// request tables retained by its sealed target.
///
/// This creates semantic configuration authority only. It does not construct
/// an HTTP executor; [`OpenAiPlatformModelBuilder::resolve`] constructs that executor.
pub fn compile_openai_platform_configuration(
    selected: &SelectedProviderConfiguration,
    local: &OpenAiPlatformConfigurationAuthority,
    authority: ResolvedProviderAuthorityBinding,
) -> Result<CompiledProviderConfiguration, ResponsesProviderConfigurationError> {
    let policy = validate_openai_platform_selection(selected, local)?;
    compile_provider_configuration(selected, local.descriptor().clone(), authority, policy)
}

pub(crate) fn validate_openai_platform_selection(
    selected: &SelectedProviderConfiguration,
    local: &OpenAiPlatformConfigurationAuthority,
) -> Result<ResponsesRequestPolicy, ResponsesProviderConfigurationError> {
    let target = &local.target;
    target
        .validate_current_route_evidence()
        .map_err(|_| ResponsesProviderConfigurationError::PolicyMismatch("descriptor"))?;
    let descriptor = target.descriptor().clone();
    let route = selected.route();
    if route.protocol().as_str() != crate::OPENAI_RESPONSES_PROTOCOL_ID
        || route.route().as_str() != crate::OPENAI_PLATFORM_ROUTE_ID
        || route.integration_profile() != descriptor.profile()
        || route.integration_revision().as_str() != OPENAI_RESPONSES_ADAPTER_REVISION
        || route.auth_profile().as_str() != crate::OPENAI_PLATFORM_AUTH_PROFILE_ID
        || route.auth_revision().as_str() != crate::OPENAI_PLATFORM_AUTH_REVISION
        || route.authority_class().as_str() != crate::OPENAI_PLATFORM_AUTHORITY_CLASS
        || route.credential_selection().as_str() != crate::OPENAI_PLATFORM_CREDENTIAL_SELECTION
        || route.requested_model() != descriptor.requested_model()
    {
        return Err(ResponsesProviderConfigurationError::PolicyMismatch(
            "provider_route",
        ));
    }
    if matches!(
        selected.behavior().reasoning_effort,
        ReasoningEffortDisposition::Enabled(plakous_model::ReasoningEffort::Ultra)
    ) {
        return Err(ResponsesProviderConfigurationError::PolicyMismatch(
            "reasoning_effort",
        ));
    }
    if !local.is_local_fixture()
        && !matches!(
            selected.behavior().reasoning_effort,
            ReasoningEffortDisposition::Enabled(
                plakous_model::ReasoningEffort::None
                    | plakous_model::ReasoningEffort::Low
                    | plakous_model::ReasoningEffort::Medium
                    | plakous_model::ReasoningEffort::High
                    | plakous_model::ReasoningEffort::XHigh
                    | plakous_model::ReasoningEffort::Max
            )
        )
    {
        return Err(ResponsesProviderConfigurationError::PolicyMismatch(
            "reasoning_effort",
        ));
    }
    if !local.is_local_fixture()
        && (!selected.behavior().hosted_tools.is_empty()
            || target.route().storage() != crate::StorageMode::Stateless)
    {
        return Err(ResponsesProviderConfigurationError::PolicyMismatch(
            "hosted_tools_or_storage",
        ));
    }
    openai_platform_request_policy(local)
}

/// Compiles one explicit Codex subscription selection through the exact
/// catalog facts and request policy used by execution.
#[cfg(feature = "native-auth")]
pub fn compile_codex_subscription_configuration(
    selected: &SelectedProviderConfiguration,
    authority: &RuntimeModelAuthority,
    evidence: &CodexModelResolutionEvidence,
) -> Result<CompiledProviderConfiguration, ResponsesProviderConfigurationError> {
    let descriptor = evidence
        .validated_descriptor()
        .map_err(ResponsesProviderConfigurationError::ProviderConfiguration)?
        .clone();
    let route = selected.route();
    if route.protocol().as_str() != crate::OPENAI_CODEX_RESPONSES_PROTOCOL_ID
        || route.route().as_str() != crate::OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID
        || route.integration_profile().as_str() != crate::OPENAI_CODEX_COMPATIBILITY_PROFILE_ID
        || route.integration_revision().as_str() != OPENAI_RESPONSES_ADAPTER_REVISION
        || route.auth_profile().as_str() != crate::OPENAI_CODEX_COMPATIBILITY_PROFILE_ID
        || route.auth_revision().as_str() != crate::OPENAI_CODEX_SOURCE_REVISION
        || route.authority_class().as_str() != "oauth-subscription"
        || route.credential_selection().as_str() != "primary"
    {
        return Err(ResponsesProviderConfigurationError::PolicyMismatch(
            "provider_route",
        ));
    }
    let ReasoningEffortDisposition::Enabled(effort) = selected.behavior().reasoning_effort else {
        return Err(ResponsesProviderConfigurationError::PolicyMismatch(
            "reasoning_effort",
        ));
    };
    if !evidence.supports_reasoning_effort(effort) {
        return Err(ResponsesProviderConfigurationError::PolicyMismatch(
            "reasoning_effort",
        ));
    }
    validate_codex_catalog_tool_mode(selected, evidence.tool_mode())?;
    let semantic_evidence = evidence.semantic_provider_evidence()?;
    let stable_authority =
        ResolvedProviderAuthorityBinding::new(&descriptor, authority, semantic_evidence)?;
    let policy = codex_request_policy(&descriptor, evidence.uses_responses_lite());
    compile_provider_configuration(selected, descriptor, stable_authority, policy)
}

#[cfg(feature = "native-auth")]
fn validate_codex_catalog_tool_mode(
    selected: &SelectedProviderConfiguration,
    mode: Option<CodexCatalogToolMode>,
) -> Result<(), ResponsesProviderConfigurationError> {
    let selected_mode = selected.behavior().tool_mode;
    let names = selected
        .behavior()
        .tools
        .iter()
        .map(|tool| tool.definition().name().as_str())
        .collect::<Vec<_>>();
    if codex_tool_mode_accepts(mode, selected_mode, &names) {
        Ok(())
    } else {
        Err(ResponsesProviderConfigurationError::PolicyMismatch("tools"))
    }
}

#[cfg(feature = "native-auth")]
fn codex_tool_mode_accepts(
    mode: Option<CodexCatalogToolMode>,
    selected: ToolPresentationMode,
    names: &[&str],
) -> bool {
    let has_exec = names.contains(&"exec");
    let has_wait = names.contains(&"wait");
    let exact_code_mode_controls = names.len() == 2 && has_exec && has_wait;
    let hybrid_code_mode_surface =
        has_exec && has_wait && names.iter().any(|name| !matches!(*name, "exec" | "wait"));
    match mode {
        Some(CodexCatalogToolMode::CodeModeOnly) => {
            (selected == ToolPresentationMode::LocalCompositionOnly && exact_code_mode_controls)
                || (selected == ToolPresentationMode::Direct && !has_exec && !has_wait)
        }
        Some(CodexCatalogToolMode::Direct) => {
            selected == ToolPresentationMode::Direct && !has_exec && !has_wait
        }
        Some(CodexCatalogToolMode::CodeMode) => {
            selected == ToolPresentationMode::LocalComposition && hybrid_code_mode_surface
        }
        None => false,
    }
}

pub(crate) fn operation_deadline(
    clock: &dyn Clock,
    config: StandardOpenAiHttpConfig,
    requested: Option<Timestamp>,
) -> Instant {
    let now = Instant::now();
    let configured = now.checked_add(config.operation_timeout()).unwrap_or(now);
    let Some(requested) = requested else {
        return configured;
    };
    let wall_now = clock.now();
    if requested <= wall_now {
        return now;
    }
    let seconds = requested.seconds().saturating_sub(wall_now.seconds());
    now.checked_add(Duration::from_secs(seconds))
        .map_or(configured, |deadline| deadline.min(configured))
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "unit tests may construct exact static authority fixtures"
)]
mod platform_tests {
    use std::sync::Arc;

    use plakous_model::{Capability, CapacityRule, FixedClock, Timestamp};

    use super::{OpenAiPlatformModelBuilder, OpenAiPlatformModelFacts};
    use crate::{
        CredentialAuthorityId, CredentialReference, EndpointOrigin, OPENAI_PLATFORM_ORIGIN,
        OpenAiPlatformRouteConfig, OpenAiProjectId, PlatformProjectPolicy, PlatformStoragePolicy,
        SecretString, StaticPlatformCredentialSource,
        manifest::OPENAI_PLATFORM_MODEL_MANIFEST_REVIEWED_AT_UNIX_SECONDS,
    };

    fn source(reference: &CredentialReference) -> Arc<StaticPlatformCredentialSource> {
        Arc::new(StaticPlatformCredentialSource::new(
            SecretString::new("fixture-key-never-dispatched").expect("fixture secret"),
            reference.clone(),
            CredentialAuthorityId::new("openai-platform-fixture-authority")
                .expect("fixture authority"),
        ))
    }

    #[test]
    fn token_window_is_installed_on_a_wire_byte_envelope() {
        let reference =
            CredentialReference::application("openai-platform-fixture").expect("fixture reference");
        let clock = Arc::new(FixedClock::new(
            Timestamp::new(
                OPENAI_PLATFORM_MODEL_MANIFEST_REVIEWED_AT_UNIX_SECONDS + 1,
                0,
            )
            .expect("fixture time"),
        ));
        let tokens = std::num::NonZeroU64::new(272_000).expect("window");
        let sealed = OpenAiPlatformModelBuilder::new(
            reference.clone(),
            source(&reference),
            OpenAiPlatformModelFacts::Gpt6Luna,
            PlatformStoragePolicy::Stateless,
            PlatformProjectPolicy::DefaultProject,
        )
        .clock(clock)
        .context_envelope(tokens)
        .seal()
        .expect("sealed authority");
        let contract = sealed
            .descriptor()
            .context_contract()
            .expect("caller envelope");
        assert_eq!(
            contract.accounting_revision(),
            "responses-wire-bytes-openai-platform-v1"
        );
        let measurement = contract
            .measurements()
            .iter()
            .find(|measurement| {
                measurement.certificate_id() == "openai-platform-complete-wire-bytes"
            })
            .expect("wire-byte measurement");
        assert_eq!(
            measurement.method(),
            "complete-lowered-wire-byte-upper-bound"
        );
        let CapacityRule::ProviderReservedInput {
            maximum_projected_input,
            ..
        } = contract.capacity().rule()
        else {
            panic!("envelope rule");
        };
        assert_eq!(
            maximum_projected_input, 272_000,
            "the token window is passed through; it is not converted to bytes"
        );
    }

    #[test]
    fn literal_platform_models_are_closed_without_hosted_tool_claims() {
        let reference =
            CredentialReference::application("openai-platform-fixture").expect("fixture reference");
        let clock = Arc::new(FixedClock::new(
            Timestamp::new(
                OPENAI_PLATFORM_MODEL_MANIFEST_REVIEWED_AT_UNIX_SECONDS + 1,
                0,
            )
            .expect("fixture time"),
        ));
        for facts in [
            OpenAiPlatformModelFacts::Gpt6Sol,
            OpenAiPlatformModelFacts::Gpt6Luna,
            OpenAiPlatformModelFacts::Gpt56Sol,
            OpenAiPlatformModelFacts::Gpt56Terra,
            OpenAiPlatformModelFacts::Gpt56Luna,
        ] {
            let sealed = OpenAiPlatformModelBuilder::new(
                reference.clone(),
                source(&reference),
                facts,
                PlatformStoragePolicy::Stateless,
                PlatformProjectPolicy::DefaultProject,
            )
            .clock(clock.clone())
            .seal()
            .expect("sealed local authority");
            assert_eq!(
                sealed.descriptor().requested_model().as_str(),
                facts.model_id()
            );
            assert_eq!(sealed.project(), &PlatformProjectPolicy::DefaultProject);
            assert_eq!(
                sealed
                    .descriptor()
                    .capabilities()
                    .get(Capability::HostedProgrammaticToolCalling)
                    .map(plakous_model::FeatureSupport::support),
                None
            );
        }
    }

    #[test]
    fn project_and_storage_are_explicit_serialized_route_authority() {
        let origin = EndpointOrigin::parse(OPENAI_PLATFORM_ORIGIN).expect("official origin");
        let reference =
            CredentialReference::application("openai-platform-fixture").expect("fixture reference");
        let default = OpenAiPlatformRouteConfig::new(
            origin.clone(),
            reference.clone(),
            PlatformStoragePolicy::Stateless,
            PlatformProjectPolicy::DefaultProject,
        )
        .expect("default-project route");
        let project = OpenAiPlatformRouteConfig::new(
            origin,
            reference,
            PlatformStoragePolicy::StoreResponses,
            PlatformProjectPolicy::Project(
                OpenAiProjectId::new("proj_fixture").expect("project ID"),
            ),
        )
        .expect("project route");
        assert_ne!(default.account_binding(), project.account_binding());
        let encoded = serde_json::to_value(&project).expect("serialize route");
        assert_eq!(encoded["storage"], "store_responses");
        assert_eq!(encoded["project"]["project"], "proj_fixture");
        let reopened: OpenAiPlatformRouteConfig =
            serde_json::from_value(encoded).expect("deserialize exact route");
        assert_eq!(reopened, project);
    }

    #[test]
    fn project_identifier_rejects_ambiguous_header_values() {
        assert!(OpenAiProjectId::new("").is_err());
        assert!(OpenAiProjectId::new("proj_é").is_err());
        assert!(OpenAiProjectId::new("proj_fixture\nother").is_err());
        assert!(OpenAiProjectId::new("x".repeat(257)).is_err());
    }

    #[test]
    fn platform_request_stream_and_error_fixtures_are_exact_and_offline() {
        let request: serde_json::Value = serde_json::from_slice(include_bytes!(
            "../tests/fixtures/platform_request_gpt_56_sol.json"
        ))
        .expect("request fixture");
        assert_eq!(request["model"], "gpt-5.6-sol");
        assert_eq!(request["store"], false);
        assert_eq!(request["parallel_tool_calls"], false);
        assert_eq!(request["tools"][0]["strict"], false);
        assert_eq!(request["reasoning"]["effort"], "high");
        assert_eq!(request["reasoning"]["summary"], "auto");
        assert!(request.get("previous_response_id").is_none());

        let stream = include_str!("../tests/fixtures/platform_response_gpt_56_sol.sse");
        assert!(stream.contains("\"sequence_number\":0"));
        assert!(stream.contains("\"sequence_number\":4"));
        assert!(stream.contains("event: response.completed"));
        assert!(!stream.contains("[DONE]"));
        assert!(!stream.contains("gpt-test"));

        let error: serde_json::Value = serde_json::from_slice(include_bytes!(
            "../tests/fixtures/platform_error_invalid_request.json"
        ))
        .expect("error fixture");
        assert_eq!(error["error"]["type"], "invalid_request_error");
        assert_eq!(error["error"]["code"], "context_length_exceeded");
        assert_eq!(error["error"]["param"], "max_output_tokens");
    }
}

#[cfg(all(test, feature = "native-auth"))]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert exact artifact construction with expect and unwrap"
)]
mod tests {
    use std::collections::BTreeMap;
    use std::error::Error;

    use plakous_model::{
        ActiveFamilyArtifact, Actor, AttemptId, CallId, CanonicalHistory, Capability,
        CapabilityConstraints, CapabilitySet, ContentBlock, ConversationItem, ConversationView,
        DataClassification, Digest, DigestDomain, EffectiveProviderConfigurationId, EntryId,
        FactProvenance, FactSource, FamilyArtifactProducerFacts, FeatureSupport, FinishDetails,
        FinishReason, HistorySegment, HistorySegmentKind, ModelAdapterProfileId, ModelRequest,
        ModelResponse, OpaqueBlock, OpaqueFallback, ProtocolId, Provenance, ReplayEnvelope,
        ReplayKindId, ReplayScope, RequestMetadata, ResolvedModelDescriptor, RouteId, RunId,
        SealedFamilyArtifactCandidate, SelectedProviderConfigurationId, Support, TextBlock,
        TextPhase, Timestamp, ToolCallBlock, ToolCallState, ToolConcurrency, ToolCorrelation,
        ToolDefinition, ToolEffect, ToolExecutionOwner, ToolInputContract, ToolName,
        ToolOutputContract, ToolPresentationMode, ToolResultBlock, ToolResultKind, ToolResultPart,
        TurnId, ValidatedRequestPlan, ValidationMode, WorkingProjection,
    };
    use plakous_responses::{
        RESPONSES_ARTIFACT_CODEC_REVISION, RESPONSES_ARTIFACT_SCHEMA_REVISION, ResponsesArtifact,
        ResponsesArtifactBinding, ResponsesArtifactCodec, ResponsesArtifactProduction,
        ResponsesArtifactReplayKind, ResponsesReplayContract, ResponsesReplayItem,
        ResponsesReplayPurpose, ResponsesSemanticRevisions, ResponsesStorageMode,
    };
    use plakous_responses_dialect::{
        ResponsesReplayMode, ResponsesRequestAuthority, ResponsesRequestConfig,
        ResponsesStoragePolicy, lower_responses_request,
    };
    use serde_json::value::RawValue;

    use super::{
        CODEX_COMPLETE_INPUT_BYTES_PER_TOKEN, codex_artifact_replay_bridge,
        codex_complete_input_token_upper_bound, codex_reference_client_input_tokens,
        codex_request_policy, codex_tool_mode_accepts,
    };
    use crate::{
        CodexCatalogToolMode, OPENAI_CODEX_COMPATIBILITY_PROFILE_ID,
        OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID,
    };

    type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

    const NOW_SECONDS: u64 = 1_700_000_000;
    const HARNESS_PLAN_TAG: &[u8] = b"codex-artifact-model-test-plan";

    #[test]
    fn catalog_tool_mode_admits_only_its_exact_tool_shape() {
        let controls = ["exec", "wait"];
        let neighboring = ["exec_command", "write_stdin"];
        assert!(codex_tool_mode_accepts(
            Some(CodexCatalogToolMode::Direct),
            ToolPresentationMode::Direct,
            &neighboring
        ));
        assert!(!codex_tool_mode_accepts(
            Some(CodexCatalogToolMode::Direct),
            ToolPresentationMode::LocalCompositionOnly,
            &controls
        ));
        assert!(!codex_tool_mode_accepts(
            Some(CodexCatalogToolMode::CodeMode),
            ToolPresentationMode::LocalComposition,
            &controls
        ));
        assert!(!codex_tool_mode_accepts(
            Some(CodexCatalogToolMode::CodeMode),
            ToolPresentationMode::LocalComposition,
            &neighboring
        ));
        assert!(codex_tool_mode_accepts(
            Some(CodexCatalogToolMode::CodeModeOnly),
            ToolPresentationMode::LocalCompositionOnly,
            &controls
        ));
        assert!(codex_tool_mode_accepts(
            Some(CodexCatalogToolMode::CodeModeOnly),
            ToolPresentationMode::Direct,
            &neighboring
        ));
        // Plak js/py-ptc replaces the explicit Direct presentation on a
        // CodeModeOnly row; the outer markers are not `exec`/`wait`.
        for plak_ptc in [&["exec_js", "wait_js"][..], &["exec_py", "wait_py"][..]] {
            assert!(codex_tool_mode_accepts(
                Some(CodexCatalogToolMode::CodeModeOnly),
                ToolPresentationMode::Direct,
                plak_ptc
            ));
            assert!(codex_tool_mode_accepts(
                Some(CodexCatalogToolMode::Direct),
                ToolPresentationMode::Direct,
                plak_ptc
            ));
        }
        assert!(!codex_tool_mode_accepts(
            None,
            ToolPresentationMode::Direct,
            &neighboring
        ));
        assert!(!codex_tool_mode_accepts(
            None,
            ToolPresentationMode::LocalCompositionOnly,
            &controls
        ));
        assert!(!codex_tool_mode_accepts(
            Some(CodexCatalogToolMode::CodeModeOnly),
            ToolPresentationMode::LocalCompositionOnly,
            &["wait", "neighboring-model-exec"]
        ));
        assert!(!codex_tool_mode_accepts(
            Some(CodexCatalogToolMode::Direct),
            ToolPresentationMode::Direct,
            &["exec", "exec_command"]
        ));
        assert!(!codex_tool_mode_accepts(
            None,
            ToolPresentationMode::Direct,
            &["wait"]
        ));
        assert!(!codex_tool_mode_accepts(
            Some(CodexCatalogToolMode::CodeMode),
            ToolPresentationMode::LocalComposition,
            &["exec", "exec_command"]
        ));
        assert!(codex_tool_mode_accepts(
            Some(CodexCatalogToolMode::CodeMode),
            ToolPresentationMode::LocalComposition,
            &["exec", "wait", "exec_command"]
        ));
        assert!(!codex_tool_mode_accepts(
            Some(CodexCatalogToolMode::CodeModeOnly),
            ToolPresentationMode::Direct,
            &controls
        ));
    }

    struct TransitionFixture {
        first: ModelRequest,
        second: ModelRequest,
        first_output: ConversationItem,
        candidate: SealedFamilyArtifactCandidate,
        active: ActiveFamilyArtifact,
    }

    fn now() -> TestResult<Timestamp> {
        Ok(Timestamp::new(NOW_SECONDS, 0)?)
    }

    fn descriptor(route: &str) -> TestResult<ResolvedModelDescriptor> {
        let observed = now()?;
        let provenance = FactProvenance::new(FactSource::TestFixture, observed, None);
        let mut capabilities = BTreeMap::new();
        capabilities.insert(
            Capability::Streaming,
            FeatureSupport::new(
                Support::Native,
                CapabilityConstraints::default(),
                provenance,
            ),
        );
        let requested = plakous_model::ModelId::new("gpt-codex-artifact-test")?;
        Ok(ResolvedModelDescriptor::new(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            ProtocolId::new("openai.responses")?,
            RouteId::new(route)?,
            requested.clone(),
            Some(requested),
            None,
            ModelAdapterProfileId::new(OPENAI_CODEX_COMPATIBILITY_PROFILE_ID)?,
            CapabilitySet::new(capabilities, BTreeMap::new()),
        ))
    }

    fn metadata(attempt: &str) -> TestResult<RequestMetadata> {
        Ok(RequestMetadata::for_turn(
            "00000000-0000-7000-8000-000000000301".parse::<RunId>()?,
            "00000000-0000-7000-8000-000000000302".parse::<TurnId>()?,
            attempt.parse::<AttemptId>()?,
            now()?,
        ))
    }

    fn harness_plan_identity() -> TestResult<Digest> {
        Ok(Digest::sha256(
            DigestDomain::ResolvedHarnessPlan,
            1,
            &[HARNESS_PLAN_TAG],
        )?)
    }

    fn provider_configuration_ids() -> TestResult<(
        SelectedProviderConfigurationId,
        EffectiveProviderConfigurationId,
    )> {
        Ok((
            serde_json::from_value(serde_json::to_value(Digest::sha256(
                DigestDomain::SelectedProviderConfiguration,
                5,
                &[b"openai-model-test-selected-configuration"],
            )?)?)?,
            serde_json::from_value(serde_json::to_value(Digest::sha256(
                DigestDomain::EffectiveProviderConfiguration,
                5,
                &[b"openai-model-test-effective-configuration"],
            )?)?)?,
        ))
    }

    fn request(segments: Vec<HistorySegment>, attempt: &str) -> TestResult<ModelRequest> {
        let (selected, effective) = provider_configuration_ids()?;
        Ok(ModelRequest::builder(
            selected,
            effective,
            ConversationView::from_shared(
                CanonicalHistory::new(segments)?,
                WorkingProjection::default(),
            ),
            metadata(attempt)?,
        )
        .resolved_harness_plan_identity(harness_plan_identity()?)
        .build()?)
    }

    fn text_request(text: &str, attempt: &str, include_tool: bool) -> TestResult<ModelRequest> {
        let item = user("00000000-0000-7000-8000-000000000399", text)?;
        let (selected, effective) = provider_configuration_ids()?;
        let mut builder = ModelRequest::builder(
            selected,
            effective,
            ConversationView::from_items(vec![item])?,
            metadata(attempt)?,
        )
        .resolved_harness_plan_identity(harness_plan_identity()?);
        if include_tool {
            builder = builder.tools(vec![plakous_model::ConfiguredTool::new(
                ToolDefinition::new(
                    ToolName::new("lookup")?,
                    "1.0.0",
                    "Look up one Unicode value: 🦀",
                    ToolInputContract::structured(serde_json::json!({
                        "$schema": "https://json-schema.org/draft/2020-12/schema",
                        "type": "object",
                        "properties": {"q": {"type": "string"}},
                        "required": ["q"],
                        "additionalProperties": false
                    }))?,
                    ToolOutputContract::portable_model_content(),
                    ToolConcurrency::Serial,
                    ToolEffect::ReadOnly,
                )?,
                plakous_model::ToolExecutionDisposition::Local,
                plakous_model::ToolCallerDisposition::DirectOnly,
            )]);
        }
        Ok(builder.build()?)
    }

    fn user(id: &str, text: &str) -> TestResult<ConversationItem> {
        Ok(ConversationItem::new(
            id.parse::<EntryId>()?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new(text, None)?)],
            Provenance::UserInput,
        )?)
    }

    fn output_replay(id: &str, text: &str) -> TestResult<ReplayEnvelope> {
        let native = serde_json::json!({
            "id": id,
            "type": "message",
            "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": text, "annotations": []}]
        });
        Ok(ReplayEnvelope::new(
            ProtocolId::new("openai.responses")?,
            None,
            ReplayScope::ExactModel(plakous_model::ModelId::new("gpt-codex-artifact-test")?),
            ReplayKindId::new("responses.output_item")?,
            1,
            DataClassification::Sensitive,
            RawValue::from_string(native.to_string())?,
        )?)
    }

    fn assistant(id: &str, replay: ReplayEnvelope, text: &str) -> TestResult<ConversationItem> {
        Ok(ConversationItem::new(
            id.parse::<EntryId>()?,
            Actor::Assistant,
            vec![
                ContentBlock::Opaque(OpaqueBlock::new(replay, OpaqueFallback::CompatibleReplay)?),
                ContentBlock::Text(TextBlock::new(text, Some(TextPhase::Final))?),
            ],
            Provenance::ModelOutput,
        )?)
    }

    fn account_binding(route: &str) -> TestResult<Digest> {
        Ok(Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[b"codex-artifact-test-account", route.as_bytes()],
        )?)
    }

    fn plan(request: &ModelRequest, route: &str) -> TestResult<ValidatedRequestPlan> {
        Ok(descriptor(route)?.validate_request(request.clone(), ValidationMode::Strict)?)
    }

    fn authority(request: &ModelRequest, route: &str) -> TestResult<ResponsesRequestAuthority> {
        let descriptor = descriptor(route)?;
        let plan = descriptor.validate_request(request.clone(), ValidationMode::Strict)?;
        Ok(ResponsesRequestAuthority::from_admitted_operation(
            &descriptor,
            &plan,
            request,
            account_binding(route)?,
            ResponsesSemanticRevisions::new(4, 1, 4)?,
            Some(Digest::sha256(
                DigestDomain::CacheAffinity,
                1,
                &[b"codex-artifact-test-affinity"],
            )?),
            ResponsesStoragePolicy::Stateless,
        )?)
    }

    fn artifact(
        producing: &ModelRequest,
        covered: &ModelRequest,
        replay: ReplayEnvelope,
        tag: &[u8],
    ) -> TestResult<ResponsesArtifact> {
        let descriptor = descriptor(OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID)?;
        let plan = descriptor.validate_request(producing.clone(), ValidationMode::Strict)?;
        let covered_projection = covered
            .input()
            .canonical_item_prefix_identity(producing.input().items().len().saturating_add(1))?;
        let production = ResponsesArtifactProduction::from_admitted_response(
            producing,
            &plan,
            Digest::sha256(
                DigestDomain::ProviderOperation,
                1,
                &[b"codex-artifact-test-physical", tag],
            )?,
            Digest::sha256(
                DigestDomain::ProviderOperation,
                1,
                &[b"codex-artifact-test-terminal", tag],
            )?,
            1,
            covered_projection.identity(),
        )?;
        let contract = ResponsesReplayContract::new(ResponsesArtifactReplayKind::new(
            ReplayKindId::new("responses.output_item")?,
            1,
        )?);
        Ok(ResponsesArtifact::new(
            ResponsesArtifactBinding::from_admitted_operation(
                &descriptor,
                &plan,
                producing,
                account_binding(OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID)?,
                ResponsesSemanticRevisions::new(4, 1, 4)?,
                Some(Digest::sha256(
                    DigestDomain::CacheAffinity,
                    1,
                    &[b"codex-artifact-test-affinity"],
                )?),
            )?,
            production,
            contract,
            vec![ResponsesReplayItem::new(
                ResponsesReplayPurpose::OutputItem,
                0,
                replay,
            )],
            None,
            None,
            ResponsesStorageMode::Stateless,
            None,
            now()?,
        )?)
    }

    fn candidate(artifact: &ResponsesArtifact) -> TestResult<SealedFamilyArtifactCandidate> {
        let binding = artifact.binding();
        let production = artifact.production();
        let producer = FamilyArtifactProducerFacts::new(
            production.producer_request(),
            binding.model_input(),
            production.validated_plan(),
            binding.cache_affinity(),
            binding.run(),
            binding.root(),
            binding.attempt(),
            production.producing_projection(),
            production.producing_projection_items(),
            production.output_items(),
            production.covered_projection(),
            production.covered_projection_items(),
            production.physical_request(),
            production.terminal_response(),
        )?;
        Ok(SealedFamilyArtifactCandidate::new(
            "responses",
            RESPONSES_ARTIFACT_SCHEMA_REVISION,
            RESPONSES_ARTIFACT_CODEC_REVISION,
            binding.identity(),
            producer,
            ResponsesArtifactCodec::encode(artifact)?,
        )?)
    }

    fn active(candidate: &SealedFamilyArtifactCandidate) -> TestResult<ActiveFamilyArtifact> {
        Ok(ActiveFamilyArtifact::new(
            candidate.family(),
            candidate.schema_revision(),
            candidate.codec_revision(),
            candidate.binding(),
            candidate.bytes().to_vec(),
        )?)
    }

    fn transition_fixture() -> TestResult<TransitionFixture> {
        let first_user = user("00000000-0000-7000-8000-000000000310", "first")?;
        let first_segment =
            HistorySegment::new(HistorySegmentKind::RecentIntent, vec![first_user])?;
        let first = request(
            vec![first_segment.clone()],
            "00000000-0000-7000-8000-000000000311",
        )?;
        let replay = output_replay("codex-output-1", "answer one")?;
        let first_output = assistant(
            "00000000-0000-7000-8000-000000000312",
            replay.clone(),
            "answer one",
        )?;
        let output_segment = HistorySegment::new(
            HistorySegmentKind::ReasoningReplay,
            vec![first_output.clone()],
        )?;
        let second = request(
            vec![
                first_segment,
                output_segment,
                HistorySegment::new(
                    HistorySegmentKind::RecentIntent,
                    vec![user("00000000-0000-7000-8000-000000000313", "second")?],
                )?,
            ],
            "00000000-0000-7000-8000-000000000314",
        )?;
        let candidate = candidate(&artifact(&first, &second, replay, b"one")?)?;
        let active = active(&candidate)?;
        Ok(TransitionFixture {
            first,
            second,
            first_output,
            candidate,
            active,
        })
    }

    fn lower(
        request: &ModelRequest,
        bridge: Option<&plakous_responses_dialect::ResponsesArtifactReplayBridge>,
    ) -> TestResult<plakous_responses_dialect::LoweredResponsesRequest> {
        let descriptor = descriptor(OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID)?;
        Ok(lower_responses_request(
            request,
            ResponsesRequestConfig::new(
                descriptor.protocol(),
                descriptor.model_family(),
                descriptor.requested_model(),
                codex_request_policy(&descriptor, false),
                bridge.map(plakous_responses_dialect::ResponsesArtifactReplayBridge::context),
            ),
        )?)
    }

    /// A compaction beside a live artifact prepares fresh instead of failing.
    ///
    /// The Codex Responses route refused the whole preparation when the active
    /// artifact covered a different canonical prefix, so a compaction fold over
    /// a sealed cut could never be prepared while a turn's artifact was live.
    /// The live Luna growth case lost five compactions to it.
    #[test]
    fn a_compaction_request_prepares_fresh_beside_a_live_artifact() -> TestResult {
        let fixture = transition_fixture()?;

        // The compaction's own request: a different canonical prefix from the
        // one the live artifact continues.
        let fold = request(
            vec![HistorySegment::new(
                HistorySegmentKind::RecentIntent,
                vec![user(
                    "00000000-0000-7000-8000-000000000315",
                    "fold the cut",
                )?],
            )?],
            "00000000-0000-7000-8000-000000000316",
        )?;
        let fold_authority = authority(&fold, OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID)?;
        let fold_bridge =
            codex_artifact_replay_bridge(Some(&fixture.active), &fold, fold_authority, now()?)?;
        assert!(
            fold_bridge.is_none(),
            "the compaction must prepare fresh, not refuse"
        );
        // And the wire it lowers to continues nothing: a sealed cut carries its
        // own coverage and asks the route to replay none of the live turn's.
        let fold_wire = lower(&fold, fold_bridge.as_ref())?;
        assert_eq!(fold_wire.replay_mode(), &ResponsesReplayMode::Full);
        assert!(
            fold_wire.body().get("previous_response_id").is_none(),
            "a compaction request must not continue a previous response"
        );
        assert_eq!(fold_wire.body()["input"].as_array().map(Vec::len), Some(1));

        // And the live turn still reuses its own artifact afterwards.
        let second_authority = authority(&fixture.second, OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID)?;
        assert!(
            codex_artifact_replay_bridge(
                Some(&fixture.active),
                &fixture.second,
                second_authority,
                now()?,
            )?
            .is_some(),
            "the live turn's own continuation still reuses its artifact"
        );
        Ok(())
    }

    #[test]
    fn codex_stateless_first_second_third_and_reopen_context_are_exact() -> TestResult {
        let fixture = transition_fixture()?;
        let first_authority = authority(&fixture.first, OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID)?;
        let absent = codex_artifact_replay_bridge(None, &fixture.first, first_authority, now()?)?;
        assert!(absent.is_none());
        let first_wire = lower(&fixture.first, None)?;
        assert_eq!(first_wire.replay_mode(), &ResponsesReplayMode::Full);
        assert_eq!(first_wire.body()["input"].as_array().map(Vec::len), Some(1));
        assert!(first_wire.body().get("previous_response_id").is_none());

        let second_authority = authority(&fixture.second, OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID)?;
        assert_ne!(
            fixture.active.binding(),
            second_authority.artifact_binding().identity(),
            "the wrapper must preserve its producer binding"
        );
        let second_bridge = codex_artifact_replay_bridge(
            Some(&fixture.active),
            &fixture.second,
            second_authority,
            now()?,
        )?
        .ok_or("active first artifact was not bridged")?;
        let second_wire = lower(&fixture.second, Some(&second_bridge))?;
        assert_eq!(second_wire.replay_mode(), &ResponsesReplayMode::Full);
        assert!(
            second_wire.body()["input"]
                .as_array()
                .is_some_and(|input| input.iter().any(|item| item["id"] == "codex-output-1"))
        );
        assert!(second_wire.body().get("previous_response_id").is_none());

        let second_replay = output_replay("codex-output-2", "answer two")?;
        let second_output = assistant(
            "00000000-0000-7000-8000-000000000315",
            second_replay.clone(),
            "answer two",
        )?;
        let mut third_segments = fixture.second.input().history().segments().to_vec();
        third_segments.push(HistorySegment::new(
            HistorySegmentKind::ReasoningReplay,
            vec![second_output],
        )?);
        third_segments.push(HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![user("00000000-0000-7000-8000-000000000316", "third")?],
        )?);
        let third = request(third_segments, "00000000-0000-7000-8000-000000000317")?;
        let second_candidate =
            candidate(&artifact(&fixture.second, &third, second_replay, b"two")?)?;
        let reopened: ActiveFamilyArtifact =
            serde_json::from_value(serde_json::to_value(active(&second_candidate)?)?)?;
        let third_bridge = codex_artifact_replay_bridge(
            Some(&reopened),
            &third,
            authority(&third, OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID)?,
            now()?,
        )?
        .ok_or("reopened second artifact was not bridged")?;
        let third_wire = lower(&third, Some(&third_bridge))?;
        let third_input = third_wire.body()["input"]
            .as_array()
            .ok_or("third request input was not an array")?;
        assert!(
            third_input
                .iter()
                .any(|item| item["id"] == "codex-output-1")
        );
        assert!(
            third_input
                .iter()
                .any(|item| item["id"] == "codex-output-2")
        );
        assert!(third_wire.body().get("previous_response_id").is_none());
        Ok(())
    }

    #[test]
    fn codex_complete_input_bound_covers_minimal_unicode_tools_and_replay() -> TestResult {
        let minimal = text_request("x", "00000000-0000-7000-8000-000000000390", false)?;
        let unicode = text_request(
            "é中🦀\\\"\\n",
            "00000000-0000-7000-8000-000000000391",
            false,
        )?;
        let tool = text_request("tool input", "00000000-0000-7000-8000-000000000392", true)?;
        let fixture = transition_fixture()?;
        let replay_bridge = codex_artifact_replay_bridge(
            Some(&fixture.active),
            &fixture.second,
            authority(&fixture.second, OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID)?,
            now()?,
        )?
        .ok_or("replay fixture did not produce a bridge")?;
        let bodies = [
            serde_json::to_vec(lower(&minimal, None)?.body())?,
            serde_json::to_vec(lower(&unicode, None)?.body())?,
            serde_json::to_vec(lower(&tool, None)?.body())?,
            serde_json::to_vec(lower(&fixture.second, Some(&replay_bridge))?.body())?,
        ];
        for body in bodies {
            let wire_size = u64::try_from(body.len())?;
            let bound = codex_complete_input_token_upper_bound(wire_size)
                .ok_or("complete-input bound overflowed")?;
            let expected = wire_size.saturating_add(CODEX_COMPLETE_INPUT_BYTES_PER_TOKEN - 1)
                / CODEX_COMPLETE_INPUT_BYTES_PER_TOKEN;
            assert_eq!(bound, expected);
            assert_eq!(
                codex_reference_client_input_tokens(&body).ok_or("reference-client bound")?,
                bound,
                "bodies without compaction items keep the ordinary ceil(bytes/4) envelope"
            );
            assert!(bound <= wire_size, "bytes/4 must not exceed wire bytes");
            assert!(bound >= wire_size / CODEX_COMPLETE_INPUT_BYTES_PER_TOKEN);
        }
        Ok(())
    }

    #[test]
    fn codex_complete_input_bound_exact_fit_one_overflow_and_arithmetic_failure() -> TestResult {
        let request = text_request("boundary", "00000000-0000-7000-8000-000000000393", true)?;
        let body = serde_json::to_vec(lower(&request, None)?.body())?;
        let measured = codex_complete_input_token_upper_bound(u64::try_from(body.len())?)
            .ok_or("complete-input bound overflowed")?;
        let exact_envelope = measured;
        assert!(measured <= exact_envelope, "exact fit must be admitted");
        assert_eq!(
            measured.checked_sub(exact_envelope - 1),
            Some(1),
            "a one-token-smaller envelope must overflow by exactly one"
        );
        assert!(codex_complete_input_token_upper_bound(u64::MAX).is_none());
        Ok(())
    }

    #[test]
    fn codex_complete_input_bound_is_ceil_bytes_div_4() {
        assert_eq!(codex_complete_input_token_upper_bound(0), Some(0));
        assert_eq!(codex_complete_input_token_upper_bound(1), Some(1));
        assert_eq!(codex_complete_input_token_upper_bound(4), Some(1));
        assert_eq!(codex_complete_input_token_upper_bound(5), Some(2));
        assert_eq!(codex_complete_input_token_upper_bound(40_000), Some(10_000));
    }

    #[test]
    fn codex_reference_client_discounts_compaction_ciphertext() -> TestResult {
        let ciphertext = "A".repeat(100_000);
        let body = serde_json::to_vec(&serde_json::json!({
            "model": "gpt-5.4",
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "continue"}]
                },
                {
                    "type": "compaction",
                    "encrypted_content": ciphertext
                }
            ],
            "stream": true
        }))?;
        let naive = codex_complete_input_token_upper_bound(u64::try_from(body.len())?)
            .ok_or("naive bound")?;
        let measured =
            codex_reference_client_input_tokens(&body).ok_or("reference-client bound")?;
        assert!(
            measured < naive,
            "compaction ciphertext must not be charged as ordinary JSON utf8/4 ({measured} < {naive})"
        );
        let visible = (100_000_u64 * 3 / 4).saturating_sub(650);
        let item = serde_json::json!({
            "type": "compaction",
            "encrypted_content": ciphertext
        });
        let item_len = u64::try_from(serde_json::to_vec(&item)?.len())?;
        let adjusted = u64::try_from(body.len())?
            .checked_sub(item_len)
            .ok_or("item larger than body")?
            .checked_add(visible)
            .ok_or("visible overflow")?;
        assert_eq!(
            measured,
            codex_complete_input_token_upper_bound(adjusted).ok_or("expected bound")?
        );
        Ok(())
    }

    #[test]
    fn codex_active_artifact_tamper_binding_revision_and_family_fail_closed() -> TestResult {
        let fixture = transition_fixture()?;
        let bytes = fixture.candidate.bytes().to_vec();
        let binding = fixture.candidate.binding();
        let mut tampered = bytes.clone();
        *tampered.last_mut().ok_or("empty artifact bytes")? ^= 1;
        let invalid = [
            ActiveFamilyArtifact::new(
                "messages",
                fixture.candidate.schema_revision(),
                fixture.candidate.codec_revision(),
                binding,
                bytes.clone(),
            )?,
            ActiveFamilyArtifact::new(
                "responses",
                fixture.candidate.schema_revision().saturating_add(1),
                fixture.candidate.codec_revision(),
                binding,
                bytes.clone(),
            )?,
            ActiveFamilyArtifact::new(
                "responses",
                fixture.candidate.schema_revision(),
                fixture.candidate.codec_revision().saturating_add(1),
                binding,
                bytes.clone(),
            )?,
            ActiveFamilyArtifact::new(
                "responses",
                fixture.candidate.schema_revision(),
                fixture.candidate.codec_revision(),
                Digest::sha256(
                    DigestDomain::ModelBinding,
                    1,
                    &[b"foreign-neutral-wrapper-binding"],
                )?,
                bytes,
            )?,
            ActiveFamilyArtifact::new(
                "responses",
                fixture.candidate.schema_revision(),
                fixture.candidate.codec_revision(),
                binding,
                tampered,
            )?,
        ];
        for artifact in &invalid {
            assert!(
                codex_artifact_replay_bridge(
                    Some(artifact),
                    &fixture.second,
                    authority(&fixture.second, OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID)?,
                    now()?,
                )
                .is_err()
            );
        }
        assert!(
            codex_artifact_replay_bridge(
                Some(&fixture.active),
                &fixture.second,
                authority(&fixture.second, "codex-foreign-route")?,
                now()?,
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn codex_terminal_uses_sidecar_and_only_item_level_replay() -> TestResult {
        let fixture = transition_fixture()?;
        let first_plan = plan(&fixture.first, OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID)?;
        let covered = first_plan
            .request()
            .input()
            .staged_canonical_item_prefix_identity(std::slice::from_ref(&fixture.first_output))?;
        fixture
            .candidate
            .validate_for_commit(&first_plan, covered, 1)?;
        let response = ModelResponse::new(
            vec![fixture.first_output],
            FinishDetails::new(FinishReason::Stop, None),
            None,
            Some("codex-response-sidecar".to_owned()),
            first_plan.digest(),
        )
        .with_family_artifact_candidate(fixture.candidate)?;
        assert!(response.family_artifact_candidate().is_some());
        assert!(
            !serde_json::to_value(&response)?
                .as_object()
                .is_some_and(|object| object.contains_key("replay"))
        );
        let replay_kinds = response
            .output()
            .iter()
            .flat_map(ConversationItem::blocks)
            .filter_map(|block| match block {
                ContentBlock::Opaque(opaque) => Some(opaque.replay().kind().as_str()),
                ContentBlock::Reasoning(reasoning) => {
                    reasoning.replay().map(|replay| replay.kind().as_str())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(replay_kinds, vec!["responses.output_item"]);
        let encoded = serde_json::to_string(&response)?;
        assert!(!encoded.contains("responses.artifact"));
        assert!(!encoded.contains("\"encoding\":\"base64\""));
        Ok(())
    }

    #[test]
    fn codex_tool_output_coverage_is_open_until_the_result_arrives() -> TestResult {
        let intent = HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![user(
                "00000000-0000-7000-8000-000000000320",
                "use the tool",
            )?],
        )?;
        let first = request(vec![intent.clone()], "00000000-0000-7000-8000-000000000321")?;
        let first_plan = plan(&first, OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID)?;
        let correlation = ToolCorrelation::new(
            Some("codex-call-1".to_owned()),
            None,
            plakous_model::ToolCallerLineage::Direct,
        )?;
        let call_id = "00000000-0000-7000-8000-000000000322".parse::<CallId>()?;
        let call = ToolCallBlock::new(
            call_id,
            ToolName::new("lookup")?,
            ToolExecutionOwner::Local,
            ToolCallState::Requested,
            correlation.clone(),
            r#"{"q":"fixture"}"#,
        )?;
        let call_item = ConversationItem::new(
            "00000000-0000-7000-8000-000000000323".parse::<EntryId>()?,
            Actor::Assistant,
            vec![ContentBlock::ToolCall(call)],
            Provenance::ModelOutput,
        )?;
        let covered = first_plan
            .request()
            .input()
            .staged_canonical_item_prefix_identity(std::slice::from_ref(&call_item))?;
        let result = ToolResultBlock::new(
            call_id,
            ToolExecutionOwner::Local,
            ToolResultKind::Success,
            vec![ToolResultPart::Text(TextBlock::new(
                "fixture result",
                None,
            )?)],
            correlation,
        )?;
        let result_item = ConversationItem::new(
            "00000000-0000-7000-8000-000000000324".parse::<EntryId>()?,
            Actor::Tool,
            vec![ContentBlock::ToolResult(result)],
            Provenance::ToolExecution,
        )?;
        let exchange = HistorySegment::new(
            HistorySegmentKind::ToolExchange,
            vec![call_item, result_item],
        )?;
        let next = ConversationView::from_shared(
            CanonicalHistory::new(vec![intent, exchange])?,
            WorkingProjection::default(),
        );
        assert_eq!(covered, next.canonical_item_prefix_identity(2)?);
        Ok(())
    }
}
