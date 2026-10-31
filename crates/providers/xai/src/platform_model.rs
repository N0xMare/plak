//! API-key Platform executor through the shared Responses attempt service.

use crate::{
    XAI_PLATFORM_API_KEY_ORIGIN, XAI_PLATFORM_ENCRYPTED_REASONING_INCLUDE,
    XAI_PLATFORM_EXECUTION_REVISION, XAI_PLATFORM_RESPONSES_PATH, XaiPlatformApiKeyAuthority,
    XaiPlatformBuildError, XaiPlatformKeyGeneration, XaiPlatformKeySource,
    platform_binding::{self, ACCOUNTING, PROJECTION},
    xai_platform_selection, xai_platform_selection_for,
};
use plakous_attempt::LedgerPolicy;
use plakous_http::{
    Client, HeaderMap, HeaderName, HeaderValue, Method, Origin, Request, SensitiveBytes,
};
use plakous_model::{
    Clock, CompiledProviderConfiguration, ConfiguredLanguageModel, ContextAdmissionRequirement,
    LanguageModel, LocalDurabilityDisposition, ModelPreparationContext, ModelPreparationError,
    ModelPreparationErrorKind, OutputBoundDisposition, PreparedModelCall,
    ProviderBehaviorConfiguration, ProviderSemanticEvidence, ReasoningEffortDisposition,
    ReplayKindId, ResolvedModelDescriptor, ResolvedProviderAuthorityBinding, RuntimeModelAuthority,
    RuntimeModelAuthorityError, RuntimeModelAuthorityOperation, SelectedProviderConfiguration,
    Timestamp, ToolPresentationMode, ValidatedRequestPlan,
};
use plakous_responses::api_key::{
    Preparation, Wire, artifact_cache, configuration_matches, dispatch_available,
    fingerprint_matches, prepare, request_authority, request_matches_configuration, seal,
    stateless_stream_body,
};
use plakous_responses::{
    LifecycleDialectPolicy, ProjectorLimits, ResponseModelBinding, ResponsesDialectProfile,
    ResponsesSemanticRevisions, ResponsesStorageMode, ResponsesStreamLimits,
};
use plakous_responses::{
    ResponsesAttemptDialectData, ResponsesAttemptService, ResponsesAttemptTarget,
    ResponsesAuthSlots, ResponsesAuthorizationAdapter, ResponsesCanonicalDialectData,
    ResponsesCanonicalPolicy, ResponsesDispatchRejection, ResponsesPreflightFuture,
    ResponsesPreflightRequest, ResponsesProviderErrorPolicy, ResponsesReplayKind,
    ResponsesReplayProjection, ResponsesRequestPolicy, ResponsesResponseLimits,
    ResponsesRevalidationRequest, ResponsesStreamCapabilities, ResponsesStreamDialectData,
    ResponsesUsagePolicy, canonical_local_tools, compile_provider_configuration,
};
use std::{num::NonZeroU64, sync::Arc, time::Duration};

const REPLAY_KIND: &str = "xai.platform.output_item";
const ACCEPT: HeaderName = HeaderName::from_static("accept");
const CONTENT_TYPE: HeaderName = HeaderName::from_static("content-type");
const AUTHORIZATION: HeaderName = HeaderName::from_static("authorization");
const TIMEOUT: Duration = Duration::from_secs(300);

struct PlatformExecution {
    source: Arc<XaiPlatformKeySource>,
    grant: XaiPlatformKeyGeneration,
    model: String,
    client: Client,
    origin: Origin,
    clock: Arc<dyn Clock>,
}

