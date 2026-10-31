//! Exact OpenAI Platform execution through the shared Responses host.

use std::sync::Arc;

use plakous_attempt::LedgerPolicy;
use plakous_http::{
    Client, HeaderMap, HeaderName, HeaderValue, Method, Origin, Request, SensitiveBytes,
    sensitive_header_value,
};
use plakous_model::{
    Clock, CompiledProviderConfiguration, LanguageModel, ModelPreparationContext,
    ModelPreparationError, ModelPreparationErrorKind, PreparedModelCall, ProviderSemanticEvidence,
    ReplayKindId, ResolvedModelDescriptor, ResolvedProviderAuthorityBinding, RuntimeModelAuthority,
    RuntimeModelAuthorityError, RuntimeModelAuthorityOperation, SelectedProviderConfiguration,
    Timestamp, ValidatedRequestPlan,
};
use plakous_responses::api_key::{
    Preparation, Wire, artifact_cache, configuration_matches, dispatch_available,
    fingerprint_matches, prepare, request_authority, request_matches_configuration, seal,
    stateless_stream_body,
};
use plakous_responses::{
    LifecycleDialectPolicy, ResponsesDialectProfile, ResponsesSemanticRevisions,
    ResponsesStorageMode,
};
use plakous_responses::{
    ResponsesAttemptDialectData, ResponsesAttemptService, ResponsesAttemptTarget,
    ResponsesAuthSlots, ResponsesAuthorizationAdapter, ResponsesCanonicalDialectData,
    ResponsesDispatchRejection, ResponsesPreflightFuture, ResponsesPreflightRequest,
    ResponsesProviderErrorPolicy, ResponsesReplayKind, ResponsesReplayProjection,
    ResponsesResponseLimits, ResponsesRevalidationRequest, ResponsesStreamCapabilities,
    ResponsesStreamDialectData, ResponsesUsagePolicy, canonical_local_tools,
};
use thiserror::Error;

use crate::{
    OPENAI_RESPONSES_ADAPTER_REVISION, OpenAiKeyGeneration, OpenAiPlatformConfigurationAuthority,
    PlatformProjectPolicy, StandardOpenAiHttpConfig, compile_openai_platform_configuration,
    config::OPENAI_PLATFORM_RESPONSES_PATH, config::OPENAI_RESPONSES_DIALECT_REVISION,
    openai_platform_request_policy,
};

const PREFLIGHT_REVISION: u32 = 1;
const FINAL_AUTHORIZATION_REVISION: u32 = 1;
const PLATFORM_PROJECTION_REVISION: &str = "responses-canonical-openai-platform-v1";
const PLATFORM_ACCOUNTING_REVISION: &str = "responses-wire-bytes-openai-platform-v1";
const HEADER_ACCEPT: HeaderName = HeaderName::from_static("accept");
const HEADER_CONTENT_TYPE: HeaderName = HeaderName::from_static("content-type");
const HEADER_AUTHORIZATION: HeaderName = HeaderName::from_static("authorization");
const HEADER_PROJECT: HeaderName = HeaderName::from_static("openai-project");
const HEADER_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

struct PlatformExecution {
    authority: OpenAiPlatformConfigurationAuthority,
    client: Client,
    dial_origin: Origin,
    generation: OpenAiKeyGeneration,
}

struct PlatformDispatchGuard {
    generation: OpenAiKeyGeneration,
}

/// Exact Platform API-key model using stateless Responses and local replay.
pub struct OpenAiPlatformModel {
    probe: plakous_model::ReachabilityProbe,
    authority: OpenAiPlatformConfigurationAuthority,
    config: StandardOpenAiHttpConfig,
    attempt_data: ResponsesAttemptDialectData,
    service: Arc<ResponsesAttemptService<PlatformExecution, PlatformDispatchGuard>>,
    descriptor: ResolvedModelDescriptor,
    configuration: CompiledProviderConfiguration,
    generation: OpenAiKeyGeneration,
    clock: Arc<dyn Clock>,
    client: Client,
    dial_origin: Origin,
}

