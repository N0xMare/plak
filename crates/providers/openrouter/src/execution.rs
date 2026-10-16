//! Runnable OpenRouter composition over the shared Responses family hosts.

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use plakous_attempt::LedgerPolicy;
use plakous_http::{
    Client, HeaderMap, HeaderName, HeaderValue, Method, Origin, Request, SensitiveBytes,
    sensitive_header_value,
};
use plakous_model::{
    CapabilityRequirement, Clock, CompiledProviderConfiguration, ConfiguredLanguageModel,
    CountObservation, CredentialSelectionId, LanguageModel, ModelAdapterProfileId,
    ModelPreparationContext, ModelPreparationError, ModelPreparationErrorKind,
    OutputBoundDisposition, PreparedCacheUse, PreparedCallRecord, PreparedModelCall,
    PreparedModelProjection, PreparedOutputLimit, PreparedTokenProjection, ProtocolId,
    ProviderAuthorityClassId, ProviderBehaviorConfiguration, ProviderProfileRevisionId,
    ProviderRouteSelection, ReasoningEffortDisposition, ReasoningPolicy, ResolvedModelDescriptor,
    RuntimeModelAuthorityError, RuntimeModelAuthorityOperation, SelectedProviderConfiguration,
    SystemClock, Timestamp, ValidatedRequestPlan,
};
use plakous_responses::{
    CanonicalIdSources, ResponsesSemanticRevisions, ResponsesStorageMode, ResponsesStreamLimits,
};
use plakous_responses_dialect::{
    PreparedResponsesModelExecution, ResponsesArtifactReplayBridge, ResponsesAttemptDialectData,
    ResponsesAttemptService, ResponsesAttemptTarget, ResponsesAuthSlot, ResponsesAuthSlots,
    ResponsesAuthorizationAdapter, ResponsesCanonicalDialectData, ResponsesCredentialAuthority,
    ResponsesDispatchRejection, ResponsesHeaderBinding, ResponsesPreflightFuture,
    ResponsesPreflightRequest, ResponsesPreparedPhysicalRequest, ResponsesProviderErrorPolicy,
    ResponsesRequestAuthority, ResponsesResponseLimits, ResponsesRevalidationRequest,
    ResponsesStoragePolicy, canonical_local_tools, is_unprojected_string_custom_tool_definition,
    prepare_responses_attempt, start_prepared_responses_model_call,
};
use thiserror::Error;

use crate::{
    OpenRouterApiKeyGenerationGrant, OpenRouterDialect, OpenRouterResponsesTarget,
    compile_openrouter_responses_configuration, dialect::OPENROUTER_RESPONSES_DIALECT_REVISION,
};

const DEFAULT_OPERATION_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const DEFAULT_EVENT_CHANNEL_CAPACITY: usize = 64;
const PREFLIGHT_REVISION: u32 = 1;
const FINAL_AUTHORIZATION_REVISION: u32 = 1;
const RESPONSES_FAMILY: &str = "responses-family";
pub(crate) const OPENROUTER_ADAPTER_REVISION: &str = "plakous-openrouter-responses-adapter-v2";
const OPENROUTER_PROJECTION_REVISION: &str = "responses-canonical-openrouter-v1";
const OPENROUTER_ACCOUNTING_REVISION: &str = "responses-wire-bytes-openrouter-v1";
const OPENROUTER_INTEGRATION_REVISION: &str = "openrouter-responses-integration-v1";
const OPENROUTER_AUTH_PROFILE: &str = "openrouter.platform-api-key";
const OPENROUTER_AUTH_REVISION: &str = "openrouter-platform-api-key-v1";
const OPENROUTER_AUTHORITY_CLASS: &str = "platform-api-key";
const OPENROUTER_CREDENTIAL_SELECTION: &str = "openrouter-product-key-generation";

