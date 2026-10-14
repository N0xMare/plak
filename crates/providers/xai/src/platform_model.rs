//! API-key Platform executor through the shared Responses attempt service.

use crate::{
    XAI_PLATFORM_API_KEY_ORIGIN, XAI_PLATFORM_ENCRYPTED_REASONING_INCLUDE,
    XAI_PLATFORM_EXECUTION_REVISION, XAI_PLATFORM_RESPONSES_PATH, XaiPlatformApiKeyAuthority,
    XaiPlatformApiKeySource, XaiPlatformBuildError, XaiPlatformCredentialGrant,
    platform_binding::{self, ACCOUNTING, PROJECTION},
    xai_platform_selection, xai_platform_selection_for,
};
use plakous_attempt::LedgerPolicy;
use plakous_http::{
    Client, HeaderMap, HeaderName, HeaderValue, Method, Origin, Request, SensitiveBytes,
};
use plakous_model::{
    CapabilityRequirement, Clock, CompiledProviderConfiguration, ConfiguredLanguageModel,
    ContextAdmissionRequirement, CountObservation, Digest, LanguageModel,
    LocalDurabilityDisposition, ModelCacheAffinity, ModelPreparationContext, ModelPreparationError,
    ModelPreparationErrorKind, ModelRequest, OutputBoundDisposition, PreparedCacheUse,
    PreparedCallError, PreparedCallRecord, PreparedModelCall, PreparedModelProjection,
    PreparedOutputLimit, PreparedTokenProjection, ProtocolId, ProviderBehaviorConfiguration,
    ProviderSemanticEvidence, ReasoningEffortDisposition, ReasoningPolicy, ReplayKindId,
    ResolvedModelDescriptor, ResolvedProviderAuthorityBinding, RuntimeModelAuthority,
    RuntimeModelAuthorityError, RuntimeModelAuthorityOperation, SelectedProviderConfiguration,
    Timestamp, ToolPresentationMode, ValidatedRequestPlan,
};
use plakous_responses::{
    CanonicalIdSources, LifecycleDialectPolicy, ProjectorLimits, ResponseModelBinding,
    ResponsesDialectProfile, ResponsesSemanticRevisions, ResponsesStorageMode,
    ResponsesStreamLimits,
};
use plakous_responses_dialect::{
    PreparedResponsesModelExecution, ResponsesArtifactReplayBridge, ResponsesAttemptDialectData,
    ResponsesAttemptService, ResponsesAttemptTarget, ResponsesAuthSlot, ResponsesAuthSlots,
    ResponsesAuthorizationAdapter, ResponsesCanonicalDialectData, ResponsesCanonicalPolicy,
    ResponsesCredentialAuthority, ResponsesDispatchRejection, ResponsesHeaderBinding,
    ResponsesPreflightFuture, ResponsesPreflightRequest, ResponsesPreparedPhysicalRequest,
    ResponsesProviderErrorPolicy, ResponsesReplayKind, ResponsesReplayProjection,
    ResponsesRequestAuthority, ResponsesRequestPolicy, ResponsesResponseLimits,
    ResponsesRevalidationRequest, ResponsesStoragePolicy, ResponsesStreamCapabilities,
    ResponsesStreamDialectData, ResponsesUsagePolicy, canonical_local_tools,
    compile_provider_configuration, prepare_responses_attempt, start_prepared_responses_model_call,
};
use std::{
    num::{NonZeroU32, NonZeroU64},
    sync::Arc,
    time::{Duration, Instant},
};

const REPLAY_KIND: &str = "xai.platform.output_item";
const FAMILY: &str = "responses-family";
const ACCEPT: HeaderName = HeaderName::from_static("accept");
const CONTENT_TYPE: HeaderName = HeaderName::from_static("content-type");
const AUTHORIZATION: HeaderName = HeaderName::from_static("authorization");
const TIMEOUT: Duration = Duration::from_secs(300);