impl OpenAiPlatformModel {
    /// Constructs the official Platform executor. Caller context bounds must be explicit.
    pub fn new(
        authority: OpenAiPlatformConfigurationAuthority,
        selected: SelectedProviderConfiguration,
    ) -> Result<Self, OpenAiPlatformBuildError> {
        Self::with_exchange_observer(authority, selected, None)
    }

    /// Constructs official execution with the existing header-free exchange observer.
    pub fn with_exchange_observer(
        authority: OpenAiPlatformConfigurationAuthority,
        selected: SelectedProviderConfiguration,
        observer: Option<Arc<dyn plakous_http::ExchangeObserver>>,
    ) -> Result<Self, OpenAiPlatformBuildError> {
        if authority.is_local_fixture()
            || authority.target().route().origin().as_str() != crate::OPENAI_PLATFORM_ORIGIN
        {
            return Err(OpenAiPlatformBuildError::NonProductionAuthority);
        }
        if authority.descriptor().context_contract().is_none() {
            return Err(OpenAiPlatformBuildError::Configuration);
        }
        let client = Client::new(plakous_http::Limits::for_streaming_inference())
            .map_err(|_| OpenAiPlatformBuildError::SharedBinding)?
            .pooled()
            .with_observer(observer);
        Self::assemble(authority, selected, client, None)
    }