const HEADER_ACCEPT: HeaderName = HeaderName::from_static("accept");
const HEADER_CONTENT_TYPE: HeaderName = HeaderName::from_static("content-type");
const HEADER_AUTHORIZATION: HeaderName = HeaderName::from_static("authorization");
const HEADER_GENERATION_ID: HeaderName = HeaderName::from_static("x-generation-id");
const HEADER_HTTP_REFERER: HeaderName = HeaderName::from_static("http-referer");
const HEADER_CATEGORIES: HeaderName = HeaderName::from_static("x-openrouter-categories");
const HEADER_TITLE: HeaderName = HeaderName::from_static("x-openrouter-title");
const HEADER_OPENROUTER_METADATA: HeaderName = HeaderName::from_static("x-openrouter-metadata");

/// Immutable operation and decoder bounds for the OpenRouter shared host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpenRouterResponsesHttpConfig {
    operation_timeout: Duration,
    event_channel_capacity: usize,
    responses_limits: ResponsesStreamLimits,
}

impl Default for OpenRouterResponsesHttpConfig {
    fn default() -> Self {
        Self {
            operation_timeout: DEFAULT_OPERATION_TIMEOUT,
            event_channel_capacity: DEFAULT_EVENT_CHANNEL_CAPACITY,
            responses_limits: ResponsesStreamLimits::default(),
        }
    }
}

impl OpenRouterResponsesHttpConfig {
    /// Creates explicit nonzero operation, delivery, and decoder bounds.
    pub fn new(
        operation_timeout: Duration,
        event_channel_capacity: usize,
        responses_limits: ResponsesStreamLimits,
    ) -> Result<Self, OpenRouterResponsesLanguageModelConfigError> {
        if operation_timeout.is_zero() || event_channel_capacity == 0 {
            return Err(OpenRouterResponsesLanguageModelConfigError::InvalidPolicy);
        }
        Ok(Self {
            operation_timeout,
            event_channel_capacity,
            responses_limits,
        })
    }
}

/// Invalid runnable OpenRouter model composition.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum OpenRouterResponsesLanguageModelConfigError {
    /// A duration or queue bound was zero.
    #[error("OpenRouter shared-host policy bounds must be nonzero")]
    InvalidPolicy,
    /// Shared Responses construction facts were inconsistent.
    #[error("OpenRouter shared Responses construction failed")]
    SharedBinding,
    /// The selected provider configuration did not exactly compile for this target.
    #[error("OpenRouter selected provider configuration does not match the local target")]
    Configuration,
}

struct OpenRouterResponsesExecution {
    target: OpenRouterResponsesTarget,
    client: Client,
}

struct OpenRouterDispatchGuard {
    generation: OpenRouterApiKeyGenerationGrant,
}

/// OpenRouter model backed exclusively by shared Responses request, attempt,
/// stream, terminal projection, and artifact hosts.
pub struct OpenRouterResponsesLanguageModel {
    probe: plakous_model::ReachabilityProbe,
    target: OpenRouterResponsesTarget,
    config: OpenRouterResponsesHttpConfig,
    attempt_data: ResponsesAttemptDialectData,
    service: Arc<ResponsesAttemptService<OpenRouterResponsesExecution, OpenRouterDispatchGuard>>,
    descriptor: ResolvedModelDescriptor,
    configuration: CompiledProviderConfiguration,
    clock: Arc<dyn Clock>,
}