struct PlatformExecution {
    source: Arc<dyn XaiPlatformApiKeySource>,
    grant: XaiPlatformCredentialGrant,
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
    service: Arc<ResponsesAttemptService<PlatformExecution, XaiPlatformCredentialGrant>>,
}
impl XaiPlatformModel {
    /// Checks reviewed facts and selection before resolving a source-owned grant.
    pub fn preflight_configuration(
        selected: &SelectedProviderConfiguration,
        clock: &dyn Clock,
    ) -> Result<(), XaiPlatformBuildError> {
        platform_binding::current(clock.now())?;
        validate_selection(selected)
    }
    /// Constructs the exact official Platform route. No secret is resolved here.
    pub fn new(
        authority: XaiPlatformApiKeyAuthority,
        source: Arc<dyn XaiPlatformApiKeySource>,
        maximum_input: NonZeroU64,
        selected: SelectedProviderConfiguration,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, XaiPlatformBuildError> {
        Self::with_exchange_observer(authority, source, maximum_input, selected, clock, None)
    }
    /// Constructs official execution with the existing exchange observer.
    pub fn with_exchange_observer(
        authority: XaiPlatformApiKeyAuthority,
        source: Arc<dyn XaiPlatformApiKeySource>,
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
        source: Arc<dyn XaiPlatformApiKeySource>,
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
        source: Arc<dyn XaiPlatformApiKeySource>,
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
        source: Arc<dyn XaiPlatformApiKeySource>,
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
        source: Arc<dyn XaiPlatformApiKeySource>,
        maximum_input: NonZeroU64,
        selected: SelectedProviderConfiguration,
        clock: Arc<dyn Clock>,
        client: Client,
        origin: Origin,
        admitted_facts: &[u8],
    ) -> Result<Self, XaiPlatformBuildError> {
        Self::preflight_configuration(&selected, clock.as_ref())?;
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
        let grant = source.grant()?;
        if grant.authority != authority {
            return Err(XaiPlatformBuildError::Authentication);
        }
        let execution_authority = grant.execution_authority()?;
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
        Some(plakous_responses_dialect::bind_reachability_probe(
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
            platform_binding::current(checked_at).map_err(|_| RuntimeModelAuthorityError::Stale)?;
            check_grant(&self.execution).map_err(|_| RuntimeModelAuthorityError::Unavailable)?;
            let runtime = runtime_authority(&self.descriptor, &self.execution.grant, checked_at)
                .map_err(|_| RuntimeModelAuthorityError::ForeignAuthority)?;
            self.configuration
                .seal_runtime_authority(&self.descriptor, runtime.clone(), checked_at)
                .map_err(|_| RuntimeModelAuthorityError::ForeignAuthority)?;
            Ok(runtime)
        })
    }
    fn prepare_call(
        self: Arc<Self>,
        plan: ValidatedRequestPlan,
        context: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError> {
        platform_binding::current(self.execution.clock.now())
            .map_err(|_| rejection("Platform route evidence is stale"))?;
        self.descriptor.validate_plan_binding(&plan)?;
        if plan.request().selected_provider_configuration_id()
            != self.configuration.selected_identity()
            || plan.request().effective_provider_configuration_id()
                != self.configuration.effective_identity()
        {
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
        let execution_authority = self
            .execution
            .grant
            .execution_authority()
            .map_err(|_| rejection("xAI Platform credential binding is invalid"))?;
        if context.authority().account_binding() != execution_authority.account_binding()
            || context.authority().credential_authority_binding()
                != execution_authority.credential_authority_binding()
            || context.authority().credential_generation()
                != self.execution.grant.authority.generation()
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
        let authority = ResponsesRequestAuthority::from_admitted_operation(
            &self.descriptor,
            &plan,
            request,
            context.authority().account_binding(),
            revisions().map_err(|_| rejection("xAI Platform revisions are invalid"))?,
            context.cache_affinity().map(ModelCacheAffinity::identity),
            ResponsesStoragePolicy::Stateless,
        )
        .map_err(|_| rejection("xAI Platform request authority is invalid"))?;
        let replay_bridge = context
            .active_family_artifact()
            .map(|active| {
                ResponsesArtifactReplayBridge::from_active_artifact(
                    active,
                    request,
                    authority.clone(),
                    ResponsesReplayKind::new(REPLAY_KIND, 1),
                    None,
                    None,
                    context.prepared_at(),
                )
            })
            .transpose()
            // An artifact covering a different canonical or stable prefix is
            // not this request's, so prepare fresh rather than refuse. Same rule as the
            // SuperGrok route.
            .or_else(|error: plakous_responses_dialect::ResponsesRequestError| {
                error
                    .artifact_is_not_for_this_request()
                    .then_some(None)
                    .ok_or(error)
            })
            .map_err(|_| rejection("xAI Platform active Responses artifact is not reusable"))?;
        let prepared_attempt = prepare_responses_attempt(
            authority,
            request,
            replay_bridge
                .as_ref()
                .map(ResponsesArtifactReplayBridge::context),
            &self.attempt_data,
        )
        .map_err(|error| {
            ModelPreparationError::rejected(
                error.preparation_error_kind(),
                format!("xAI Platform Responses request lowering failed: {error}"),
            )
        })?;
        let artifact_binding = prepared_attempt.authority().artifact_binding().clone();
        let wire_digest = prepared_attempt.wire_digest();
        let wire_size = u64::try_from(prepared_attempt.wire_size())
            .map_err(|_| rejection("xAI Platform wire size is not representable"))?;
        let count_observation = self
            .descriptor
            .context_contract()
            .and_then(|contract| contract.measurements().first())
            .map(|certificate| {
                CountObservation::new(
                    certificate.certificate_id(),
                    certificate.revision(),
                    wire_size,
                )
                .map_err(|_| rejection("xAI Platform wire measurement is invalid"))
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
        let output_limit =
            enforced_wire_output_limit(prepared_attempt.enforced_output_tokens(), wire_digest)?;
        let projection = PreparedModelProjection::new(
            &self.descriptor,
            &plan,
            &context,
            ProtocolId::new(FAMILY).map_err(|_| rejection("invalid family ID"))?,
            XAI_PLATFORM_EXECUTION_REVISION,
            PROJECTION,
            ACCOUNTING,
            wire_digest,
            wire_size,
            token_projection,
            context
                .active_family_artifact()
                .map_or(PreparedCacheUse::None, |artifact| {
                    PreparedCacheUse::ActiveFamilyArtifact {
                        artifact: artifact.integrity(),
                    }
                }),
            output_limit,
        )?;
        let record =
            PreparedCallRecord::for_prepared(&self.descriptor, &plan, projection, &context)?;
        let execution = PreparedResponsesModelExecution::new(
            prepared_attempt,
            canonical,
            CanonicalIdSources::with_clock(Arc::clone(&self.execution.clock)),
            CanonicalIdSources::with_clock(Arc::clone(&self.execution.clock)),
            replay_bridge
                .as_ref()
                .map(|bridge| bridge.hosted_programs().clone())
                .unwrap_or_default(),
            plan,
            artifact_binding,
            Arc::clone(&self.execution.clock),
            self.descriptor.clone(),
            64,
            None,
        )
        .map_err(|_| rejection("xAI Platform prepared execution is invalid"))?;
        let service = Arc::clone(&self.service);
        let clock = Arc::clone(&self.execution.clock);
        Ok(PreparedModelCall::new(record, move |call_context| {
            let deadline = operation_deadline(clock.as_ref(), None);
            start_prepared_responses_model_call(execution, service, call_context, deadline)
        }))
    }
}
fn request_matches_configuration(
    request: &ModelRequest,
    behavior: &ProviderBehaviorConfiguration,
) -> bool {
    let reasoning = match (request.reasoning(), behavior.reasoning_effort) {
        (ReasoningPolicy::Disabled, ReasoningEffortDisposition::Disabled) => true,
        (
            ReasoningPolicy::Enabled {
                effort: Some(requested),
                requirement: CapabilityRequirement::Required,
            },
            ReasoningEffortDisposition::Enabled(configured),
        ) => requested == configured,
        _ => false,
    };
    reasoning
        && request.output() == &behavior.output
        && request.cache() == behavior.cache
        && behavior.matches_request_execution_controls(request)
        && request.context_admission_requirement() == behavior.context_evidence_requirement
        && request.limits().stop_sequences().is_empty()
        && request.native().entries().is_empty()
        && request.tools().len() == behavior.tools.len()
        && request.hosted_tools() == behavior.hosted_tools
        && request
            .tools()
            .iter()
            .zip(&behavior.tools)
            .all(|(request, configured)| request == configured)
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
    grant: &XaiPlatformCredentialGrant,
    admitted_facts: &[u8],
) -> Result<CompiledProviderConfiguration, XaiPlatformBuildError> {
    let build = || -> Result<_, Box<dyn std::error::Error>> {
        let mut semantic_bytes = serde_json::to_vec(&(
            XAI_PLATFORM_EXECUTION_REVISION,
            XAI_PLATFORM_API_KEY_ORIGIN,
            XAI_PLATFORM_RESPONSES_PATH,
            descriptor.semantic_binding_digest()?,
            grant.fingerprint,
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
    grant: &XaiPlatformCredentialGrant,
    now: Timestamp,
) -> Result<RuntimeModelAuthority, XaiPlatformBuildError> {
    let authority = grant.execution_authority()?;
    RuntimeModelAuthority::new(
        descriptor,
        authority.account_binding(),
        authority.credential_authority_binding(),
        grant.authority.generation(),
        now,
        descriptor
            .context_contract()
            .and_then(|c| c.capacity().freshness().expires_at()),
    )
    .map_err(|_| XaiPlatformBuildError::Configuration)
}
fn check_grant(execution: &PlatformExecution) -> Result<(), XaiPlatformBuildError> {
    platform_binding::current(execution.clock.now())?;
    if execution.source.grant()? != execution.grant {
        return Err(XaiPlatformBuildError::Authentication);
    }
    Ok(())
}
fn preflight<'a>(
    execution: &'a PlatformExecution,
    request: ResponsesPreflightRequest<'a>,
) -> ResponsesPreflightFuture<'a, XaiPlatformCredentialGrant> {
    Box::pin(async move {
        if request.cancellation().is_cancelled() || Instant::now() >= request.deadline() {
            return Err(ResponsesDispatchRejection::Unavailable);
        }
        check_grant(execution).map_err(|_| ResponsesDispatchRejection::Authentication)?;
        let body: serde_json::Value = serde_json::from_slice(request.body_bytes())
            .map_err(|_| ResponsesDispatchRejection::Binding)?;
        if body["model"].as_str() != Some(execution.model.as_str())
            || body["stream"] != true
            || body["store"] != false
        {
            return Err(ResponsesDispatchRejection::Binding);
        }
        let key = execution
            .source
            .api_key(&execution.grant)
            .map_err(|_| ResponsesDispatchRejection::Authentication)?;
        if key
            .grant(execution.grant.authority.clone())
            .map_err(|_| ResponsesDispatchRejection::Authentication)?
            != execution.grant
        {
            return Err(ResponsesDispatchRejection::Authentication);
        }
        check_grant(execution).map_err(|_| ResponsesDispatchRejection::Authentication)?;
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            AUTHORIZATION,
            key.bearer()
                .map_err(|_| ResponsesDispatchRejection::Authentication)?,
        );
        let bindings = vec![
            ResponsesHeaderBinding::PublicExact {
                name: ACCEPT,
                value: b"text/event-stream".to_vec(),
            },
            ResponsesHeaderBinding::PublicExact {
                name: CONTENT_TYPE,
                value: b"application/json".to_vec(),
            },
            ResponsesHeaderBinding::CredentialSecret {
                name: AUTHORIZATION,
                slot: ResponsesAuthSlot::Primary,
            },
        ];
        let wire = Request::new(
            execution.origin.clone(),
            Method::POST,
            XAI_PLATFORM_RESPONSES_PATH,
            headers,
            SensitiveBytes::new(request.body_bytes().to_vec()).into_bytes(),
            execution.client.limits(),
        )
        .map_err(|_| ResponsesDispatchRejection::Binding)?;
        Ok(ResponsesPreparedPhysicalRequest::new(
            request.physical().clone(),
            wire,
            ResponsesCredentialAuthority::primary(execution.grant.fingerprint),
            bindings,
            execution.grant.clone(),
        ))
    })
}
fn revalidate(
    execution: &PlatformExecution,
    request: ResponsesRevalidationRequest<'_, XaiPlatformCredentialGrant>,
) -> Result<(), ResponsesDispatchRejection> {
    if request.cancellation().is_cancelled() || Instant::now() >= request.deadline() {
        return Err(ResponsesDispatchRejection::Unavailable);
    }
    check_grant(execution).map_err(|_| ResponsesDispatchRejection::Authentication)?;
    if request
        .binding()
        .credential_authority()
        .generation(ResponsesAuthSlot::Primary)
        != Some(execution.grant.fingerprint)
    {
        return Err(ResponsesDispatchRejection::Binding);
    }
    Ok(())
}
fn revisions() -> Result<ResponsesSemanticRevisions, XaiPlatformBuildError> {
    ResponsesSemanticRevisions::new(1, 1, 1).map_err(|_| XaiPlatformBuildError::Transport)
}
fn enforced_wire_output_limit(
    tokens: Option<NonZeroU32>,
    wire_digest: Digest,
) -> Result<Option<PreparedOutputLimit>, PreparedCallError> {
    tokens
        .map(|tokens| {
            PreparedOutputLimit::enforced_wire_maximum(
                tokens,
                "max_output_tokens",
                wire_digest,
                XAI_PLATFORM_EXECUTION_REVISION,
            )
        })
        .transpose()
}
fn rejection(message: &str) -> ModelPreparationError {
    ModelPreparationError::rejected(ModelPreparationErrorKind::InvalidConfiguration, message)
}
fn operation_deadline(clock: &dyn Clock, requested: Option<Timestamp>) -> Instant {
    let now = Instant::now();
    let configured = now.checked_add(TIMEOUT).unwrap_or(now);
    let Some(requested) = requested else {
        return configured;
    };
    let wall = clock.now();
    if requested <= wall {
        return now;
    }
    now.checked_add(Duration::from_secs(
        requested.seconds().saturating_sub(wall.seconds()),
    ))
    .map_or(configured, |deadline| deadline.min(configured))
}