/// Exact Grok 4.6 API-key model with explicitly caller-declared input bounds.
/// Uses only local function tools and stateless, encrypted reasoning replay.
pub struct XaiPlatformModel {
    execution: Arc<PlatformExecution>,
    descriptor: ResolvedModelDescriptor,
    configuration: CompiledProviderConfiguration,
    attempt_data: ResponsesAttemptDialectData,
    service: Arc<ResponsesAttemptService<PlatformExecution, XaiPlatformKeyGeneration>>,
}
impl XaiPlatformModel {
    /// Checks the selection before resolving a source-owned grant.
    pub fn preflight_configuration(
        selected: &SelectedProviderConfiguration,
    ) -> Result<(), XaiPlatformBuildError> {
        validate_selection(selected)
    }
    /// Constructs the exact official Platform route. No secret is resolved here.
    pub fn new(
        authority: XaiPlatformApiKeyAuthority,
        source: Arc<XaiPlatformKeySource>,
        maximum_input: NonZeroU64,
        selected: SelectedProviderConfiguration,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, XaiPlatformBuildError> {
        Self::with_exchange_observer(authority, source, maximum_input, selected, clock, None)
    }
    /// Constructs official execution with the existing exchange observer.
    pub fn with_exchange_observer(
        authority: XaiPlatformApiKeyAuthority,
        source: Arc<XaiPlatformKeySource>,
        maximum_input: NonZeroU64,
        selected: SelectedProviderConfiguration,
        clock: Arc<dyn Clock>,
        observer: Option<Arc<dyn plakous_http::ExchangeObserver>>,
    ) -> Result<Self, XaiPlatformBuildError> {
        let client = Client::new(plakous_http::Limits::for_streaming_inference())
            .map_err(|_| XaiPlatformBuildError::Transport)?
            .pooled()
            .with_observer(observer);
        let origin = Origin::parse(XAI_PLATFORM_API_KEY_ORIGIN)
            .map_err(|_| XaiPlatformBuildError::Transport)?;
        Self::assemble(
            authority,
            source,
            maximum_input,
            selected,
            clock,
            client,
            origin,
            &[],
        )
    }
    /// Same as [`Self::with_exchange_observer`], appending admitted catalog facts
    /// to the provider semantic evidence. Empty bytes leave that evidence unchanged.
    pub fn with_admitted_facts(
        authority: XaiPlatformApiKeyAuthority,
        source: Arc<XaiPlatformKeySource>,
        maximum_input: NonZeroU64,
        selected: SelectedProviderConfiguration,
        clock: Arc<dyn Clock>,
        observer: Option<Arc<dyn plakous_http::ExchangeObserver>>,
        admitted_facts: &[u8],
    ) -> Result<Self, XaiPlatformBuildError> {
        let client = Client::new(plakous_http::Limits::for_streaming_inference())
            .map_err(|_| XaiPlatformBuildError::Transport)?
            .pooled()
            .with_observer(observer);
        let origin = Origin::parse(XAI_PLATFORM_API_KEY_ORIGIN)
            .map_err(|_| XaiPlatformBuildError::Transport)?;
        Self::assemble(
            authority,
            source,
            maximum_input,
            selected,
            clock,
            client,
            origin,
            admitted_facts,
        )
    }
    /// Builds the exact route selection and returns a configured executable model.
    pub async fn from_behavior(
        authority: XaiPlatformApiKeyAuthority,
        source: Arc<XaiPlatformKeySource>,
        maximum_input: NonZeroU64,
        behavior: ProviderBehaviorConfiguration,
        clock: Arc<dyn Clock>,
    ) -> Result<ConfiguredLanguageModel, XaiPlatformBuildError> {
        Arc::new(Self::new(
            authority,
            source,
            maximum_input,
            xai_platform_selection(behavior)?,
            clock,
        )?)
        .configured()
        .await
    }
    /// Injects an explicit HTTPS loopback dial while retaining official credential authority.
    #[doc(hidden)]
    pub fn with_local_transport(
        authority: XaiPlatformApiKeyAuthority,
        source: Arc<XaiPlatformKeySource>,
        maximum_input: NonZeroU64,
        selected: SelectedProviderConfiguration,
        clock: Arc<dyn Clock>,
        client: Client,
        origin: Origin,
    ) -> Result<Self, XaiPlatformBuildError> {
        let bound = origin.binding_authority();
        let Some((host, port)) = bound.rsplit_once(':') else {
            return Err(XaiPlatformBuildError::Transport);
        };
        if !matches!(host, "127.0.0.1" | "localhost" | "[::1]")
            || port.parse::<u16>().ok().is_none_or(|port| port == 0)
        {
            return Err(XaiPlatformBuildError::Transport);
        }
        Self::assemble(
            authority,
            source,
            maximum_input,
            selected,
            clock,
            client,
            origin,
            &[],
        )
    }
    #[allow(
        clippy::too_many_arguments,
        reason = "admitted facts are the only addition to the existing assembler"
    )]
    fn assemble(
        authority: XaiPlatformApiKeyAuthority,
        source: Arc<XaiPlatformKeySource>,
        maximum_input: NonZeroU64,
        selected: SelectedProviderConfiguration,
        clock: Arc<dyn Clock>,
        client: Client,
        origin: Origin,
        admitted_facts: &[u8],
    ) -> Result<Self, XaiPlatformBuildError> {
        Self::preflight_configuration(&selected)?;
        let OutputBoundDisposition::Enforced(output) = selected.behavior().output_bound else {
            return Err(XaiPlatformBuildError::Configuration);
        };
        if !crate::xai_platform_capacity_admitted(
            selected.route().requested_model().as_str(),
            maximum_input.get(),
            output.get(),
        ) {
            return Err(XaiPlatformBuildError::Configuration);
        }
        let grant = platform_binding::xai_api_key_generation(source.as_ref())?;
        if grant.authority() != &authority {
            return Err(XaiPlatformBuildError::Authentication);
        }
        let execution_authority = platform_binding::execution_authority(&grant)?;
        let descriptor = platform_binding::descriptor(
            &execution_authority,
            selected.route().requested_model().as_str(),
            maximum_input,
        )?;
        let runtime = runtime_authority(&descriptor, &grant, clock.now())?;
        let configuration =
            compile_configuration(&selected, &descriptor, &runtime, &grant, admitted_facts)?;
        configuration
            .seal_runtime_authority(&descriptor, runtime, clock.now())
            .map_err(|_| XaiPlatformBuildError::Configuration)?;
        let authorization = ResponsesAuthorizationAdapter::new(1, 1, preflight, revalidate)
            .map_err(|_| XaiPlatformBuildError::Transport)?;
        let target = ResponsesAttemptTarget::from_admitted_target(
            &descriptor,
            execution_authority.account_binding(),
            revisions()?,
        )
        .map_err(|_| XaiPlatformBuildError::Transport)?;
        let mut lifecycle = LifecycleDialectPolicy::xai();
        lifecycle.model_binding = ResponseModelBinding::ExactRequest;
        let limits = ResponsesResponseLimits::default();
        let stream = ResponsesStreamDialectData::new(
            ResponsesDialectProfile::xai(),
            lifecycle,
            descriptor.requested_model().as_str(),
            ResponsesUsagePolicy::RequireTotal,
            limits,
            ResponsesStreamCapabilities::grok(),
        )
        .with_typed_terminal_cutoff();
        let replay = ResponsesReplayProjection::new(
            descriptor.protocol().clone(),
            descriptor.model_family().cloned(),
            descriptor.requested_model().clone(),
            ReplayKindId::new(REPLAY_KIND).map_err(|_| XaiPlatformBuildError::Configuration)?,
            1,
            execution_authority.account_binding(),
        )
        .map_err(|_| XaiPlatformBuildError::Transport)?;
        let attempt_data = ResponsesAttemptDialectData::new(
            1,
            target,
            &client,
            &authorization,
            LedgerPolicy::RESPONSES_FAMILY,
            ResponsesAuthSlots::Primary,
            request_policy(),
            stream,
            ResponsesStreamLimits::default(),
            replay,
            ResponsesStorageMode::Stateless,
            ResponsesProviderErrorPolicy::Grok,
            limits,
            None,
            false,
        )
        .map_err(|_| XaiPlatformBuildError::Transport)?;
        let execution = Arc::new(PlatformExecution {
            source,
            grant,
            model: descriptor.requested_model().as_str().to_owned(),
            client: client.clone(),
            origin,
            clock,
        });
        let service = Arc::new(
            ResponsesAttemptService::new(
                client,
                execution.clone(),
                attempt_data.clone(),
                authorization,
            )
            .map_err(|_| XaiPlatformBuildError::Transport)?,
        );
        Ok(Self {
            execution,
            descriptor,
            configuration,
            attempt_data,
            service,
        })
    }
    /// The session's hosted web and X search side-request executor on this
    /// exact route, key grant, model and client, at `effort` (pass the
    /// lowest effort the model's ladder admits).
    #[must_use]
    pub fn retrieval(self: &Arc<Self>, effort: Option<&'static str>) -> crate::XaiRetrieval {
        crate::XaiRetrieval::new(Arc::clone(self), effort)
    }
    /// The session's image generation executor on this exact route, key
    /// and client.
    #[must_use]
    pub fn images(self: &Arc<Self>) -> crate::XaiImages {
        crate::XaiImages::new(Arc::clone(self))
    }

    pub(crate) fn client(&self) -> &Client {
        &self.execution.client
    }
    pub(crate) fn requested_model(&self) -> &str {
        &self.execution.model
    }
    /// Revalidates admission exactly as before a model round, then builds
    /// the authorized request carrying `body` with the grant's current key.
    pub(crate) async fn side_request(
        self: &Arc<Self>,
        body: &[u8],
    ) -> Result<Request, &'static str> {
        self.side_request_to(XAI_PLATFORM_RESPONSES_PATH, "text/event-stream", body)
            .await
    }

    /// [`Self::side_request`] to another `path` of the same origin.
    pub(crate) async fn side_request_to(
        self: &Arc<Self>,
        path: &'static str,
        accept: &'static str,
        body: &[u8],
    ) -> Result<Request, &'static str> {
        let execution = &self.execution;
        Arc::clone(self)
            .revalidate_runtime_authority(execution.clock.now())
            .await
            .map_err(|_| "xAI API admission is no longer current")?;
        check_grant(execution).map_err(|_| "xAI API key grant is no longer current")?;
        let key = execution
            .source
            .api_key(&platform_binding::API_KEY_REQUEST, &execution.grant)
            .map_err(|_| "xAI API key is unavailable")?;
        if key.generation_grant(
            plakous_auth::api_key::CredentialProviderTag::XaiPlatform,
            execution.grant.authority().clone(),
        ) != execution.grant
        {
            return Err("xAI API key does not match the session's grant");
        }
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static(accept));
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            AUTHORIZATION,
            platform_binding::platform_bearer(&key)
                .map_err(|_| "xAI API key is not a header value")?,
        );
        Request::new(
            execution.origin.clone(),
            Method::POST,
            path,
            headers,
            SensitiveBytes::new(body.to_vec()).into_bytes(),
            execution.client.limits(),
        )
        .map_err(|_| "xAI side request could not be built")
    }
    /// Returns the maintained descriptor with caller-declared context evidence.
    #[must_use]
    pub const fn descriptor(&self) -> &ResolvedModelDescriptor {
        &self.descriptor
    }
    /// Returns the complete selected and effective provider configuration.
    #[must_use]
    pub const fn configuration(&self) -> &CompiledProviderConfiguration {
        &self.configuration
    }
    /// Joins the model, compiled configuration, and current credential authority.
    pub async fn configured(
        self: Arc<Self>,
    ) -> Result<ConfiguredLanguageModel, XaiPlatformBuildError> {
        let authority = self
            .clone()
            .revalidate_runtime_authority(self.execution.clock.now())
            .await
            .map_err(|_| XaiPlatformBuildError::Authentication)?;
        ConfiguredLanguageModel::new(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            self.clone(),
            self.configuration.clone(),
            authority,
        )
        .map_err(|_| XaiPlatformBuildError::Configuration)
    }
}