impl OpenRouterResponsesLanguageModel {
    /// Builds a configured GatewayManaged Custom model from complete behavior.
    pub fn configured_gateway_managed_behavior(
        route: crate::OpenRouterChatRoute,
        evidence: crate::OpenRouterChatModelEvidence,
        context_contract: plakous_model::ContextContract,
        behavior: ProviderBehaviorConfiguration,
        client: Client,
    ) -> Result<ConfiguredLanguageModel, crate::OpenRouterResponsesBuildError> {
        let target = OpenRouterResponsesTarget::gateway_managed(route, evidence, context_contract)?;
        let descriptor = target.descriptor();
        let selected = SelectedProviderConfiguration::new(
            ProviderRouteSelection::new(
                descriptor.protocol().clone(),
                descriptor.route().clone(),
                descriptor.profile().clone(),
                ProviderProfileRevisionId::new(OPENROUTER_INTEGRATION_REVISION)?,
                ModelAdapterProfileId::new(OPENROUTER_AUTH_PROFILE)?,
                ProviderProfileRevisionId::new(OPENROUTER_AUTH_REVISION)?,
                ProviderAuthorityClassId::new(OPENROUTER_AUTHORITY_CLASS)?,
                CredentialSelectionId::new(OPENROUTER_CREDENTIAL_SELECTION)?,
                descriptor.requested_model().clone(),
            ),
            behavior,
        )?;
        // Name the failing fact: the shared-host constructor below folds every
        // target and compile refusal into one construction error.
        let runtime = target.runtime_authority(SystemClock.now())?;
        compile_openrouter_responses_configuration(&selected, &target, &runtime)?;
        let model = Arc::new(
            Self::new(target, selected, client)
                .map_err(|_| crate::OpenRouterResponsesBuildError::Configuration)?,
        );
        let authority = model.target.runtime_authority(model.clock.now())?;
        ConfiguredLanguageModel::custom(model.clone(), model.configuration.clone(), authority)
            .map_err(crate::OpenRouterResponsesBuildError::from)
    }

    /// Binds one exact dynamic Custom target to the shared family host.
    pub fn new(
        target: OpenRouterResponsesTarget,
        selected: SelectedProviderConfiguration,
        client: Client,
    ) -> Result<Self, OpenRouterResponsesLanguageModelConfigError> {
        Self::with_config(
            target,
            selected,
            client,
            OpenRouterResponsesHttpConfig::default(),
        )
    }

    /// Binds one exact dynamic target with explicit shared-host bounds.
    pub fn with_config(
        target: OpenRouterResponsesTarget,
        selected: SelectedProviderConfiguration,
        client: Client,
        config: OpenRouterResponsesHttpConfig,
    ) -> Result<Self, OpenRouterResponsesLanguageModelConfigError> {
        if config.operation_timeout.is_zero() || config.event_channel_capacity == 0 {
            return Err(OpenRouterResponsesLanguageModelConfigError::InvalidPolicy);
        }
        let route = target.route();
        let descriptor = target.descriptor().clone();
        let checked_at = SystemClock.now();
        let generation = route
            .current_credential_generation_grant()
            .map_err(|_| OpenRouterResponsesLanguageModelConfigError::Configuration)?;
        let runtime = target
            .runtime_authority(checked_at)
            .map_err(|_| OpenRouterResponsesLanguageModelConfigError::Configuration)?;
        if generation_number(generation) != runtime.credential_generation() {
            return Err(OpenRouterResponsesLanguageModelConfigError::Configuration);
        }
        let configuration =
            compile_openrouter_responses_configuration(&selected, &target, &runtime)
                .map_err(|_| OpenRouterResponsesLanguageModelConfigError::Configuration)?;
        let dialect = OpenRouterDialect::ResponsesV1;
        let execution = Arc::new(OpenRouterResponsesExecution {
            target: target.clone(),
            client: client.clone(),
        });
        let authorization = ResponsesAuthorizationAdapter::new(
            PREFLIGHT_REVISION,
            FINAL_AUTHORIZATION_REVISION,
            openrouter_preflight,
            revalidate_openrouter,
        )
        .map_err(|_| OpenRouterResponsesLanguageModelConfigError::SharedBinding)?;
        let attempt_target = ResponsesAttemptTarget::from_admitted_target(
            &descriptor,
            route.identity().account_binding(),
            openrouter_semantic_revisions()?,
        )
        .map_err(|_| OpenRouterResponsesLanguageModelConfigError::SharedBinding)?;
        let replay = dialect
            .replay_projection(
                descriptor.protocol().clone(),
                descriptor.model_family().cloned(),
                descriptor.requested_model().clone(),
            )
            .map_err(|_| OpenRouterResponsesLanguageModelConfigError::SharedBinding)?;
        let retention = route
            .identity()
            .retention_policy()
            .ok_or(OpenRouterResponsesLanguageModelConfigError::Configuration)?;
        let router = plakous_responses_dialect::ResponsesRouterPolicy::gateway_managed(
            retention.data_collection().map(str::to_owned),
            retention.zdr(),
        )
        .map_err(|_| OpenRouterResponsesLanguageModelConfigError::SharedBinding)?;
        let attempt_data = ResponsesAttemptDialectData::new(
            OPENROUTER_RESPONSES_DIALECT_REVISION,
            attempt_target,
            &client,
            &authorization,
            LedgerPolicy::RESPONSES_FAMILY,
            ResponsesAuthSlots::Primary,
            dialect.request_policy(),
            dialect.stream_data(descriptor.requested_model()),
            config.responses_limits,
            replay,
            ResponsesStorageMode::Stateless,
            ResponsesProviderErrorPolicy::OpenRouter,
            ResponsesResponseLimits::default(),
            Some(HEADER_GENERATION_ID),
            false,
        )
        .and_then(|data| data.with_router_policy(router))
        .map_err(|_| OpenRouterResponsesLanguageModelConfigError::SharedBinding)?;
        let probe = plakous_responses_dialect::bind_reachability_probe(
            client.clone(),
            Origin::parse(route.identity().origin().as_str())
                .map_err(|_| OpenRouterResponsesLanguageModelConfigError::SharedBinding)?,
            descriptor.route().clone(),
        );
        let service = Arc::new(
            ResponsesAttemptService::new(client, execution, attempt_data.clone(), authorization)
                .map_err(|_| OpenRouterResponsesLanguageModelConfigError::SharedBinding)?,
        );
        Ok(Self {
            probe,
            target,
            config,
            attempt_data,
            service,
            descriptor,
            configuration,
            clock: Arc::new(SystemClock),
        })
    }

