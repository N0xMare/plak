//! Runnable OpenRouter composition over the shared Responses family hosts.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use plakous_attempt::LedgerPolicy;
use plakous_http::{Client, HeaderName, Origin, sensitive_header_value};
use plakous_model::{
    CapabilityRequirement, Clock, CompiledProviderConfiguration, ConfiguredLanguageModel,
    CredentialSelectionId, LanguageModel, ModelAdapterProfileId, ModelPreparationContext,
    ModelPreparationError, ModelPreparationErrorKind, OutputBoundDisposition, PreparedCacheUse,
    PreparedModelCall, ProviderAuthorityClassId, ProviderBehaviorConfiguration,
    ProviderProfileRevisionId, ProviderRouteSelection, ReasoningEffortDisposition, ReasoningPolicy,
    ResolvedModelDescriptor, RuntimeModelAuthorityError, RuntimeModelAuthorityOperation,
    SelectedProviderConfiguration, Timestamp, ValidatedRequestPlan,
};
use plakous_responses::api_key::{
    Preparation, Wire, configuration_matches, dispatch_available, fingerprint_matches, prepare,
    request_authority, seal, stateless_stream_body,
};
use plakous_responses::{
    ResponsesAttemptDialectData, ResponsesAttemptService, ResponsesAttemptTarget,
    ResponsesAuthSlots, ResponsesAuthorizationAdapter, ResponsesCanonicalDialectData,
    ResponsesDispatchRejection, ResponsesPreflightFuture, ResponsesPreflightRequest,
    ResponsesProviderErrorPolicy, ResponsesReplayKind, ResponsesRequestAuthority,
    ResponsesResponseLimits, ResponsesRevalidationRequest, canonical_local_tools,
    is_unprojected_string_custom_tool_definition,
};
use plakous_responses::{ResponsesSemanticRevisions, ResponsesStorageMode, ResponsesStreamLimits};
use thiserror::Error;

use crate::{
    OpenRouterDialect, OpenRouterKeyGeneration, OpenRouterResponsesTarget,
    compile_openrouter_responses_configuration, dialect::OPENROUTER_RESPONSES_DIALECT_REVISION,
};

const DEFAULT_OPERATION_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const DEFAULT_EVENT_CHANNEL_CAPACITY: usize = 64;
const PREFLIGHT_REVISION: u32 = 1;
const FINAL_AUTHORIZATION_REVISION: u32 = 1;
pub(crate) const OPENROUTER_ADAPTER_REVISION: &str = "plakous-openrouter-responses-adapter-v2";
const OPENROUTER_PROJECTION_REVISION: &str = "responses-canonical-openrouter-v1";
const OPENROUTER_ACCOUNTING_REVISION: &str = "responses-wire-bytes-openrouter-v1";
const OPENROUTER_INTEGRATION_REVISION: &str = "openrouter-responses-integration-v1";
const OPENROUTER_AUTH_PROFILE: &str = "openrouter.platform-api-key";
const OPENROUTER_AUTH_REVISION: &str = "openrouter-platform-api-key-v1";
const OPENROUTER_AUTHORITY_CLASS: &str = "platform-api-key";
const OPENROUTER_CREDENTIAL_SELECTION: &str = "openrouter-product-key-generation";

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
    generation: OpenRouterKeyGeneration,
}