    /// Runs the production executor against explicitly sealed HTTPS loopback transport.
    /// Credential resolution remains bound to the official Platform origin.
    #[doc(hidden)]
    pub fn with_local_transport(
        authority: OpenAiPlatformConfigurationAuthority,
        selected: SelectedProviderConfiguration,
        client: Client,
        dial_origin: Origin,
    ) -> Result<Self, OpenAiPlatformBuildError> {
        let url = url::Url::parse(&format!("https://{}", dial_origin.binding_authority()))
            .map_err(|_| OpenAiPlatformBuildError::NonLocalAuthority)?;
        if url.scheme() != "https"
            || url.port().is_none()
            || !matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))
        {
            return Err(OpenAiPlatformBuildError::NonLocalAuthority);
        }
        if authority.is_local_fixture()
            || authority.target().route().origin().as_str() != crate::OPENAI_PLATFORM_ORIGIN
        {
            return Err(OpenAiPlatformBuildError::NonProductionAuthority);
        }
        if authority.descriptor().context_contract().is_none() {
            return Err(OpenAiPlatformBuildError::Configuration);
        }
        Self::assemble(authority, selected, client, Some(dial_origin))
    }

    /// Binds exact authority to the shared HTTP executor.
    #[allow(
        clippy::too_many_lines,
        reason = "Bind one Platform key generation to its reviewed target, configuration authority and transport before publishing the model."
    )]
    fn assemble(
        authority: OpenAiPlatformConfigurationAuthority,
        selected: SelectedProviderConfiguration,
        client: Client,
        dial_origin: Option<Origin>,
    ) -> Result<Self, OpenAiPlatformBuildError> {
        if authority.target().route().responses_path() != OPENAI_PLATFORM_RESPONSES_PATH {
            return Err(OpenAiPlatformBuildError::NonLocalAuthority);
        }
        let config = authority.http_config();
        let descriptor = authority.descriptor().clone();
        descriptor
            .preflight_context_admission_requirement(
                selected.behavior().context_evidence_requirement,
            )
            .map_err(|_| OpenAiPlatformBuildError::Configuration)?;
        crate::model::validate_openai_platform_selection(&selected, &authority)
            .map_err(|_| OpenAiPlatformBuildError::Configuration)?;
        let initial_generation = authority
            .target()
            .route()
            .platform_generation_grant()
            .map_err(|_| OpenAiPlatformBuildError::Configuration)?;
        let authority = authority
            .bind_initial_generation(initial_generation)
            .map_err(|_| OpenAiPlatformBuildError::Configuration)?;
        let descriptor = authority.descriptor().clone();
        let clock = authority.target().clock();
        let checked_at = clock.now();
        let runtime = fresh_runtime_authority(&authority, &descriptor, checked_at)?;
        let account_binding = runtime.account_binding();
        let configuration = compile_platform_configuration(&authority, &selected, &runtime)?;
        configuration
            .seal_runtime_authority(&descriptor, runtime, checked_at)
            .map_err(|_| OpenAiPlatformBuildError::Configuration)?;
        let authorization = ResponsesAuthorizationAdapter::new(
            PREFLIGHT_REVISION,
            FINAL_AUTHORIZATION_REVISION,
            platform_preflight,
            revalidate_platform,
        )
        .map_err(|_| OpenAiPlatformBuildError::SharedBinding)?;
        let target = ResponsesAttemptTarget::from_admitted_target(
            &descriptor,
            account_binding,
            semantic_revisions()?,
        )
        .map_err(|_| OpenAiPlatformBuildError::SharedBinding)?;
        let response_limits = ResponsesResponseLimits::default();
        let stream = ResponsesStreamDialectData::new(
            ResponsesDialectProfile::openai_platform(),
            LifecycleDialectPolicy::openai_platform_api(),
            descriptor.requested_model().as_str(),
            ResponsesUsagePolicy::DeriveTotalWhenOmitted,
            response_limits,
            ResponsesStreamCapabilities::codex(),
        );
        let replay = ResponsesReplayProjection::new(
            descriptor.protocol().clone(),
            descriptor.model_family().cloned(),
            descriptor.requested_model().clone(),
            ReplayKindId::new("responses.output_item")
                .map_err(|_| OpenAiPlatformBuildError::SharedBinding)?,
            1,
            account_binding,
        )
        .map_err(|_| OpenAiPlatformBuildError::SharedBinding)?;
        let replay = if authority.is_local_fixture() {
            replay
        } else {
            replay.requiring_encrypted_reasoning_with_optional_status()
        };
        let attempt_data = ResponsesAttemptDialectData::new(
            OPENAI_RESPONSES_DIALECT_REVISION,
            target,
            &client,
            &authorization,
            LedgerPolicy::OPENAI_STRICT,
            ResponsesAuthSlots::Primary,
            openai_platform_request_policy(&authority)
                .map_err(|_| OpenAiPlatformBuildError::SharedBinding)?,
            stream,
            config.responses_limits(),
            replay,
            ResponsesStorageMode::Stateless,
            ResponsesProviderErrorPolicy::OpenAiCompatible,
            response_limits,
            Some(HEADER_REQUEST_ID),
            true,
        )
        .map_err(|_| OpenAiPlatformBuildError::SharedBinding)?;
        let execution = Arc::new(PlatformExecution {
            generation: authority
                .target()
                .route()
                .platform_generation_grant()
                .map_err(|_| OpenAiPlatformBuildError::Configuration)?,
            dial_origin: dial_origin.unwrap_or(
                Origin::parse(authority.target().route().origin().as_str())
                    .map_err(|_| OpenAiPlatformBuildError::SharedBinding)?,
            ),
            authority: authority.clone(),
            client: client.clone(),
        });
        let probe = plakous_responses::bind_reachability_probe(
            client.clone(),
            execution.dial_origin.clone(),
            descriptor.route().clone(),
        );
        let side_client = client.clone();
        let side_origin = execution.dial_origin.clone();
        let service = Arc::new(
            ResponsesAttemptService::new(client, execution, attempt_data.clone(), authorization)
                .map_err(|_| OpenAiPlatformBuildError::SharedBinding)?,
        );
        let generation = authority
            .target()
            .route()
            .platform_generation_grant()
            .map_err(|_| OpenAiPlatformBuildError::Configuration)?;
        Ok(Self {
            probe,
            authority,
            config,
            attempt_data,
            service,
            descriptor,
            configuration,
            generation,
            clock,
            client: side_client,
            dial_origin: side_origin,
        })
    }

    /// The session's hosted web search side-request executor on this exact
    /// route, key generation, model and client, at `effort` (pass the lowest
    /// effort the model's ladder admits).
    #[must_use]
    pub fn retrieval(self: &Arc<Self>, effort: Option<&'static str>) -> crate::OpenAiRetrieval {
        crate::OpenAiRetrieval::new(Arc::clone(self), effort)
    }

    /// The session's image generation executor on this exact route, key
    /// and client.
    #[must_use]
    pub fn images(self: &Arc<Self>) -> crate::OpenAiImages {
        crate::OpenAiImages::new(Arc::clone(self))
    }

    pub(crate) const fn client(&self) -> &Client {
        &self.client
    }

    pub(crate) fn requested_model(&self) -> &str {
        self.descriptor.requested_model().as_str()
    }

    /// Revalidates admission exactly as before a model round, then builds
    /// the authorized Platform request carrying `body`.
    pub(crate) async fn side_request(
        self: &Arc<Self>,
        body: &[u8],
    ) -> Result<Request, &'static str> {
        self.side_request_to(OPENAI_PLATFORM_RESPONSES_PATH, "text/event-stream", body)
            .await
    }

    /// [`Self::side_request`] to another `path` of the same origin.
    pub(crate) async fn side_request_to(
        self: &Arc<Self>,
        path: &'static str,
        accept: &'static str,
        body: &[u8],
    ) -> Result<Request, &'static str> {
        Arc::clone(self)
            .revalidate_runtime_authority(self.clock.now())
            .await
            .map_err(|_| "OpenAI API admission is no longer current")?;
        let route = self.authority.target().route();
        let authorization = route
            .platform_bearer_header(&self.generation)
            .map_err(|_| "OpenAI API key generation is no longer current")?;
        let mut headers = HeaderMap::new();
        headers.insert(HEADER_ACCEPT, HeaderValue::from_static(accept));
        headers.insert(
            HEADER_CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        headers.insert(HEADER_AUTHORIZATION, authorization);
        if let PlatformProjectPolicy::Project(project) = self.authority.project() {
            headers.insert(
                HEADER_PROJECT,
                HeaderValue::from_str(project.as_str()).map_err(|_| "OpenAI project is invalid")?,
            );
        }
        Request::new(
            self.dial_origin.clone(),
            Method::POST,
            path,
            headers,
            SensitiveBytes::new(body.to_vec()).into_bytes(),
            self.client.limits(),
        )
        .map_err(|_| "OpenAI side request could not be built")
    }

    /// Joins the executable model with its sealed configuration and fresh runtime authority.
    pub async fn configured(
        self: Arc<Self>,
    ) -> Result<plakous_model::ConfiguredLanguageModel, crate::OpenAiPlatformResolveError> {
        let authority = self
            .clone()
            .revalidate_runtime_authority(self.clock.now())
            .await?;
        Ok(plakous_model::ConfiguredLanguageModel::new(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            self.clone(),
            self.configuration.clone(),
            authority,
        )?)
    }

    /// Returns the exact selected-to-effective configuration used by execution.
    #[must_use]
    pub const fn configuration(&self) -> &CompiledProviderConfiguration {
        &self.configuration
    }
}