    /// Returns the exact admitted target.
    #[must_use]
    pub const fn target(&self) -> &OpenRouterResponsesTarget {
        &self.target
    }

    /// Returns the exact selected-to-effective provider configuration.
    #[must_use]
    pub const fn configuration(&self) -> &CompiledProviderConfiguration {
        &self.configuration
    }
}

impl fmt::Debug for OpenRouterResponsesLanguageModel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenRouterResponsesLanguageModel")
            .field("target", &self.target)
            .field("config", &self.config)
            .field("execution", &"shared Responses host")
            .finish_non_exhaustive()
    }
}

impl LanguageModel for OpenRouterResponsesLanguageModel {
    fn reachability_probe(&self) -> Option<plakous_model::ReachabilityProbe> {
        Some(self.probe.clone())
    }

    fn revalidate_runtime_authority(
        self: Arc<Self>,
        checked_at: Timestamp,
    ) -> RuntimeModelAuthorityOperation {
        Box::pin(async move {
            self.target
                .runtime_authority(checked_at)
                .map_err(|error| match error {
                    crate::OpenRouterResponsesBuildError::StaleEvidence => {
                        RuntimeModelAuthorityError::Stale
                    }
                    crate::OpenRouterResponsesBuildError::Authentication(_) => {
                        RuntimeModelAuthorityError::Unavailable
                    }
                    _ => RuntimeModelAuthorityError::ForeignAuthority,
                })
        })
    }