impl LanguageModel for XaiPlatformModel {
    fn reachability_probe(&self) -> Option<plakous_model::ReachabilityProbe> {
        let origin = self.execution.origin.clone();
        Some(plakous_responses::bind_reachability_probe(
            self.execution.client.clone(),
            origin,
            self.descriptor.route().clone(),
        ))
    }

    fn revalidate_runtime_authority(
        self: Arc<Self>,
        checked_at: Timestamp,
    ) -> RuntimeModelAuthorityOperation {
        Box::pin(async move {
            check_grant(&self.execution).map_err(|_| RuntimeModelAuthorityError::Unavailable)?;
            let runtime = runtime_authority(&self.descriptor, &self.execution.grant, checked_at)
                .map_err(|_| RuntimeModelAuthorityError::ForeignAuthority)?;
            self.configuration
                .seal_runtime_authority(&self.descriptor, runtime.clone(), checked_at)
                .map_err(|_| RuntimeModelAuthorityError::ForeignAuthority)?;
            Ok(runtime)
        })
    }
    #[allow(
        clippy::too_many_lines,
        reason = "Validate exact xAI Platform authority and seal the Responses wire and context projection without dispatch."
    )]
    fn prepare_call(
        self: Arc<Self>,
        plan: ValidatedRequestPlan,
        context: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError> {
        self.descriptor.validate_plan_binding(&plan)?;
        if !configuration_matches(&plan, &self.configuration) {
            return Err(rejection(
                "xAI Platform request provider configuration is foreign",
            ));
        }
        if !request_matches_configuration(plan.request(), self.configuration.selected().behavior())
        {
            return Err(rejection(
                "xAI Platform request behavior differs from compiled configuration",
            ));
        }
        self.descriptor
            .validate_plan_admission(plan.request(), &plan)?;
        context
            .authority()
            .validate(&self.descriptor, context.prepared_at())?;
        let execution_authority = platform_binding::execution_authority(&self.execution.grant)
            .map_err(|_| rejection("xAI Platform credential binding is invalid"))?;
        if context.authority().account_binding() != execution_authority.account_binding()
            || context.authority().credential_authority_binding()
                != execution_authority.credential_authority_binding()
            || context.authority().credential_generation()
                != self.execution.grant.authority().generation()
        {
            return Err(RuntimeModelAuthorityError::ForeignAuthority.into());
        }
        let request = plan.request();
        let canonical_tools = canonical_local_tools(request);
        let canonical = ResponsesCanonicalDialectData::new(
            self.descriptor.requested_model().clone(),
            ResponsesCanonicalPolicy::xai_platform(),
            ProjectorLimits::default(),
            1024 * 1024,
            canonical_tools,
        )
        .map_err(|_| rejection("xAI Platform canonical projection is invalid"))?;
        let authority = request_authority(
            &self.descriptor,
            &plan,
            &context,
            revisions().map_err(|_| rejection("xAI Platform revisions are invalid"))?,
        )
        .map_err(|_| rejection("xAI Platform request authority is invalid"))?;
        prepare(Preparation {
            label: "xAI Platform",
            descriptor: &self.descriptor,
            plan,
            context: &context,
            attempt: &self.attempt_data,
            canonical,
            authority,
            output_item_replay: ResponsesReplayKind::new(REPLAY_KIND, 1),
            previous_response_replay: None,
            adapter_revision: XAI_PLATFORM_EXECUTION_REVISION,
            projection_revision: PROJECTION,
            accounting_revision: ACCOUNTING,
            cache_use: artifact_cache(&context),
            observation_capacity: 64,
            clock: Arc::clone(&self.execution.clock),
            service: Arc::clone(&self.service),
            operation_timeout: TIMEOUT,
        })
    }
}
fn request_policy() -> ResponsesRequestPolicy {
    ResponsesRequestPolicy::xai_platform_stateless(XAI_PLATFORM_ENCRYPTED_REASONING_INCLUDE)
}
fn validate_selection(
    selected: &SelectedProviderConfiguration,
) -> Result<(), XaiPlatformBuildError> {
    let model = selected.route().requested_model().as_str();
    if &xai_platform_selection_for(model, selected.behavior().clone())? != selected {
        return Err(XaiPlatformBuildError::Configuration);
    }
    let behavior = selected.behavior();
    let effort_admitted = match behavior.reasoning_effort {
        ReasoningEffortDisposition::Enabled(effort) => {
            crate::xai_platform_effort_admitted(model, effort)
        }
        ReasoningEffortDisposition::Disabled => false,
    };
    // xAI publishes no output limit: the bound is reserved out of the
    // shared context, checked against the input window in `assemble`.
    if !effort_admitted
        || behavior.tool_mode != ToolPresentationMode::Direct
        || behavior.context_evidence_requirement != ContextAdmissionRequirement::AnyBounded
        || behavior.local_durability != LocalDurabilityDisposition::DurableRequired
        || !behavior.hosted_tools.is_empty()
        || !matches!(behavior.output_bound, OutputBoundDisposition::Enforced(_))
    {
        return Err(XaiPlatformBuildError::Configuration);
    }
    Ok(())
}
fn compile_configuration(
    selected: &SelectedProviderConfiguration,
    descriptor: &ResolvedModelDescriptor,
    runtime: &RuntimeModelAuthority,
    grant: &XaiPlatformKeyGeneration,
    admitted_facts: &[u8],
) -> Result<CompiledProviderConfiguration, XaiPlatformBuildError> {
    let build = || -> Result<_, Box<dyn std::error::Error>> {
        let mut semantic_bytes = serde_json::to_vec(&(
            XAI_PLATFORM_EXECUTION_REVISION,
            XAI_PLATFORM_API_KEY_ORIGIN,
            XAI_PLATFORM_RESPONSES_PATH,
            descriptor.semantic_binding_digest()?,
            grant.fingerprint(),
        ))?;
        semantic_bytes.extend_from_slice(admitted_facts);
        let semantic = ProviderSemanticEvidence::new(1, semantic_bytes)?;
        let authority = ResolvedProviderAuthorityBinding::new(descriptor, runtime, semantic)?;
        Ok(compile_provider_configuration(
            selected,
            descriptor.clone(),
            authority,
            request_policy(),
        )?)
    };
    build().map_err(|_| XaiPlatformBuildError::Configuration)
}
fn runtime_authority(
    descriptor: &ResolvedModelDescriptor,
    grant: &XaiPlatformKeyGeneration,
    now: Timestamp,
) -> Result<RuntimeModelAuthority, XaiPlatformBuildError> {
    let authority = platform_binding::execution_authority(grant)?;
    RuntimeModelAuthority::new(
        descriptor,
        authority.account_binding(),
        authority.credential_authority_binding(),
        grant.authority().generation(),
        now,
        descriptor
            .context_contract()
            .and_then(|c| c.capacity().freshness().expires_at()),
    )
    .map_err(|_| XaiPlatformBuildError::Configuration)
}
fn check_grant(execution: &PlatformExecution) -> Result<(), XaiPlatformBuildError> {
    if platform_binding::xai_api_key_generation(execution.source.as_ref())? != execution.grant {
        return Err(XaiPlatformBuildError::Authentication);
    }
    Ok(())
}
fn preflight<'a>(
    execution: &'a PlatformExecution,
    request: ResponsesPreflightRequest<'a>,
) -> ResponsesPreflightFuture<'a, XaiPlatformKeyGeneration> {
    Box::pin(async move {
        dispatch_available(request.cancellation().is_cancelled(), request.deadline())?;
        check_grant(execution).map_err(|_| ResponsesDispatchRejection::Authentication)?;
        stateless_stream_body(request.body_bytes(), Some(execution.model.as_str()))?;
        let key = execution
            .source
            .api_key(&platform_binding::API_KEY_REQUEST, &execution.grant)
            .map_err(|_| ResponsesDispatchRejection::Authentication)?;
        if key.generation_grant(
            plakous_auth::api_key::CredentialProviderTag::XaiPlatform,
            execution.grant.authority().clone(),
        ) != execution.grant
        {
            return Err(ResponsesDispatchRejection::Authentication);
        }
        check_grant(execution).map_err(|_| ResponsesDispatchRejection::Authentication)?;
        seal(Wire {
            origin: execution.origin.clone(),
            path: XAI_PLATFORM_RESPONSES_PATH,
            authorization: platform_binding::platform_bearer(&key)
                .map_err(|_| ResponsesDispatchRejection::Authentication)?,
            extra_headers: Vec::new(),
            body: request.body_bytes().to_vec(),
            limits: execution.client.limits().clone(),
            fingerprint: execution.grant.fingerprint(),
            physical: request.physical().clone(),
            guard: execution.grant.clone(),
        })
    })
}
fn revalidate(
    execution: &PlatformExecution,
    request: ResponsesRevalidationRequest<'_, XaiPlatformKeyGeneration>,
) -> Result<(), ResponsesDispatchRejection> {
    dispatch_available(request.cancellation().is_cancelled(), request.deadline())?;
    check_grant(execution).map_err(|_| ResponsesDispatchRejection::Authentication)?;
    fingerprint_matches(
        request.binding().credential_authority(),
        execution.grant.fingerprint(),
    )
}
fn revisions() -> Result<ResponsesSemanticRevisions, XaiPlatformBuildError> {
    ResponsesSemanticRevisions::new(1, 1, 1).map_err(|_| XaiPlatformBuildError::Transport)
}
fn rejection(message: &str) -> ModelPreparationError {
    ModelPreparationError::rejected(ModelPreparationErrorKind::InvalidConfiguration, message)
}