impl LanguageModel for OpenAiPlatformModel {
    fn reachability_probe(&self) -> Option<plakous_model::ReachabilityProbe> {
        Some(self.probe.clone())
    }

    fn revalidate_runtime_authority(
        self: Arc<Self>,
        checked_at: Timestamp,
    ) -> RuntimeModelAuthorityOperation {
        Box::pin(async move {
            self.authority
                .target()
                .validate_current_route_evidence()
                .map_err(|_| RuntimeModelAuthorityError::Stale)?;
            self.authority
                .target()
                .route()
                .revalidate_platform_generation(&self.generation)
                .map_err(|_| RuntimeModelAuthorityError::ForeignAuthority)?;
            let runtime = fresh_runtime_authority(&self.authority, &self.descriptor, checked_at)
                .map_err(|_| RuntimeModelAuthorityError::Unavailable)?;
            let current = compile_platform_configuration(
                &self.authority,
                self.configuration.selected(),
                &runtime,
            )
            .map_err(|_| RuntimeModelAuthorityError::ForeignAuthority)?;
            if current != self.configuration {
                return Err(RuntimeModelAuthorityError::ForeignAuthority);
            }
            self.configuration
                .seal_runtime_authority(&self.descriptor, runtime.clone(), checked_at)
                .map_err(|_| RuntimeModelAuthorityError::ForeignAuthority)?;
            Ok(runtime)
        })
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Validate exact Platform authority and seal the complete Responses wire and context projection without dispatch."
    )]
    fn prepare_call(
        self: Arc<Self>,
        plan: ValidatedRequestPlan,
        context: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError> {
        self.authority
            .target()
            .validate_current_route_evidence()
            .map_err(|_| rejection("Platform route evidence is stale"))?;
        self.authority
            .target()
            .validate_plan(&plan)
            .map_err(|_| rejection("Platform plan belongs to another target"))?;
        if !configuration_matches(&plan, &self.configuration) {
            return Err(rejection(
                "Platform request provider configuration is foreign",
            ));
        }
        if !request_matches_configuration(plan.request(), self.configuration.selected().behavior())
        {
            return Err(rejection(
                "local request behavior differs from compiled configuration",
            ));
        }
        self.descriptor
            .validate_plan_admission(plan.request(), &plan)?;
        self.authority
            .target()
            .validate_request_intent(plan.request())
            .map_err(|_| rejection("Platform request intent is not admitted"))?;
        context
            .authority()
            .validate(&self.descriptor, context.prepared_at())?;
        if context.authority().account_binding()
            != self.configuration.effective().authority().account_binding()
            || context.authority().credential_authority_binding()
                != self
                    .configuration
                    .effective()
                    .authority()
                    .credential_authority_binding()
        {
            return Err(RuntimeModelAuthorityError::ForeignAuthority.into());
        }
        let request = plan.request();
        let canonical_tools = canonical_local_tools(request);
        let canonical = ResponsesCanonicalDialectData::openai_platform(
            self.descriptor.requested_model().clone(),
            canonical_tools,
        )
        .map_err(|_| rejection("Platform canonical projection is invalid"))?;
        let authority = request_authority(
            &self.descriptor,
            &plan,
            &context,
            semantic_revisions().map_err(|_| rejection("Platform revisions are invalid"))?,
        )
        .map_err(|_| rejection("Platform request authority is invalid"))?;
        prepare(Preparation {
            label: "Platform",
            descriptor: &self.descriptor,
            plan,
            context: &context,
            attempt: &self.attempt_data,
            canonical,
            authority,
            output_item_replay: ResponsesReplayKind::new("responses.output_item", 1),
            previous_response_replay: Some(ResponsesReplayKind::new(
                "openai.previous_response_id",
                1,
            )),
            adapter_revision: OPENAI_RESPONSES_ADAPTER_REVISION,
            projection_revision: PLATFORM_PROJECTION_REVISION,
            accounting_revision: PLATFORM_ACCOUNTING_REVISION,
            cache_use: artifact_cache(&context),
            observation_capacity: self.config.event_channel_capacity(),
            clock: Arc::clone(&self.clock),
            service: Arc::clone(&self.service),
            operation_timeout: self.config.operation_timeout(),
        })
    }
}