    fn prepare_call(
        self: Arc<Self>,
        plan: ValidatedRequestPlan,
        context: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError> {
        self.target
            .runtime_authority(context.prepared_at())
            .map_err(|_| {
                rejection(
                    ModelPreparationErrorKind::InvalidConfiguration,
                    "OpenRouter Responses discovery evidence is stale or foreign",
                )
            })?;
        self.descriptor.validate_plan_binding(&plan)?;
        if plan.request().selected_provider_configuration_id()
            != self.configuration.selected_identity()
            || plan.request().effective_provider_configuration_id()
                != self.configuration.effective_identity()
        {
            return Err(rejection(
                ModelPreparationErrorKind::InvalidConfiguration,
                "OpenRouter request provider configuration is foreign",
            ));
        }
        if !request_matches_configuration(plan.request(), self.configuration.selected().behavior())
        {
            return Err(rejection(
                ModelPreparationErrorKind::InvalidConfiguration,
                "OpenRouter request behavior differs from compiled configuration",
            ));
        }
        self.descriptor
            .validate_plan_admission(plan.request(), &plan)?;
        context
            .authority()
            .validate(&self.descriptor, context.prepared_at())?;
        let route = self.target.route();
        if context.authority().account_binding() != route.identity().account_binding()
            || context.authority().credential_authority_binding()
                != route.identity().credential_authority_binding()
        {
            return Err(RuntimeModelAuthorityError::ForeignAuthority.into());
        }
        let request = plan.request();
        if request
            .tools()
            .iter()
            .any(|tool| is_unprojected_string_custom_tool_definition(request, tool.definition()))
        {
            return Err(rejection(
                ModelPreparationErrorKind::InvalidRequest,
                "OpenRouter does not admit custom tools",
            ));
        }
        let canonical_tools = canonical_local_tools(request);
        let canonical = ResponsesCanonicalDialectData::openrouter(
            self.descriptor.requested_model().clone(),
            canonical_tools,
        )
        .map_err(|_| {
            rejection(
                ModelPreparationErrorKind::InvalidConfiguration,
                "OpenRouter canonical projection facts are invalid",
            )
        })?;
        let authority = ResponsesRequestAuthority::from_admitted_operation(
            &self.descriptor,
            &plan,
            request,
            context.authority().account_binding(),
            openrouter_semantic_revisions().map_err(|_| {
                rejection(
                    ModelPreparationErrorKind::InvalidConfiguration,
                    "OpenRouter semantic revisions are invalid",
                )
            })?,
            context
                .cache_affinity()
                .map(plakous_model::ModelCacheAffinity::identity),
            ResponsesStoragePolicy::Stateless,
        )
        .map_err(|error| {
            let kind = if matches!(
                error,
                plakous_responses_dialect::ResponsesReplayError::IncompleteAuthority
            ) {
                ModelPreparationErrorKind::InvalidConfiguration
            } else {
                ModelPreparationErrorKind::InvalidRequest
            };
            rejection(
                kind,
                "OpenRouter request authority does not match the operation",
            )
        })?;
        let replay_bridge = context
            .active_family_artifact()
            .map(|active| {
                ResponsesArtifactReplayBridge::from_active_artifact(
                    active,
                    request,
                    authority.clone(),
                    plakous_responses_dialect::ResponsesReplayKind::new(
                        crate::dialect::OPENROUTER_OUTPUT_ITEM_REPLAY_KIND,
                        crate::dialect::OPENROUTER_OUTPUT_ITEM_REPLAY_VERSION,
                    ),
                    None,
                    None,
                    context.prepared_at(),
                )
            })
            .transpose()
            // An artifact covering a different canonical or stable prefix is
            // not this request's, so prepare fresh rather than refuse.
            .or_else(|error: plakous_responses_dialect::ResponsesRequestError| {
                error
                    .artifact_is_not_for_this_request()
                    .then_some(None)
                    .ok_or(error)
            })
            .map_err(|_| {
                rejection(
                    ModelPreparationErrorKind::InvalidRequest,
                    "OpenRouter active Responses artifact is not reusable",
                )
            })?;
        let artifact_binding = authority.artifact_binding().clone();
        let prepared_attempt = prepare_responses_attempt(
            authority,
            request,
            replay_bridge
                .as_ref()
                .map(ResponsesArtifactReplayBridge::context),
            &self.attempt_data,
        )
        .map_err(|error| {
            rejection(
                error.preparation_error_kind(),
                format!("OpenRouter shared request lowering failed: {error}"),
            )
        })?;
        let wire_digest = prepared_attempt.wire_digest();
        let wire_size = u64::try_from(prepared_attempt.wire_size()).map_err(|_| {
            rejection(
                ModelPreparationErrorKind::ResourceLimit,
                "OpenRouter prepared request exceeds addressable size",
            )
        })?;
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
                .map_err(|_| {
                    rejection(
                        ModelPreparationErrorKind::InvalidConfiguration,
                        "OpenRouter local context measurement is invalid",
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
        let output_limit = prepared_attempt
            .enforced_output_tokens()
            .map(|tokens| {
                PreparedOutputLimit::enforced_wire_maximum(
                    tokens,
                    "max_output_tokens",
                    wire_digest,
                    OPENROUTER_ADAPTER_REVISION,
                )
            })
            .transpose()?;
        let cache_use = match (
            context.active_family_artifact(),
            context
                .cache_affinity()
                .map(plakous_model::ModelCacheAffinity::identity),
        ) {
            (Some(artifact), Some(affinity)) => {
                PreparedCacheUse::ActiveFamilyArtifactAndRuntimeAffinity {
                    artifact: artifact.integrity(),
                    affinity,
                }
            }
            (Some(artifact), None) => PreparedCacheUse::ActiveFamilyArtifact {
                artifact: artifact.integrity(),
            },
            (None, Some(affinity)) => PreparedCacheUse::RuntimeAffinity { affinity },
            (None, None) => PreparedCacheUse::None,
        };
        let projection = PreparedModelProjection::new(
            &self.descriptor,
            &plan,
            &context,
            ProtocolId::new(RESPONSES_FAMILY).map_err(|_| {
                rejection(
                    ModelPreparationErrorKind::InvalidConfiguration,
                    "Responses family identity is invalid",
                )
            })?,
            OPENROUTER_ADAPTER_REVISION,
            OPENROUTER_PROJECTION_REVISION,
            OPENROUTER_ACCOUNTING_REVISION,
            wire_digest,
            wire_size,
            token_projection,
            cache_use,
            output_limit,
        )?;
        let record =
            PreparedCallRecord::for_prepared(&self.descriptor, &plan, projection, &context)?;
        let execution = PreparedResponsesModelExecution::new(
            prepared_attempt,
            canonical,
            CanonicalIdSources::with_clock(Arc::clone(&self.clock)),
            CanonicalIdSources::with_clock(Arc::clone(&self.clock)),
            replay_bridge
                .as_ref()
                .map(|bridge| bridge.hosted_programs().clone())
                .unwrap_or_default(),
            plan,
            artifact_binding,
            Arc::clone(&self.clock),
            self.descriptor.clone(),
            self.config.event_channel_capacity,
            None,
        )
        .map_err(|_| {
            rejection(
                ModelPreparationErrorKind::InvalidConfiguration,
                "OpenRouter prepared execution facts are inconsistent",
            )
        })?;
        let service = Arc::clone(&self.service);
        let clock = Arc::clone(&self.clock);
        let timeout = self.config.operation_timeout;
        Ok(PreparedModelCall::new(record, move |call_context| {
            let deadline = operation_deadline(clock.as_ref(), timeout, None);
            start_prepared_responses_model_call(execution, service, call_context, deadline)
        }))
    }
}

fn openrouter_preflight<'a>(
    dialect: &'a OpenRouterResponsesExecution,
    request: ResponsesPreflightRequest<'a>,
) -> ResponsesPreflightFuture<'a, OpenRouterDispatchGuard> {
    Box::pin(async move {
        if request.cancellation().is_cancelled() || Instant::now() >= request.deadline() {
            return Err(ResponsesDispatchRejection::Unavailable);
        }
        dialect
            .target
            .runtime_authority(SystemClock.now())
            .map_err(|_| ResponsesDispatchRejection::Binding)?;
        let route = dialect.target.route();
        let body: serde_json::Value = serde_json::from_slice(request.body_bytes())
            .map_err(|_| ResponsesDispatchRejection::Binding)?;
        if body.get("stream") != Some(&serde_json::Value::Bool(true))
            || body.get("store") != Some(&serde_json::Value::Bool(false))
            || body.get("provider").is_none()
        {
            return Err(ResponsesDispatchRejection::Binding);
        }
        let generation = route
            .responses_credential_generation_grant(request.physical())
            .map_err(|_| ResponsesDispatchRejection::Authentication)?;
        let identity = route.identity();
        let key = route
            .responses_credential(request.physical(), generation)
            .map_err(|_| ResponsesDispatchRejection::Authentication)?;
        let authorization = sensitive_header_value(key.bearer_bytes())
            .map_err(|_| ResponsesDispatchRejection::Binding)?;
        let mut headers = HeaderMap::new();
        headers.insert(HEADER_ACCEPT, HeaderValue::from_static("text/event-stream"));
        headers.insert(
            HEADER_CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        headers.insert(
            HEADER_OPENROUTER_METADATA,
            HeaderValue::from_static("enabled"),
        );
        headers.insert(HEADER_AUTHORIZATION, authorization);
        let mut bindings = vec![
            ResponsesHeaderBinding::PublicExact {
                name: HEADER_ACCEPT,
                value: b"text/event-stream".to_vec(),
            },
            ResponsesHeaderBinding::PublicExact {
                name: HEADER_CONTENT_TYPE,
                value: b"application/json".to_vec(),
            },
            ResponsesHeaderBinding::PublicExact {
                name: HEADER_OPENROUTER_METADATA,
                value: b"enabled".to_vec(),
            },
            ResponsesHeaderBinding::CredentialSecret {
                name: HEADER_AUTHORIZATION,
                slot: ResponsesAuthSlot::Primary,
            },
        ];
        if let Some(attribution) = identity.attribution() {
            headers.insert(
                HEADER_HTTP_REFERER,
                HeaderValue::from_static(attribution.referer()),
            );
            headers.insert(HEADER_TITLE, HeaderValue::from_static(attribution.title()));
            headers.insert(
                HEADER_CATEGORIES,
                HeaderValue::from_static(crate::OpenRouterAttribution::CATEGORY),
            );
            bindings.push(ResponsesHeaderBinding::PublicExact {
                name: HEADER_CATEGORIES,
                value: crate::OpenRouterAttribution::CATEGORY.as_bytes().to_vec(),
            });
            bindings.push(ResponsesHeaderBinding::PublicExact {
                name: HEADER_HTTP_REFERER,
                value: attribution.referer().as_bytes().to_vec(),
            });
            bindings.push(ResponsesHeaderBinding::PublicExact {
                name: HEADER_TITLE,
                value: attribution.title().as_bytes().to_vec(),
            });
        }
        let wire = Request::new(
            Origin::parse(identity.origin().as_str())
                .map_err(|_| ResponsesDispatchRejection::Binding)?,
            Method::POST,
            crate::OPENROUTER_RESPONSES_PATH,
            headers,
            SensitiveBytes::new(request.body_bytes().to_vec()).into_bytes(),
            dialect.client.limits(),
        )
        .map_err(|_| ResponsesDispatchRejection::Binding)?;
        Ok(ResponsesPreparedPhysicalRequest::new(
            request.physical().clone(),
            wire,
            ResponsesCredentialAuthority::primary(generation.fingerprint()),
            bindings,
            OpenRouterDispatchGuard { generation },
        ))
    })
}

fn revalidate_openrouter(
    dialect: &OpenRouterResponsesExecution,
    mut request: ResponsesRevalidationRequest<'_, OpenRouterDispatchGuard>,
) -> Result<(), ResponsesDispatchRejection> {
    if request.cancellation().is_cancelled() || Instant::now() >= request.deadline() {
        return Err(ResponsesDispatchRejection::Unavailable);
    }
    dialect
        .target
        .runtime_authority(SystemClock.now())
        .map_err(|_| ResponsesDispatchRejection::Binding)?;
    let route = dialect.target.route();
    let generation = request.response_head_guard_mut().generation;
    route
        .revalidate_responses_credential_generation(request.binding().physical(), generation)
        .map_err(|_| ResponsesDispatchRejection::Authentication)?;
    if request
        .binding()
        .credential_authority()
        .generation(ResponsesAuthSlot::Primary)
        != Some(generation.fingerprint())
    {
        return Err(ResponsesDispatchRejection::Binding);
    }
    Ok(())
}

fn openrouter_semantic_revisions()
-> Result<ResponsesSemanticRevisions, OpenRouterResponsesLanguageModelConfigError> {
    ResponsesSemanticRevisions::new(1, 1, 1)
        .map_err(|_| OpenRouterResponsesLanguageModelConfigError::SharedBinding)
}

pub(crate) fn request_matches_configuration(
    request: &plakous_model::ModelRequest,
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
        && request_tool_choice_matches(request.tool_choice(), &behavior.tool_choice)
        && request.context_admission_requirement() == behavior.context_evidence_requirement
        && request.limits().stop_sequences().is_empty()
        && request.native().entries().is_empty()
        && request.hosted_tools() == behavior.hosted_tools
        && request.tools().len() == behavior.tools.len()
        && request
            .tools()
            .iter()
            .zip(&behavior.tools)
            .all(|(request, configured)| request == configured)
        && request_output_bound_matches(request.limits().max_output_tokens(), behavior.output_bound)
}

/// Compact keeps the compiled coding `tool_choice` and restricts
/// the request to [`plakous_model::ToolChoice::None`]. Equality would mark
/// that restriction foreign.
fn request_tool_choice_matches(
    request: &plakous_model::ToolChoice,
    configured: &plakous_model::ToolChoice,
) -> bool {
    request == configured || *request == plakous_model::ToolChoice::None
}

/// Compact also restricts max output below the compiled coding bound
/// (`SEMANTIC_COMPACTION_MAX_OUTPUT_TOKENS` vs `NATIVE_RUN_MAX_OUTPUT_TOKENS`).
fn request_output_bound_matches(
    requested: Option<std::num::NonZeroU32>,
    configured: OutputBoundDisposition,
) -> bool {
    configured.bound().map_or_else(
        || requested.is_none(),
        |bound| requested.is_some_and(|requested| requested.get() <= bound.get()),
    )
}

fn generation_number(grant: OpenRouterApiKeyGenerationGrant) -> u64 {
    let fingerprint = grant.fingerprint();
    let bytes = fingerprint.as_bytes();
    u64::from_be_bytes(bytes[..8].try_into().unwrap_or([0; 8])).max(1)
}

fn operation_deadline(
    clock: &dyn Clock,
    timeout: Duration,
    requested: Option<Timestamp>,
) -> Instant {
    let now = Instant::now();
    let configured = now.checked_add(timeout).unwrap_or(now);
    let Some(requested) = requested else {
        return configured;
    };
    let wall_now = clock.now();
    if requested <= wall_now {
        return now;
    }
    now.checked_add(Duration::from_secs(
        requested.seconds().saturating_sub(wall_now.seconds()),
    ))
    .map_or(configured, |deadline| deadline.min(configured))
}

fn rejection(kind: ModelPreparationErrorKind, message: impl AsRef<str>) -> ModelPreparationError {
    ModelPreparationError::rejected(kind, message)
}

#[cfg(test)]
mod tests {
    use plakous_model::ToolChoice;

    #[test]
    fn compact_tool_choice_none_matches_coding_auto() {
        assert!(super::request_tool_choice_matches(
            &ToolChoice::None,
            &ToolChoice::Auto
        ));
        assert!(super::request_tool_choice_matches(
            &ToolChoice::Auto,
            &ToolChoice::Auto
        ));
        assert!(!super::request_tool_choice_matches(
            &ToolChoice::Auto,
            &ToolChoice::None
        ));
    }

    #[test]
    fn compact_output_bound_may_restrict_coding_maximum() -> Result<(), &'static str> {
        use plakous_model::OutputBoundDisposition;
        use std::num::NonZeroU32;
        let coding = NonZeroU32::new(32_768).ok_or("coding max")?;
        let compact = plakous_model::SEMANTIC_COMPACTION_MAX_OUTPUT_TOKENS;
        assert!(super::request_output_bound_matches(
            Some(compact),
            OutputBoundDisposition::Enforced(coding)
        ));
        assert!(super::request_output_bound_matches(
            Some(coding),
            OutputBoundDisposition::Enforced(coding)
        ));
        assert!(!super::request_output_bound_matches(
            Some(coding),
            OutputBoundDisposition::Enforced(compact)
        ));
        Ok(())
    }
}