struct OpenRouterDispatchGuard {
    generation: OpenRouterKeyGeneration,
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
        let runtime = target.runtime_authority(target.route().clock.now())?;
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
        let checked_at = target.route().clock.now();
        let generation = route
            .current_credential_generation_grant()
            .map_err(|_| OpenRouterResponsesLanguageModelConfigError::Configuration)?;
        let runtime = target
            .runtime_authority(checked_at)
            .map_err(|_| OpenRouterResponsesLanguageModelConfigError::Configuration)?;
        if generation_number(&generation) != runtime.credential_generation() {
            return Err(OpenRouterResponsesLanguageModelConfigError::Configuration);
        }
        let configuration =
            compile_openrouter_responses_configuration(&selected, &target, &runtime)
                .map_err(|_| OpenRouterResponsesLanguageModelConfigError::Configuration)?;
        let dialect = OpenRouterDialect::ResponsesV1;
        let execution = Arc::new(OpenRouterResponsesExecution {
            target: target.clone(),
            client: client.clone(),
            generation: generation.clone(),
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
                route.identity().account_binding(),
            )
            .map_err(|_| OpenRouterResponsesLanguageModelConfigError::SharedBinding)?;
        let retention = route
            .identity()
            .retention_policy()
            .ok_or(OpenRouterResponsesLanguageModelConfigError::Configuration)?;
        let router = plakous_responses::ResponsesRouterPolicy::gateway_managed(
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
        let probe = plakous_responses::bind_reachability_probe(
            client.clone(),
            Origin::parse(route.identity().origin().as_str())
                .map_err(|_| OpenRouterResponsesLanguageModelConfigError::SharedBinding)?,
            descriptor.route().clone(),
        );
        let service = Arc::new(
            ResponsesAttemptService::new(client, execution, attempt_data.clone(), authorization)
                .map_err(|_| OpenRouterResponsesLanguageModelConfigError::SharedBinding)?,
        );
        let clock = Arc::clone(&target.route().clock);
        Ok(Self {
            probe,
            target,
            config,
            attempt_data,
            service,
            descriptor,
            configuration,
            clock,
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
        self.admit_preparation(&plan, &context)?;
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
        let authority = self.request_authority(&plan, &context)?;
        prepare(Preparation {
            label: "OpenRouter",
            descriptor: &self.descriptor,
            plan,
            context: &context,
            attempt: &self.attempt_data,
            canonical,
            authority,
            output_item_replay: ResponsesReplayKind::new(
                crate::dialect::OPENROUTER_OUTPUT_ITEM_REPLAY_KIND,
                crate::dialect::OPENROUTER_OUTPUT_ITEM_REPLAY_VERSION,
            ),
            previous_response_replay: None,
            adapter_revision: OPENROUTER_ADAPTER_REVISION,
            projection_revision: OPENROUTER_PROJECTION_REVISION,
            accounting_revision: OPENROUTER_ACCOUNTING_REVISION,
            cache_use: openrouter_cache_use(&context),
            observation_capacity: self.config.event_channel_capacity,
            clock: Arc::clone(&self.clock),
            service: Arc::clone(&self.service),
            operation_timeout: self.config.operation_timeout,
        })
    }
}

impl OpenRouterResponsesLanguageModel {
    fn admit_preparation(
        &self,
        plan: &ValidatedRequestPlan,
        context: &ModelPreparationContext,
    ) -> Result<(), ModelPreparationError> {
        self.target
            .runtime_authority(context.prepared_at())
            .map_err(|_| {
                rejection(
                    ModelPreparationErrorKind::InvalidConfiguration,
                    "OpenRouter Responses discovery evidence is stale or foreign",
                )
            })?;
        self.descriptor.validate_plan_binding(plan)?;
        if !configuration_matches(plan, &self.configuration) {
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
            .validate_plan_admission(plan.request(), plan)?;
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
        Ok(())
    }

    fn request_authority(
        &self,
        plan: &ValidatedRequestPlan,
        context: &ModelPreparationContext,
    ) -> Result<ResponsesRequestAuthority, ModelPreparationError> {
        request_authority(
            &self.descriptor,
            plan,
            context,
            openrouter_semantic_revisions().map_err(|_| {
                rejection(
                    ModelPreparationErrorKind::InvalidConfiguration,
                    "OpenRouter semantic revisions are invalid",
                )
            })?,
        )
        .map_err(|error| {
            let kind = if matches!(
                error,
                plakous_responses::ResponsesReplayError::IncompleteAuthority
            ) {
                ModelPreparationErrorKind::InvalidConfiguration
            } else {
                ModelPreparationErrorKind::InvalidRequest
            };
            rejection(
                kind,
                "OpenRouter request authority does not match the operation",
            )
        })
    }
}

fn openrouter_preflight<'a>(
    dialect: &'a OpenRouterResponsesExecution,
    request: ResponsesPreflightRequest<'a>,
) -> ResponsesPreflightFuture<'a, OpenRouterDispatchGuard> {
    Box::pin(async move {
        dispatch_available(request.cancellation().is_cancelled(), request.deadline())?;
        dialect
            .target
            .runtime_authority(dialect.target.route().clock.now())
            .map_err(|_| ResponsesDispatchRejection::Binding)?;
        let route = dialect.target.route();
        let body = stateless_stream_body(request.body_bytes(), None)?;
        if body.get("provider").is_none() {
            return Err(ResponsesDispatchRejection::Binding);
        }
        let generation = route
            .responses_credential_generation_grant(request.physical())
            .map_err(|_| ResponsesDispatchRejection::Authentication)?;
        if generation != dialect.generation {
            return Err(ResponsesDispatchRejection::Authentication);
        }
        let identity = route.identity();
        let key = route
            .responses_credential(request.physical(), &generation)
            .map_err(|_| ResponsesDispatchRejection::Authentication)?;
        let authorization = sensitive_header_value(crate::route::bearer_bytes(&key))
            .map_err(|_| ResponsesDispatchRejection::Binding)?;
        // Extras follow authorization. Attribution stays referer, title, categories.
        // Physical identity sorts header frames, so this order does not change the digest.
        let mut extra_headers = vec![(HEADER_OPENROUTER_METADATA, b"enabled".to_vec())];
        if let Some(attribution) = identity.attribution() {
            extra_headers.push((
                HEADER_HTTP_REFERER,
                attribution.referer().as_bytes().to_vec(),
            ));
            extra_headers.push((HEADER_TITLE, attribution.title().as_bytes().to_vec()));
            extra_headers.push((
                HEADER_CATEGORIES,
                crate::OpenRouterAttribution::CATEGORY.as_bytes().to_vec(),
            ));
        }
        seal(Wire {
            origin: Origin::parse(identity.origin().as_str())
                .map_err(|_| ResponsesDispatchRejection::Binding)?,
            path: crate::OPENROUTER_RESPONSES_PATH,
            authorization,
            extra_headers,
            body: request.body_bytes().to_vec(),
            limits: dialect.client.limits().clone(),
            fingerprint: generation.fingerprint(),
            physical: request.physical().clone(),
            guard: OpenRouterDispatchGuard { generation },
        })
    })
}

fn revalidate_openrouter(
    dialect: &OpenRouterResponsesExecution,
    mut request: ResponsesRevalidationRequest<'_, OpenRouterDispatchGuard>,
) -> Result<(), ResponsesDispatchRejection> {
    dispatch_available(request.cancellation().is_cancelled(), request.deadline())?;
    dialect
        .target
        .runtime_authority(dialect.target.route().clock.now())
        .map_err(|_| ResponsesDispatchRejection::Binding)?;
    let route = dialect.target.route();
    let generation = request.response_head_guard_mut().generation.clone();
    route
        .revalidate_responses_credential_generation(request.binding().physical(), &generation)
        .map_err(|_| ResponsesDispatchRejection::Authentication)?;
    fingerprint_matches(
        request.binding().credential_authority(),
        generation.fingerprint(),
    )
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

fn generation_number(grant: &OpenRouterKeyGeneration) -> u64 {
    let fingerprint = grant.fingerprint();
    let bytes = fingerprint.as_bytes();
    u64::from_be_bytes(bytes[..8].try_into().unwrap_or([0; 8])).max(1)
}

fn openrouter_cache_use(context: &ModelPreparationContext) -> PreparedCacheUse {
    match (
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
    }
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