/// Explicit sealed HTTPS loopback fixture using the production Platform executor.
#[doc(hidden)]
pub struct OpenAiPlatformLocalModel {
    inner: Arc<OpenAiPlatformModel>,
}
impl OpenAiPlatformLocalModel {
    /// Constructs an injected-client fixture without granting production-origin authority.
    pub fn new(
        authority: OpenAiPlatformConfigurationAuthority,
        selected: SelectedProviderConfiguration,
        client: Client,
    ) -> Result<Self, OpenAiPlatformBuildError> {
        if !authority.is_local_fixture() {
            return Err(OpenAiPlatformBuildError::NonLocalAuthority);
        }
        Ok(Self {
            inner: Arc::new(OpenAiPlatformModel::assemble(
                authority, selected, client, None,
            )?),
        })
    }
    /// Returns the exact configured behavior.
    #[must_use]
    pub fn configuration(&self) -> &CompiledProviderConfiguration {
        self.inner.configuration()
    }
}
impl LanguageModel for OpenAiPlatformLocalModel {
    fn reachability_probe(&self) -> Option<plakous_model::ReachabilityProbe> {
        self.inner.reachability_probe()
    }

    fn revalidate_runtime_authority(
        self: Arc<Self>,
        checked_at: Timestamp,
    ) -> RuntimeModelAuthorityOperation {
        self.inner.clone().revalidate_runtime_authority(checked_at)
    }
    fn prepare_call(
        self: Arc<Self>,
        plan: ValidatedRequestPlan,
        context: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError> {
        self.inner.clone().prepare_call(plan, context)
    }
}

fn platform_preflight<'a>(
    execution: &'a PlatformExecution,
    request: ResponsesPreflightRequest<'a>,
) -> ResponsesPreflightFuture<'a, PlatformDispatchGuard> {
    Box::pin(async move {
        dispatch_available(request.cancellation().is_cancelled(), request.deadline())?;
        let authority = &execution.authority;
        authority
            .target()
            .validate_current_route_evidence()
            .map_err(|_| ResponsesDispatchRejection::Binding)?;
        stateless_stream_body(
            request.body_bytes(),
            Some(authority.descriptor().requested_model().as_str()),
        )?;
        let generation = authority
            .target()
            .route()
            .platform_generation_grant()
            .map_err(|_| ResponsesDispatchRejection::Authentication)?;
        if generation != execution.generation {
            return Err(ResponsesDispatchRejection::Authentication);
        }
        let authorization = if authority.is_local_fixture() {
            sensitive_header_value(b"Bearer plakous-openai-platform-local-fixture".to_vec())
                .map_err(|_| ResponsesDispatchRejection::Binding)?
        } else {
            authority
                .target()
                .route()
                .platform_bearer_header(&generation)
                .map_err(|_| ResponsesDispatchRejection::Authentication)?
        };
        let mut extra_headers = Vec::new();
        if let PlatformProjectPolicy::Project(project) = authority.project() {
            extra_headers.push((HEADER_PROJECT, project.as_str().as_bytes().to_vec()));
        }
        seal(Wire {
            origin: execution.dial_origin.clone(),
            path: OPENAI_PLATFORM_RESPONSES_PATH,
            authorization,
            extra_headers,
            body: request.body_bytes().to_vec(),
            limits: execution.client.limits().clone(),
            fingerprint: generation.fingerprint(),
            physical: request.physical().clone(),
            guard: PlatformDispatchGuard { generation },
        })
    })
}

fn revalidate_platform(
    execution: &PlatformExecution,
    mut request: ResponsesRevalidationRequest<'_, PlatformDispatchGuard>,
) -> Result<(), ResponsesDispatchRejection> {
    dispatch_available(request.cancellation().is_cancelled(), request.deadline())?;
    let generation = request.response_head_guard_mut().generation.clone();
    execution
        .authority
        .target()
        .route()
        .revalidate_platform_generation(&generation)
        .map_err(|_| ResponsesDispatchRejection::Authentication)?;
    fingerprint_matches(
        request.binding().credential_authority(),
        generation.fingerprint(),
    )
}

fn semantic_revisions() -> Result<ResponsesSemanticRevisions, OpenAiPlatformBuildError> {
    ResponsesSemanticRevisions::new(1, 1, 1).map_err(|_| OpenAiPlatformBuildError::SharedBinding)
}

fn fresh_runtime_authority(
    authority: &OpenAiPlatformConfigurationAuthority,
    descriptor: &ResolvedModelDescriptor,
    checked_at: Timestamp,
) -> Result<RuntimeModelAuthority, OpenAiPlatformBuildError> {
    let grant = authority
        .target()
        .route()
        .platform_generation_grant()
        .map_err(|_| OpenAiPlatformBuildError::Configuration)?;
    RuntimeModelAuthority::new(
        descriptor,
        authority.target().account_binding(),
        authority.target().credential_authority_binding(),
        generation_number(&grant),
        checked_at,
        descriptor
            .context_contract()
            .and_then(|contract| contract.capacity().freshness().expires_at()),
    )
    .map_err(|_| OpenAiPlatformBuildError::Configuration)
}

fn compile_platform_configuration(
    authority: &OpenAiPlatformConfigurationAuthority,
    selected: &SelectedProviderConfiguration,
    runtime: &RuntimeModelAuthority,
) -> Result<CompiledProviderConfiguration, OpenAiPlatformBuildError> {
    let descriptor = authority.descriptor();
    let mut semantic_bytes = serde_json::to_vec(&(
        "openai-platform-semantic-evidence-v2",
        authority
            .target()
            .route()
            .platform_generation_grant()
            .map_err(|_| OpenAiPlatformBuildError::Configuration)?
            .fingerprint()
            .as_bytes(),
        authority.target().route().id(),
        authority.target().route().origin(),
        descriptor.requested_model(),
        descriptor.reported_model(),
        descriptor.model_family(),
        descriptor.profile(),
        descriptor
            .context_contract()
            .map(plakous_model::ContextContract::semantic_identity),
        OPENAI_RESPONSES_ADAPTER_REVISION,
        OPENAI_RESPONSES_DIALECT_REVISION,
    ))
    .map_err(|_| OpenAiPlatformBuildError::Configuration)?;
    // The caller's admitted-facts binding is opaque: it only makes the facts
    // the session was admitted under identity-bearing. Empty leaves the
    // previous evidence bytes unchanged.
    semantic_bytes.extend_from_slice(authority.admitted_facts());
    let evidence = ProviderSemanticEvidence::new(1, semantic_bytes)
        .map_err(|_| OpenAiPlatformBuildError::Configuration)?;
    let stable = ResolvedProviderAuthorityBinding::new(descriptor, runtime, evidence)
        .map_err(|_| OpenAiPlatformBuildError::Configuration)?;
    compile_openai_platform_configuration(selected, authority, stable)
        .map_err(OpenAiPlatformBuildError::Policy)
}

fn generation_number(grant: &OpenAiKeyGeneration) -> u64 {
    let fingerprint = grant.fingerprint();
    u64::from_be_bytes(fingerprint.as_bytes()[..8].try_into().unwrap_or([0; 8])).max(1)
}

fn rejection(message: &str) -> ModelPreparationError {
    ModelPreparationError::rejected(ModelPreparationErrorKind::InvalidConfiguration, message)
}

/// Invalid Platform shared-host composition.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum OpenAiPlatformBuildError {
    /// Authority was not minted for the official Platform origin.
    #[error("OpenAI Platform production execution requires official origin authority")]
    NonProductionAuthority,
    /// Authority was not minted by the exact loopback constructor.
    #[error("OpenAI Platform local execution requires sealed HTTPS loopback authority")]
    NonLocalAuthority,
    /// The selected provider behavior is not admitted.
    #[error(transparent)]
    Policy(#[from] plakous_responses::ResponsesProviderConfigurationError),
    /// Complete selected/effective configuration or fresh authority disagreed.
    #[error("OpenAI Platform provider configuration is invalid")]
    Configuration,
    /// Shared Responses construction facts were inconsistent.
    #[error("OpenAI Platform shared Responses construction failed")]
    SharedBinding,
}
