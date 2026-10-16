//! Thin dynamic OpenRouter Messages dialect over the shared Messages kernel.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::Stream;
use plakous_attempt::PhysicalRequestBinding;
use plakous_http::{
    Client, Failure as HttpFailure, HeaderMap, HeaderName, HeaderValue, Method, Origin, Request,
    Response, SensitiveBytes, StatusCode, sensitive_header_value,
};
use plakous_messages::{
    MessagesAttempt, MessagesAttemptError, MessagesAttemptProtocolError,
    MessagesHttpAttemptService, MessagesHttpErrorHead, MessagesHttpHooks, MessagesHttpResponseHead,
    MessagesHttpSafety, MessagesOperationBinding, MessagesRequestErrorKind, MessagesRequestPolicy,
    MessagesRouterPolicy, MessagesStreamDialectHooks, MessagesStreamError,
    MessagesStreamExtensions, PreparedMessagesHttpOperation, PreparedMessagesStreamBinding,
    ToolOwnership, call_messages_operation, lower_request_with_active_artifact,
};
use plakous_model::{
    Capability, CapabilityConstraints, CapabilitySet, Clock, CompiledProviderConfiguration,
    ConfiguredLanguageModel, ContextContract, ContextEvidenceClass, CountObservation,
    CredentialSelectionId, Digest, DigestDomain, EffectiveProviderConfiguration, FactProvenance,
    FactSource, FamilyPolicyEnvelope, FeatureSupport, LanguageModel, ModelAdapterProfileId,
    ModelDiagnostic, ModelError, ModelErrorKind, ModelEvent, ModelFamilyId,
    ModelImplementationIdentity, ModelPreparationContext, ModelPreparationError,
    ModelPreparationErrorKind, NamespaceId, PreparedCacheUse, PreparedCallRecord,
    PreparedModelCall, PreparedModelProjection, PreparedOutputLimit, PreparedTokenProjection,
    ProtocolId, ProviderAuthorityClassId, ProviderBehaviorConfiguration, ProviderProfileRevisionId,
    ProviderRouteSelection, ProviderSemanticEvidence, ResolvedModelDescriptor,
    ResolvedProviderAuthorityBinding, RuntimeModelAuthority, RuntimeModelAuthorityError,
    RuntimeModelAuthorityOperation, SelectedProviderConfiguration, Support, SystemClock,
    SystemIdGenerator, Timestamp, ValidatedRequestPlan,
};
use thiserror::Error;

use crate::{
    OpenRouterApiKeyGenerationGrant, OpenRouterChatModelEvidence, OpenRouterChatRoute,
    OpenRouterRetentionPolicy,
};

/// Exact OpenRouter Messages resource path.
pub const OPENROUTER_MESSAGES_PATH: &str = "/api/v1/messages";
/// Exact reviewed OpenRouter Messages dialect identity.
pub const OPENROUTER_MESSAGES_PROTOCOL_ID: &str = "openrouter.messages.v1";
/// Provider-neutral Messages family identity.
pub const MESSAGES_FAMILY_ID: &str = "messages-family";
/// OpenRouter Messages dialect revision.
pub(crate) const OPENROUTER_MESSAGES_DIALECT_REVISION: u32 = 1;

pub(crate) const PROFILE: &str = "openrouter.messages.dynamic-custom-v1";
const MODEL_FAMILY: &str = "openrouter.messages";
const IMPLEMENTATION: &str = "plakous.openrouter.messages";
const PROVENANCE: &str = "caller.openrouter.messages.experimental";
const INTEGRATION_REVISION: &str = "openrouter-messages-integration-v1";
const AUTH_PROFILE: &str = "openrouter.platform-api-key";
const AUTH_REVISION: &str = "openrouter-platform-api-key-v1";
const AUTHORITY_CLASS: &str = "platform-api-key";
const CREDENTIAL_SELECTION: &str = "openrouter-product-key-generation";
pub(crate) const LOWERING_REVISION: &str = "openrouter-messages-lowering-v1";
const PROJECTION_REVISION: &str = "openrouter-messages-canonical-v2";
const ACCOUNTING_REVISION: &str = "openrouter-messages-wire-v1";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);
const DEFAULT_EVENT_CAPACITY: usize = 64;
const DEFAULT_ERROR_BODY_LIMIT: usize = 64 * 1024;

/// Gateway-managed routing makes no endpoint or cross-wire claim.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpenRouterMessagesRoutingPolicy;

impl OpenRouterMessagesRoutingPolicy {
    /// Reports that another protocol is never selected after admission.
    pub const fn allows_cross_protocol_fallback(self) -> bool {
        false
    }

    /// Reports that gateway-managed routing proves no serving endpoint.
    pub const fn endpoint_claim(self) -> Option<&'static str> {
        None
    }
}

/// Immutable dynamic Custom/experimental OpenRouter Messages target.
#[derive(Clone)]
pub struct OpenRouterMessagesTarget {
    route: OpenRouterChatRoute,
    evidence: OpenRouterChatModelEvidence,
    descriptor: ResolvedModelDescriptor,
    target_identity: Digest,
}

impl OpenRouterMessagesTarget {
    /// Builds one Messages-family target from authenticated literal discovery and caller Custom context.
    pub fn gateway_managed(
        route: OpenRouterChatRoute,
        evidence: OpenRouterChatModelEvidence,
        context_contract: ContextContract,
    ) -> Result<Self, OpenRouterMessagesBuildError> {
        if route.identity().retention_policy().is_none() {
            return Err(OpenRouterMessagesBuildError::MissingRetentionPolicy);
        }
        validate_evidence(&route, &evidence, &context_contract, SystemClock.now())?;
        let descriptor = ResolvedModelDescriptor::new_custom(
            ProtocolId::new(OPENROUTER_MESSAGES_PROTOCOL_ID)?,
            route.identity().route_id().clone(),
            evidence.model().clone(),
            None,
            Some(ModelFamilyId::new(MODEL_FAMILY)?),
            ModelAdapterProfileId::new(PROFILE)?,
            messages_capabilities(&evidence)?,
        )
        .with_context_contract(context_contract);
        let target_identity = Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[
                b"plakous-openrouter/messages-target-v1",
                OPENROUTER_MESSAGES_PATH.as_bytes(),
                OPENROUTER_MESSAGES_PROTOCOL_ID.as_bytes(),
                route.identity().fingerprint().as_bytes(),
                evidence.model().as_str().as_bytes(),
                &evidence
                    .admission_identity(crate::discovery::AdmissionWire::Messages)
                    .map_err(|_| OpenRouterMessagesBuildError::Identity)?
                    .identity_bytes(),
                b"routing=gateway-managed;endpoint-claim=none;cross-protocol-fallback=false",
            ],
        )?;
        Ok(Self {
            route,
            evidence,
            descriptor,
            target_identity,
        })
    }

    /// Returns the exact Custom descriptor.
    pub const fn descriptor(&self) -> &ResolvedModelDescriptor {
        &self.descriptor
    }

    /// Returns the authenticated literal model unchanged.
    pub const fn model(&self) -> &plakous_model::ModelId {
        self.evidence.model()
    }

    /// Returns the credential-free target identity.
    pub const fn target_identity(&self) -> Digest {
        self.target_identity
    }

    /// Returns gateway-managed routing with no endpoint assertion.
    pub const fn routing(&self) -> OpenRouterMessagesRoutingPolicy {
        OpenRouterMessagesRoutingPolicy
    }

    fn runtime_authority(
        &self,
        checked_at: Timestamp,
    ) -> Result<RuntimeModelAuthority, OpenRouterMessagesBuildError> {
        validate_evidence(
            &self.route,
            &self.evidence,
            self.descriptor
                .context_contract()
                .ok_or(OpenRouterMessagesBuildError::Configuration)?,
            checked_at,
        )?;
        let generation = self.route.current_credential_generation_grant()?;
        RuntimeModelAuthority::new(
            &self.descriptor,
            self.route.identity().account_binding(),
            self.route.identity().credential_authority_binding(),
            generation_number(generation),
            checked_at,
            Some(self.evidence.expires_at()),
        )
        .map_err(OpenRouterMessagesBuildError::RuntimeAuthority)
    }
}

impl fmt::Debug for OpenRouterMessagesTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenRouterMessagesTarget")
            .field("route", self.route.identity())
            .field("model", &self.model())
            .field("routing", &self.routing())
            .field("descriptor", &self.descriptor)
            .field("target_identity", &self.target_identity)
            .finish_non_exhaustive()
    }
}

/// Explicit HTTP bounds for one OpenRouter Messages exchange.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpenRouterMessagesHttpConfig {
    operation_timeout: Duration,
    event_channel_capacity: usize,
    maximum_error_body_bytes: usize,
}

impl Default for OpenRouterMessagesHttpConfig {
    fn default() -> Self {
        Self {
            operation_timeout: DEFAULT_TIMEOUT,
            event_channel_capacity: DEFAULT_EVENT_CAPACITY,
            maximum_error_body_bytes: DEFAULT_ERROR_BODY_LIMIT,
        }
    }
}

/// Production dynamic OpenRouter Messages model using the shared family host.
pub struct OpenRouterMessagesLanguageModel {
    target: OpenRouterMessagesTarget,
    client: Client,
    config: OpenRouterMessagesHttpConfig,
    configuration: CompiledProviderConfiguration,
    clock: Arc<dyn Clock>,
}

impl OpenRouterMessagesLanguageModel {
    /// Builds a complete Custom model from explicit behavior.
    pub fn configured_gateway_managed_behavior(
        route: OpenRouterChatRoute,
        evidence: OpenRouterChatModelEvidence,
        context_contract: ContextContract,
        behavior: ProviderBehaviorConfiguration,
        client: Client,
    ) -> Result<ConfiguredLanguageModel, OpenRouterMessagesBuildError> {
        let target = OpenRouterMessagesTarget::gateway_managed(route, evidence, context_contract)?;
        let descriptor = target.descriptor();
        let selected = SelectedProviderConfiguration::new(
            ProviderRouteSelection::new(
                descriptor.protocol().clone(),
                descriptor.route().clone(),
                descriptor.profile().clone(),
                ProviderProfileRevisionId::new(INTEGRATION_REVISION)?,
                ModelAdapterProfileId::new(AUTH_PROFILE)?,
                ProviderProfileRevisionId::new(AUTH_REVISION)?,
                ProviderAuthorityClassId::new(AUTHORITY_CLASS)?,
                CredentialSelectionId::new(CREDENTIAL_SELECTION)?,
                descriptor.requested_model().clone(),
            ),
            behavior,
        )?;
        Self::configured_gateway_managed_target(target, selected, client)
    }

    /// Builds a complete Custom model from an exact selected configuration.
    pub fn configured_gateway_managed(
        route: OpenRouterChatRoute,
        evidence: OpenRouterChatModelEvidence,
        context_contract: ContextContract,
        selected: SelectedProviderConfiguration,
        client: Client,
    ) -> Result<ConfiguredLanguageModel, OpenRouterMessagesBuildError> {
        let target = OpenRouterMessagesTarget::gateway_managed(route, evidence, context_contract)?;
        Self::configured_gateway_managed_target(target, selected, client)
    }

    fn configured_gateway_managed_target(
        target: OpenRouterMessagesTarget,
        selected: SelectedProviderConfiguration,
        client: Client,
    ) -> Result<ConfiguredLanguageModel, OpenRouterMessagesBuildError> {
        let model = Arc::new(Self::new(target, selected, client)?);
        let authority = model.target.runtime_authority(model.clock.now())?;
        ConfiguredLanguageModel::custom(model.clone(), model.configuration.clone(), authority)
            .map_err(OpenRouterMessagesBuildError::ProviderConfiguration)
    }

    /// Binds one exact target, configuration, and HTTP client.
    pub fn new(
        target: OpenRouterMessagesTarget,
        selected: SelectedProviderConfiguration,
        client: Client,
    ) -> Result<Self, OpenRouterMessagesBuildError> {
        Self::with_config(
            target,
            selected,
            client,
            OpenRouterMessagesHttpConfig::default(),
        )
    }

    /// Binds explicit transport bounds.
    pub fn with_config(
        target: OpenRouterMessagesTarget,
        selected: SelectedProviderConfiguration,
        client: Client,
        config: OpenRouterMessagesHttpConfig,
    ) -> Result<Self, OpenRouterMessagesBuildError> {
        if config.operation_timeout.is_zero()
            || config.event_channel_capacity == 0
            || config.maximum_error_body_bytes == 0
        {
            return Err(OpenRouterMessagesBuildError::InvalidPolicy);
        }
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let runtime = target.runtime_authority(clock.now())?;
        let configuration = compile_configuration(&target, selected, &runtime)?;
        Ok(Self {
            target,
            client,
            config,
            configuration,
            clock,
        })
    }

    /// Returns exact selected/effective configuration authority.
    pub const fn configuration(&self) -> &CompiledProviderConfiguration {
        &self.configuration
    }
}

impl fmt::Debug for OpenRouterMessagesLanguageModel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenRouterMessagesLanguageModel")
            .field("target", &self.target)
            .field("config", &self.config)
            .field("configuration", &self.configuration)
            .finish_non_exhaustive()
    }
}

impl LanguageModel for OpenRouterMessagesLanguageModel {
    fn reachability_probe(&self) -> Option<plakous_model::ReachabilityProbe> {
        let origin = Origin::parse(self.target.route.identity().origin().as_str()).ok()?;
        Some(plakous_responses_dialect::bind_reachability_probe(
            self.client.clone(),
            origin,
            self.target.descriptor().route().clone(),
        ))
    }

    fn revalidate_runtime_authority(
        self: Arc<Self>,
        checked_at: Timestamp,
    ) -> RuntimeModelAuthorityOperation {
        Box::pin(async move {
            self.target
                .runtime_authority(checked_at)
                .map_err(|error| match error {
                    OpenRouterMessagesBuildError::StaleEvidence => {
                        RuntimeModelAuthorityError::Stale
                    }
                    OpenRouterMessagesBuildError::Authentication(_) => {
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
        let descriptor = self.target.descriptor();
        descriptor.validate_plan_binding(&plan)?;
        if plan.request().selected_provider_configuration_id()
            != self.configuration.selected_identity()
            || plan.request().effective_provider_configuration_id()
                != self.configuration.effective_identity()
            || !crate::execution::request_matches_configuration(
                plan.request(),
                self.configuration.selected().behavior(),
            )
        {
            return Err(rejection(
                ModelPreparationErrorKind::InvalidConfiguration,
                "OpenRouter Messages request configuration is foreign",
            ));
        }
        descriptor.validate_plan_admission(plan.request(), &plan)?;
        context
            .authority()
            .validate(descriptor, context.prepared_at())?;
        let identity = self.target.route.identity();
        if context.authority().account_binding() != identity.account_binding()
            || context.authority().credential_authority_binding()
                != identity.credential_authority_binding()
        {
            return Err(RuntimeModelAuthorityError::ForeignAuthority.into());
        }
        let output_bound = self
            .configuration
            .selected()
            .behavior()
            .output_bound
            .bound()
            .ok_or_else(|| {
                rejection(
                    ModelPreparationErrorKind::InvalidConfiguration,
                    "OpenRouter Messages requires an enforced output bound",
                )
            })?;
        let retention = identity.retention_policy();
        let router = MessagesRouterPolicy::gateway_managed(
            retention
                .and_then(OpenRouterRetentionPolicy::data_collection)
                .map(str::to_owned),
            retention.and_then(OpenRouterRetentionPolicy::zdr),
        )
        .map_err(|_| {
            rejection(
                ModelPreparationErrorKind::InvalidConfiguration,
                "OpenRouter Messages routing policy is invalid",
            )
        })?;
        let policy = MessagesRequestPolicy::new(
            descriptor.protocol().clone(),
            descriptor.requested_model().clone(),
            None,
            plan.binding_digest(),
            descriptor.semantic_binding_digest().map_err(|_| {
                rejection(
                    ModelPreparationErrorKind::InvalidConfiguration,
                    "OpenRouter Messages replay descriptor binding failed",
                )
            })?,
            identity.account_binding(),
            self.configuration.selected_identity(),
            self.configuration.effective_identity(),
            output_bound.get(),
            None,
            "openrouter-messages-v1",
            "",
        )
        .with_router_policy(router)
        .with_plugins(plakous_messages::OPENROUTER_DISABLE_CONTEXT_COMPRESSION_PLUGINS)
        .with_prompt_cache(true)
        .with_top_level_prompt_cache(false);
        let policy = if let Some(affinity) = context
            .cache_affinity()
            .map(plakous_model::ModelCacheAffinity::identity)
        {
            policy.with_session_id(affinity.to_string())
        } else {
            policy
        };
        let lowered = lower_request_with_active_artifact(
            &plan,
            &policy,
            ToolOwnership::default(),
            context.active_family_artifact(),
        )
        .map_err(|error| {
            let kind = match error.kind() {
                MessagesRequestErrorKind::UnsupportedSemantic => {
                    ModelPreparationErrorKind::UnsupportedCapability
                }
                MessagesRequestErrorKind::InvalidRequest => {
                    ModelPreparationErrorKind::InvalidRequest
                }
                MessagesRequestErrorKind::ProtectedHistory => {
                    ModelPreparationErrorKind::ProtectedHistory
                }
            };
            rejection(kind, "OpenRouter Messages request lowering failed")
        })?;
        let replay_diagnostics = lowered
            .replay_diagnostics()
            .iter()
            .map(ModelDiagnostic::new)
            .collect::<VecDeque<_>>();
        let wire_digest = Digest::sha256(
            DigestDomain::Blob,
            1,
            &[b"plakous-openrouter/messages-wire-v1", lowered.body()],
        )
        .map_err(|_| {
            rejection(
                ModelPreparationErrorKind::InvalidConfiguration,
                "OpenRouter Messages wire identity failed",
            )
        })?;
        let wire_size = u64::try_from(lowered.body().len()).map_err(|_| {
            rejection(
                ModelPreparationErrorKind::ResourceLimit,
                "OpenRouter Messages request is too large",
            )
        })?;
        let observation = descriptor
            .context_contract()
            .and_then(|contract| contract.measurements().first())
            .map(|certificate| {
                CountObservation::new(
                    certificate.certificate_id(),
                    certificate.revision(),
                    wire_size,
                )
            })
            .transpose()
            .map_err(|_| {
                rejection(
                    ModelPreparationErrorKind::InvalidConfiguration,
                    "OpenRouter Messages context observation is invalid",
                )
            })?;
        let token_projection = PreparedTokenProjection::new(wire_digest, wire_size, observation)?
            .with_input_parts(plakous_model::ContextInputParts::measure_wire(
                lowered.body(),
            ))
            .with_wire_input(
                lowered.body(),
                Some(plakous_model::CONSERVATIVE_IMAGE_MAX_TOKENS),
                &context,
            )?;
        let output_limit = PreparedOutputLimit::enforced_wire_maximum(
            output_bound,
            "max_tokens",
            wire_digest,
            LOWERING_REVISION,
        )?;
        let projection = PreparedModelProjection::new(
            descriptor,
            &plan,
            &context,
            ProtocolId::new(MESSAGES_FAMILY_ID).map_err(|_| {
                rejection(
                    ModelPreparationErrorKind::InvalidConfiguration,
                    "Messages family identity is invalid",
                )
            })?,
            LOWERING_REVISION,
            PROJECTION_REVISION,
            ACCOUNTING_REVISION,
            wire_digest,
            wire_size,
            token_projection,
            context
                .cache_affinity()
                .map_or(PreparedCacheUse::None, |affinity| {
                    PreparedCacheUse::RuntimeAffinity {
                        affinity: affinity.identity(),
                    }
                }),
            Some(output_limit),
        )?;
        let record = PreparedCallRecord::for_prepared(descriptor, &plan, projection, &context)?;
        let stream = PreparedMessagesStreamBinding::new(
            policy,
            plan.digest(),
            lowered.tool_ownership().clone(),
            MessagesStreamDialectHooks::client_tools_only()
                .with_extensions(MessagesStreamExtensions::OpenRouterV1),
        );
        let stream = lowered.bind_active_artifact_to_stream(stream);
        let binding = MessagesOperationBinding::new(
            self.target.target_identity(),
            plan.digest(),
            lowered.operation_digest(),
            lowered.replay_safety(),
        );
        let operation = PreparedMessagesHttpOperation::new(
            binding.clone(),
            Arc::<[u8]>::from(lowered.body()),
            stream,
        );
        let hooks = OpenRouterMessagesHooks {
            target: self.target.clone(),
            clock: Arc::clone(&self.clock),
            generation: self
                .target
                .route
                .current_credential_generation_grant()
                .map_err(|_| {
                    rejection(
                        ModelPreparationErrorKind::InvalidConfiguration,
                        "OpenRouter Messages credential generation is unavailable",
                    )
                })?,
        };
        let client = self.client.clone();
        let origin = Origin::parse(identity.origin().as_str()).map_err(|_| {
            rejection(
                ModelPreparationErrorKind::InvalidConfiguration,
                "OpenRouter Messages origin is invalid",
            )
        })?;
        let clock = Arc::clone(&self.clock);
        let config = self.config;
        let descriptor = descriptor.clone();
        let turn_rooted = matches!(
            plan.request().metadata().root(),
            plakous_model::ModelOperationRoot::Turn(_)
        );
        Ok(PreparedModelCall::new(record, move |call_context| {
            Box::pin(async move {
                let capacity =
                    NonZeroUsize::new(config.event_channel_capacity).unwrap_or(NonZeroUsize::MIN);
                let (attempt, receiver) =
                    MessagesAttempt::with_event_channel(binding, call_context, capacity);
                let mut service = MessagesHttpAttemptService::new(
                    client,
                    origin,
                    operation,
                    hooks,
                    Arc::clone(&clock),
                    Arc::new(SystemIdGenerator),
                    config.operation_timeout,
                    config.maximum_error_body_bytes,
                    turn_rooted,
                )
                .with_retry(revalidate_hooks);
                let operation =
                    Box::pin(async move { call_messages_operation(&mut service, attempt).await });
                Ok(Box::pin(JoinedMessagesStream {
                    descriptor,
                    pre_dispatch_diagnostics: replay_diagnostics,
                    operation,
                    receiver,
                    operation_done: false,
                    finished: false,
                    terminal_error: None,
                    observation_error: None,
                }) as plakous_model::ModelStream)
            })
        }))
    }
}

#[derive(Clone)]
struct OpenRouterMessagesHooks {
    target: OpenRouterMessagesTarget,
    clock: Arc<dyn Clock>,
    generation: OpenRouterApiKeyGenerationGrant,
}

#[derive(Clone)]
struct ErrorMetadata {
    status: u16,
    retry_after: Option<Duration>,
}

impl MessagesHttpHooks for OpenRouterMessagesHooks {
    type ErrorMetadata = ErrorMetadata;

    fn build_request(
        &self,
        client: &Client,
        dial_origin: Origin,
        attempt: &MessagesAttempt,
        operation: &PreparedMessagesHttpOperation,
    ) -> Result<Request, MessagesAttemptProtocolError> {
        revalidate_messages_authority(self)?;
        let physical = PhysicalRequestBinding::new(
            attempt.binding().clone(),
            attempt.terminal_facts().physical_attempts(),
        );
        let key = self
            .target
            .route
            .messages_credential(&physical, self.generation)
            .map_err(|_| {
                MessagesAttemptProtocolError::new("OpenRouter Messages authentication failed")
            })?;
        let authorization = sensitive_header_value(key.bearer_bytes()).map_err(|_| {
            MessagesAttemptProtocolError::new("OpenRouter Messages authorization failed")
        })?;
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("accept"),
            HeaderValue::from_static("text/event-stream"),
        );
        headers.insert(
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("application/json"),
        );
        headers.insert(HeaderName::from_static("authorization"), authorization);
        if let Some(attribution) = self.target.route.identity().attribution() {
            headers.insert(
                HeaderName::from_static("http-referer"),
                HeaderValue::from_static(attribution.referer()),
            );
            headers.insert(
                HeaderName::from_static("x-openrouter-title"),
                HeaderValue::from_static(attribution.title()),
            );
            headers.insert(
                HeaderName::from_static("x-openrouter-categories"),
                HeaderValue::from_static(crate::OpenRouterAttribution::CATEGORY),
            );
        }
        Request::new(
            dial_origin,
            Method::POST,
            OPENROUTER_MESSAGES_PATH,
            headers,
            SensitiveBytes::new(operation.body().to_vec()).into_bytes(),
            client.limits(),
        )
        .map_err(|_| {
            MessagesAttemptProtocolError::new("OpenRouter Messages request binding failed")
        })
    }

    fn classify_response_head(
        &self,
        response: &Response,
    ) -> Result<MessagesHttpResponseHead<Self::ErrorMetadata>, MessagesAttemptProtocolError> {
        let status = response.status();
        if status != StatusCode::OK {
            return Ok(MessagesHttpResponseHead::Error(MessagesHttpErrorHead {
                metadata: ErrorMetadata {
                    status: status.as_u16(),
                    retry_after: response
                        .headers()
                        .get(&HeaderName::from_static("retry-after"))
                        .and_then(|value| value.to_str().ok())
                        .and_then(plakous_attempt::retry_after_seconds),
                },
                outcome: crate::chat::post_send_status_outcome(status),
                provider_accepted: false,
            }));
        }
        let content_type = response
            .headers()
            .get(&HeaderName::from_static("content-type"))
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if !content_type
            .split(';')
            .next()
            .is_some_and(|media| media.trim().eq_ignore_ascii_case("text/event-stream"))
        {
            return Err(MessagesAttemptProtocolError::new(
                "OpenRouter Messages success was not an SSE response",
            ));
        }
        let request_id = response
            .headers()
            .get(&HeaderName::from_static("x-request-id"))
            .and_then(|value| value.to_str().ok())
            .and_then(|value| plakous_model::ModelProviderRequestId::new(value).ok());
        Ok(request_id.map_or(
            MessagesHttpResponseHead::StreamWithoutRequestId,
            MessagesHttpResponseHead::Stream,
        ))
    }

    fn classify_error_response(
        &self,
        metadata: Self::ErrorMetadata,
        body: &[u8],
        safety: MessagesHttpSafety,
    ) -> MessagesAttemptProtocolError {
        let disposition = if safety.provider_accepted || safety.visible_output {
            "post-acceptance"
        } else {
            "rejected"
        };
        let error = MessagesAttemptProtocolError::new(format!(
            "OpenRouter Messages HTTP {} ({disposition})",
            metadata.status
        ));
        let error = match plakous_model::ModelHttpStatus::new(metadata.status) {
            Ok(status) => error.with_http_status(status),
            Err(_) => error,
        };
        let error = match metadata.retry_after.and_then(retry_delay) {
            Some(advice) => error.with_retry_after(advice),
            None => error,
        };
        let error = if crate::chat::body_reports_context_overflow(body) {
            error.with_context_overflow()
        } else {
            error
        };
        // Carry the classification, not just the number in a sentence.
        match plakous_attempt::ProviderRefusal::from_http_status(metadata.status) {
            Some(refusal) => error.with_refusal(refusal),
            None => error,
        }
    }

    fn classify_transport_failure(
        &self,
        failure: HttpFailure,
        _request_id: Option<&str>,
    ) -> MessagesAttemptProtocolError {
        let cause = plakous_exchange::network_failure(failure).map_or_else(
            || "network exchange failed".to_owned(),
            |cause| cause.to_string(),
        );
        let error = MessagesAttemptProtocolError::new(format!(
            "OpenRouter Messages transport failed: {cause}"
        ))
        .with_detail(failure.to_string());
        // A deadline is the client's own decision to stop waiting, so this
        // attempt is definitely over. The Chat wire says the same thing the
        // same way; see `transport_refusal` there.
        match failure.kind() {
            plakous_http::ErrorKind::DeadlineExceeded => {
                error.with_refusal(plakous_messages::MessagesProviderRefusal::Deadline)
            }
            _ => error,
        }
    }

    fn cancellation_error(&self) -> MessagesAttemptProtocolError {
        MessagesAttemptProtocolError::new("OpenRouter Messages operation cancelled")
    }

    fn classify_stream_error(&self, error: MessagesStreamError) -> MessagesAttemptProtocolError {
        MessagesAttemptProtocolError::new(format!(
            "OpenRouter Messages stream validation failed: {error}"
        ))
    }
}

/// Refuse a resend if the sealed credential generation changed.
fn revalidate_hooks(
    hooks: &OpenRouterMessagesHooks,
) -> Pin<
    Box<
        dyn Future<Output = Result<OpenRouterMessagesHooks, MessagesAttemptProtocolError>>
            + Send
            + '_,
    >,
> {
    Box::pin(async move {
        revalidate_messages_authority(hooks)?;
        Ok(hooks.clone())
    })
}

fn revalidate_messages_authority(
    hooks: &OpenRouterMessagesHooks,
) -> Result<(), MessagesAttemptProtocolError> {
    let current = hooks
        .target
        .runtime_authority(hooks.clock.now())
        .map_err(|_| {
            MessagesAttemptProtocolError::new("OpenRouter Messages dispatch authority unavailable")
        })?;
    if current.credential_generation() != generation_number(hooks.generation) {
        return Err(MessagesAttemptProtocolError::new(
            "OpenRouter Messages dispatch credential changed",
        ));
    }
    Ok(())
}

type MessagesOperation = Pin<
    Box<
        dyn Future<Output = Result<plakous_messages::MessagesAttemptResponse, MessagesAttemptError>>
            + Send,
    >,
>;

struct JoinedMessagesStream {
    descriptor: ResolvedModelDescriptor,
    pre_dispatch_diagnostics: VecDeque<ModelDiagnostic>,
    operation: MessagesOperation,
    receiver: plakous_messages::MessagesAttemptEventReceiver,
    operation_done: bool,
    finished: bool,
    terminal_error: Option<ModelError>,
    observation_error: Option<MessagesAttemptProtocolError>,
}

impl Stream for JoinedMessagesStream {
    type Item = Result<ModelEvent, ModelError>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.finished {
            return Poll::Ready(None);
        }
        if let Some(diagnostic) = self.pre_dispatch_diagnostics.pop_front() {
            return Poll::Ready(Some(Ok(ModelEvent::Diagnostic(diagnostic))));
        }
        match Pin::new(&mut self.receiver).poll_next(context) {
            Poll::Ready(Some(Ok(event))) => return Poll::Ready(Some(Ok(event))),
            Poll::Ready(Some(Err(error))) => self.observation_error = Some(error),
            Poll::Ready(None) | Poll::Pending => {}
        }
        if !self.operation_done {
            match self.operation.as_mut().poll(context) {
                Poll::Pending => {}
                Poll::Ready(result) => {
                    self.operation_done = true;
                    self.terminal_error = result
                        .err()
                        .map(|error| messages_model_error(&self.descriptor, &error));
                }
            }
        }
        // Polling the operation can enqueue a successful prefix and its error
        // together. Drain that prefix before publishing the joined terminal.
        loop {
            match Pin::new(&mut self.receiver).poll_next(context) {
                Poll::Ready(Some(Ok(event))) => return Poll::Ready(Some(Ok(event))),
                Poll::Ready(Some(Err(error))) => self.observation_error = Some(error),
                Poll::Ready(None) | Poll::Pending => break,
            }
        }
        if !self.operation_done {
            return Poll::Pending;
        }
        self.finished = true;
        let error = self.terminal_error.take().or_else(|| {
            self.observation_error.take().map(|error| {
                ModelError::new(
                    crate::chat::inferred_failure_kind(None, error.exchange_break()),
                    format!("OpenRouter Messages protocol failed: {error}"),
                )
            })
        });
        Poll::Ready(error.map(Err))
    }
}

fn messages_model_error(
    descriptor: &ResolvedModelDescriptor,
    error: &MessagesAttemptError,
) -> ModelError {
    // A refusal the provider named, on an exchange that started no hosted
    // effect, is a definite outcome whatever acceptance says. This route commits
    // HTTP 200 at acceptance, so classifying on acceptance alone made every
    // post-acceptance refusal an unknown outcome and fenced the session.
    // Streamed text that was abandoned is discarded with the turn and leaves
    // nothing to reconcile; a hosted effect is the one thing that would.
    let facts = error.facts();
    let hosted = facts.is_some_and(plakous_messages::TerminalFacts::hosted_effects_possible);
    let named =
        (!hosted)
            .then(|| error.provider_refusal())
            .flatten()
            .map(|refusal| match refusal {
                plakous_attempt::ProviderRefusal::RateLimit => ModelErrorKind::RateLimited,
                plakous_attempt::ProviderRefusal::Overloaded => ModelErrorKind::Unavailable,
                plakous_attempt::ProviderRefusal::Deadline => ModelErrorKind::Timeout,
            });
    let kind = match error {
        MessagesAttemptError::Cancelled { .. } => ModelErrorKind::Cancelled,
        MessagesAttemptError::Unavailable => ModelErrorKind::Unavailable,
        _ => crate::chat::usage_limit_kind(error.usage_limit(), facts)
            .or_else(|| crate::chat::context_overflow_kind(error.context_overflow(), facts))
            .or(named)
            .unwrap_or_else(|| crate::chat::inferred_failure_kind(facts, error.exchange_break())),
    };
    let model_error = crate::chat::operation_error(
        descriptor,
        kind,
        facts,
        format!("OpenRouter Messages operation failed: {error}"),
    )
    .with_attempt_summary(facts.map_or_else(String::new, |facts| facts.summary().to_string()));
    if let MessagesAttemptError::Protocol { source, .. } = error {
        if let Some(detail) = source.detail() {
            return model_error.with_source(detail);
        }
    }
    model_error
}

fn compile_configuration(
    target: &OpenRouterMessagesTarget,
    selected: SelectedProviderConfiguration,
    runtime: &RuntimeModelAuthority,
) -> Result<CompiledProviderConfiguration, OpenRouterMessagesBuildError> {
    let descriptor = target.descriptor();
    let implementation = ModelImplementationIdentity::custom(
        NamespaceId::new(IMPLEMENTATION)?,
        1,
        NamespaceId::new(PROVENANCE)?,
    )?;
    let retention = target.route.identity().retention_policy();
    let semantic = ProviderSemanticEvidence::new(
        1,
        serde_json::to_vec(&(
            "plakous-openrouter/messages-semantic-v1",
            OPENROUTER_MESSAGES_PATH,
            OPENROUTER_MESSAGES_PROTOCOL_ID,
            OPENROUTER_MESSAGES_DIALECT_REVISION,
            target.model(),
            target
                .evidence
                .admission_identity(crate::discovery::AdmissionWire::Messages)
                .map_err(|_| OpenRouterMessagesBuildError::Identity)?,
            retention.map(OpenRouterRetentionPolicy::data_collection),
            retention.map(OpenRouterRetentionPolicy::requires_zdr),
            "routing=gateway-managed;endpoint-claim=none;cross-protocol-fallback=false",
        ))
        .map_err(|_| OpenRouterMessagesBuildError::Identity)?,
    )?;
    let authority = ResolvedProviderAuthorityBinding::new(descriptor, runtime, semantic)?;
    let family = descriptor
        .model_family()
        .cloned()
        .ok_or(OpenRouterMessagesBuildError::Configuration)?;
    let policy = FamilyPolicyEnvelope::new(
        family,
        1,
        1,
        OPENROUTER_MESSAGES_DIALECT_REVISION,
        serde_json::to_vec(&(
            "messages-router=gateway-managed",
            retention.map(OpenRouterRetentionPolicy::data_collection),
            retention.map(OpenRouterRetentionPolicy::requires_zdr),
            "stream-extensions=openrouter-v1",
            "client-tools-only",
            "aggregate-artifact=none",
        ))
        .map_err(|_| OpenRouterMessagesBuildError::Identity)?,
    )?;
    let effective = EffectiveProviderConfiguration::compile(
        &selected,
        descriptor.clone(),
        implementation,
        authority,
        selected.behavior().clone(),
        policy,
    )?;
    CompiledProviderConfiguration::new(selected, effective)
        .map_err(OpenRouterMessagesBuildError::ProviderConfiguration)
}

fn validate_evidence(
    route: &OpenRouterChatRoute,
    evidence: &OpenRouterChatModelEvidence,
    context: &ContextContract,
    now: Timestamp,
) -> Result<(), OpenRouterMessagesBuildError> {
    if !matches!(
        context.capacity().evidence_class(),
        ContextEvidenceClass::CustomEnvelope { .. }
    ) {
        return Err(OpenRouterMessagesBuildError::NonCustomContextAuthority);
    }
    if context.protocol_family().as_str() != MESSAGES_FAMILY_ID {
        return Err(OpenRouterMessagesBuildError::ForeignContextFamily);
    }
    if !crate::chat::context_within_discovery(context, evidence.context_length()) {
        return Err(OpenRouterMessagesBuildError::ContextExceedsDiscovery);
    }
    if !evidence.validates(route.identity(), now) {
        return Err(OpenRouterMessagesBuildError::StaleEvidence);
    }
    Ok(())
}

fn messages_capabilities(
    evidence: &OpenRouterChatModelEvidence,
) -> Result<CapabilitySet, plakous_model::IdentityError> {
    let provenance = FactProvenance::new(FactSource::BuiltIn, Timestamp::new(0, 0)?, None);
    let mut features: BTreeMap<_, _> = [Capability::Streaming, Capability::Tools]
        .into_iter()
        .map(|capability| {
            (
                capability,
                FeatureSupport::new(
                    Support::Native,
                    CapabilityConstraints::new(
                        None,
                        Some("Custom OpenRouter Messages dialect".to_owned()),
                    ),
                    provenance,
                ),
            )
        })
        .collect();
    if let Some(image) = crate::chat::image_input_fact(evidence) {
        features.insert(Capability::ImageInput, image);
    }
    Ok(CapabilitySet::new(features, BTreeMap::new()))
}

fn generation_number(grant: OpenRouterApiKeyGenerationGrant) -> u64 {
    let fingerprint = grant.fingerprint();
    let bytes = fingerprint.as_bytes();
    u64::from_be_bytes(bytes[..8].try_into().unwrap_or([0; 8])).max(1)
}

fn rejection(kind: ModelPreparationErrorKind, message: &'static str) -> ModelPreparationError {
    ModelPreparationError::rejected(kind, message)
}

/// Safe OpenRouter Messages construction failure.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum OpenRouterMessagesBuildError {
    /// Gateway-managed dispatch lacked an explicit route retention contract.
    #[error("OpenRouter Messages requires an explicit route retention policy")]
    MissingRetentionPolicy,
    /// Authenticated discovery evidence expired.
    #[error("OpenRouter Messages discovery evidence is stale")]
    StaleEvidence,
    /// Discovery evidence names another route or authority.
    #[error("OpenRouter Messages discovery evidence is foreign")]
    ForeignEvidence,
    /// Context authority was not a caller Custom envelope.
    #[error("OpenRouter Messages requires CustomEnvelope context authority")]
    NonCustomContextAuthority,
    /// Caller context exceeds authenticated discovery.
    #[error("OpenRouter Messages context exceeds authenticated discovery")]
    ContextExceedsDiscovery,
    /// Caller context was compiled for another protocol family.
    #[error("OpenRouter Messages requires a Messages-family context contract")]
    ForeignContextFamily,
    /// Transport bounds were zero.
    #[error("OpenRouter Messages HTTP bounds must be nonzero")]
    InvalidPolicy,
    /// Selected/effective configuration was inconsistent.
    #[error("OpenRouter Messages provider configuration is inconsistent")]
    Configuration,
    /// Identity serialization failed.
    #[error("OpenRouter Messages identity construction failed")]
    Identity,
    /// Neutral identity construction failed.
    #[error(transparent)]
    NeutralIdentity(#[from] plakous_model::IdentityError),
    /// Neutral digest construction failed.
    #[error(transparent)]
    Digest(#[from] plakous_model::DigestError),
    /// Provider configuration compilation failed.
    #[error(transparent)]
    ProviderConfiguration(#[from] plakous_model::ProviderConfigurationError),
    /// Runtime authority construction failed.
    #[error(transparent)]
    RuntimeAuthority(#[from] RuntimeModelAuthorityError),
    /// Credential authority was unavailable.
    #[error(transparent)]
    Authentication(#[from] crate::OpenRouterAuthError),
}

/// Saturates provider advice at the representable 24 h so long advice
/// reaches the retry kernel's 60 s clamp instead of being dropped.
fn retry_delay(advice: Duration) -> Option<plakous_model::ModelRetryDelay> {
    let millis = u64::try_from(advice.as_millis())
        .unwrap_or(u64::MAX)
        .min(plakous_model::ModelRetryDelay::MAX_MILLIS);
    plakous_model::ModelRetryDelay::from_millis(millis).ok()
}

#[cfg(test)]
mod tests {
    #[test]
    fn long_retry_after_advice_saturates_instead_of_dropping() {
        use std::time::Duration;

        use plakous_model::ModelRetryDelay;

        assert_eq!(
            retry_delay(Duration::from_secs(172_800)).map(ModelRetryDelay::as_millis),
            Some(ModelRetryDelay::MAX_MILLIS)
        );
        assert_eq!(
            retry_delay(Duration::from_secs(7)).map(ModelRetryDelay::as_millis),
            Some(7_000)
        );
    }

    /// The `OpenRouter` Messages row of the provider-error matrix.
    ///
    /// The same columns and the same rule as the Chat row, through this route's
    /// own classifier. Both routes commit HTTP 200 at acceptance, so both had
    /// the same defect and both needed the same fix.
    #[test]
    fn the_openrouter_messages_row_of_the_provider_failure_matrix_is_definite() {
        use plakous_attempt::ObservationSnapshot;
        use plakous_attempt::ProviderRefusal as Refusal;
        use plakous_messages::{MessagesAttemptProtocolError, TerminalFacts};

        let facts = |hosted: bool| {
            TerminalFacts::from_snapshot(ObservationSnapshot {
                physical_attempts: 1,
                provider_acceptance: plakous_attempt::ProviderAcceptance::Accepted,
                visible_output: true,
                replay_safety: if hosted {
                    plakous_attempt::ReplaySafety::ServerEffect
                } else {
                    plakous_attempt::ReplaySafety::CostOnly
                },
                hosted_effects_possible: hosted,
                logical_success: false,
            })
        };
        for (name, refusal, expected) in [
            (
                "pre-stream 5xx",
                Refusal::Overloaded,
                ModelErrorKind::Unavailable,
            ),
            (
                "pre-stream 429",
                Refusal::RateLimit,
                ModelErrorKind::RateLimited,
            ),
            (
                "in-band 5xx after partial stream",
                Refusal::Overloaded,
                ModelErrorKind::Unavailable,
            ),
            (
                "in-band 429 after partial stream",
                Refusal::RateLimit,
                ModelErrorKind::RateLimited,
            ),
            (
                "transport timeout",
                Refusal::Deadline,
                ModelErrorKind::Timeout,
            ),
            (
                "operation deadline",
                Refusal::Deadline,
                ModelErrorKind::Timeout,
            ),
        ] {
            let error = MessagesAttemptError::protocol(
                MessagesAttemptProtocolError::new("OpenRouter Messages HTTP failure")
                    .with_refusal(refusal),
                facts(false),
            );
            let projected = messages_model_error(&crate::chat::test_descriptor(), &error);
            assert_eq!(projected.kind(), expected, "{name}");
            assert_ne!(projected.kind(), ModelErrorKind::OutcomeUnknown, "{name}");

            // The one column that stays undetermined on every row.
            let hosted = MessagesAttemptError::protocol(
                MessagesAttemptProtocolError::new("OpenRouter Messages HTTP failure")
                    .with_refusal(refusal),
                facts(true),
            );
            assert_eq!(
                messages_model_error(&crate::chat::test_descriptor(), &hosted).kind(),
                ModelErrorKind::OutcomeUnknown,
                "{name} with a hosted effect"
            );
        }

        // A break the gateway did not name is definite by its own kind.
        let unnamed = MessagesAttemptError::protocol(
            MessagesAttemptProtocolError::new("OpenRouter Messages stream failed")
                .with_exchange_break(plakous_messages::ExchangeBreak::Transport),
            facts(false),
        );
        assert_eq!(
            messages_model_error(&crate::chat::test_descriptor(), &unnamed).kind(),
            ModelErrorKind::Transport
        );
    }
    use std::collections::BTreeSet;
    use std::num::NonZeroUsize;
    use std::str::FromStr;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use futures_util::{FutureExt, StreamExt};
    use plakous_attempt::{ObservationSnapshot, TerminalFacts};
    use plakous_model::{
        AttemptId, CancellationToken, CapacityCertificate, CapacityRule, ContextEvidenceSource,
        CountCertificateKind, EvidenceFreshness, IntegrationClass, MeasurementCertificate,
        ModelCallContext, RuntimeModelAuthority,
    };

    use super::*;
    use crate::{
        OpenRouterApiKey, OpenRouterChatEndpointCandidate, OpenRouterOrigin,
        OpenRouterToolChoiceModes, StaticOpenRouterApiKeySource,
    };

    fn route() -> Result<OpenRouterChatRoute, Box<dyn std::error::Error>> {
        let source = Arc::new(StaticOpenRouterApiKeySource::new(
            OpenRouterApiKey::new("messages-test-key")?,
            "messages-test-account",
            "messages-test-authority",
        )?);
        Ok(OpenRouterChatRoute::new_local_fixture_with_retention(
            source,
            OpenRouterOrigin::new("https://127.0.0.1:18443")?,
            OpenRouterRetentionPolicy::strict(),
        )?)
    }

    fn context(
        route: &OpenRouterChatRoute,
        limit: u64,
    ) -> Result<ContextContract, Box<dyn std::error::Error>> {
        let authority = RuntimeModelAuthority::authority_class_for(
            route.identity().account_binding(),
            route.identity().credential_authority_binding(),
        )?;
        let declaration = Digest::sha256(
            DigestDomain::ProviderSemanticEvidence,
            1,
            &[
                b"test-caller/openrouter-messages-context-v1",
                route.identity().fingerprint().as_bytes(),
                &limit.to_be_bytes(),
            ],
        )?;
        let freshness = EvidenceFreshness::new(Timestamp::new(1, 0)?, None)?;
        let revision = "test-openrouter-messages-context-v1";
        let capacity = CapacityCertificate::custom_envelope(
            "test-openrouter-messages-capacity",
            revision,
            CapacityRule::ProviderReservedInput {
                reported_context_window: Some(limit),
                maximum_projected_input: limit,
                auto_compact_input: None,
            },
            authority,
            freshness,
            declaration,
            revision,
        )?;
        let measurement = MeasurementCertificate::new(
            "test-openrouter-messages-wire-bound",
            revision,
            "complete-lowered-messages-body-utf8-byte-upper-bound",
            revision,
            ContextEvidenceSource::CallerDeclaration,
            CountCertificateKind::CustomEnvelopeUpperBound { declaration },
            Some(authority),
            freshness,
        )?;
        Ok(ContextContract::new(
            ProtocolId::new(MESSAGES_FAMILY_ID)?,
            ACCOUNTING_REVISION,
            PROJECTION_REVISION,
            "openrouter-messages-output-reservation-v1",
            capacity,
            vec![measurement],
        )?)
    }

    fn evidence(
        route: &OpenRouterChatRoute,
        observed_offset: u64,
        expiry_offset: u64,
    ) -> Result<OpenRouterChatModelEvidence, Box<dyn std::error::Error>> {
        let now = SystemClock.now();
        OpenRouterChatModelEvidence::from_authenticated_discovery(
            plakous_model::ModelId::new("vendor/dynamic-model")?,
            route.identity(),
            vec![OpenRouterChatEndpointCandidate {
                endpoint: "provider/dynamic-model".to_owned(),
                provider_name: "Provider".to_owned(),
                supported_parameters: BTreeSet::from([
                    "tools".to_owned(),
                    "tool_choice".to_owned(),
                ]),
                context_length: Some(8_192),
                max_prompt_tokens: Some(4_096),
                max_completion_tokens: Some(4_096),
                tool_choice: OpenRouterToolChoiceModes::default(),
                supports_implicit_caching: false,
            }],
            BTreeSet::from(["tools".to_owned(), "tool_choice".to_owned()]),
            None,
            8_192,
            false,
            Timestamp::new(
                now.seconds().saturating_sub(observed_offset),
                now.nanoseconds(),
            )?,
            Timestamp::new(now.seconds() + expiry_offset, now.nanoseconds())?,
        )
        .map_err(Into::into)
    }

    #[test]
    fn target_is_messages_family_custom_and_endpoint_neutral()
    -> Result<(), Box<dyn std::error::Error>> {
        fn assert_language_model<T: LanguageModel>() {}
        assert_language_model::<OpenRouterMessagesLanguageModel>();
        let route = route()?;
        let target = OpenRouterMessagesTarget::gateway_managed(
            route.clone(),
            evidence(&route, 2, 3_600)?,
            context(&route, 4_096)?,
        )?;
        assert_eq!(
            target.descriptor().integration_class(),
            IntegrationClass::Custom
        );
        assert_eq!(
            target.descriptor().protocol().as_str(),
            OPENROUTER_MESSAGES_PROTOCOL_ID
        );
        assert_eq!(
            target
                .descriptor()
                .context_contract()
                .ok_or("context")?
                .protocol_family()
                .as_str(),
            MESSAGES_FAMILY_ID
        );
        assert_eq!(
            target
                .descriptor()
                .context_contract()
                .ok_or("context")?
                .projection_revision(),
            "openrouter-messages-canonical-v2"
        );
        assert_eq!(target.model().as_str(), "vendor/dynamic-model");
        assert_eq!(target.routing().endpoint_claim(), None);
        assert!(!target.routing().allows_cross_protocol_fallback());
        assert_eq!(OPENROUTER_MESSAGES_PATH, "/api/v1/messages");
        Ok(())
    }

    #[test]
    fn target_rejects_route_without_retention_contract() -> Result<(), Box<dyn std::error::Error>> {
        let source = Arc::new(StaticOpenRouterApiKeySource::new(
            OpenRouterApiKey::new("messages-no-retention-key")?,
            "messages-no-retention-account",
            "messages-no-retention-authority",
        )?);
        let route = OpenRouterChatRoute::new_local_fixture(
            source,
            OpenRouterOrigin::new("https://127.0.0.1:18443")?,
        )?;
        let Err(error) = OpenRouterMessagesTarget::gateway_managed(
            route.clone(),
            evidence(&route, 2, 3_600)?,
            context(&route, 4_096)?,
        ) else {
            panic!("retention-less route must not admit Messages dispatch");
        };
        assert!(matches!(
            error,
            OpenRouterMessagesBuildError::MissingRetentionPolicy
        ));
        Ok(())
    }

    #[test]
    fn freshness_only_refresh_preserves_messages_target_identity()
    -> Result<(), Box<dyn std::error::Error>> {
        let route = route()?;
        let contract = context(&route, 4_096)?;
        let first = OpenRouterMessagesTarget::gateway_managed(
            route.clone(),
            evidence(&route, 20, 3_600)?,
            contract.clone(),
        )?;
        let second = OpenRouterMessagesTarget::gateway_managed(
            route.clone(),
            evidence(&route, 10, 7_200)?,
            contract,
        )?;
        assert_eq!(first.target_identity(), second.target_identity());
        assert_eq!(first.descriptor(), second.descriptor());
        Ok(())
    }

    #[tokio::test]
    async fn joined_stream_prefers_fact_bearing_post_acceptance_error()
    -> Result<(), Box<dyn std::error::Error>> {
        let binding = MessagesOperationBinding::new(
            Digest::sha256(DigestDomain::ModelBinding, 1, &[b"target"])?,
            Digest::sha256(DigestDomain::RequestPlan, 1, &[b"plan"])?,
            Digest::sha256(DigestDomain::ProviderOperation, 1, &[b"operation"])?,
            plakous_attempt::ReplaySafety::CostOnly,
        );
        let context = ModelCallContext::new(
            AttemptId::from_str("00000000-0000-4000-8000-000000000044")?,
            CancellationToken::new(),
            None,
        );
        let (mut attempt, receiver) = MessagesAttempt::with_event_channel(
            binding,
            context,
            NonZeroUsize::new(1).ok_or("capacity")?,
        );
        let observation = MessagesAttemptProtocolError::new("malformed accepted stream");
        attempt.events_mut().send_error(observation.clone()).await?;
        let facts = TerminalFacts::from_snapshot(ObservationSnapshot {
            physical_attempts: 1,
            provider_acceptance: plakous_attempt::ProviderAcceptance::Accepted,
            visible_output: true,
            replay_safety: plakous_attempt::ReplaySafety::CostOnly,
            hosted_effects_possible: false,
            logical_success: false,
        });
        let operation: MessagesOperation = Box::pin(futures_util::future::ready(Err(
            MessagesAttemptError::protocol(observation, facts),
        )));
        let mut stream = JoinedMessagesStream {
            descriptor: crate::chat::test_descriptor(),
            pre_dispatch_diagnostics: VecDeque::new(),
            operation,
            receiver,
            operation_done: false,
            finished: false,
            terminal_error: None,
            observation_error: None,
        };

        let Some(Err(error)) = stream.next().await else {
            return Err("missing terminal error".into());
        };
        assert_eq!(error.kind(), ModelErrorKind::Protocol);
        assert!(stream.next().await.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn joined_error_follows_prefix_queued_by_the_same_operation_poll()
    -> Result<(), Box<dyn std::error::Error>> {
        use plakous_model::ResponseStarted;
        use std::sync::atomic::AtomicUsize;

        let binding = MessagesOperationBinding::new(
            Digest::sha256(DigestDomain::ModelBinding, 1, &[b"target"])?,
            Digest::sha256(DigestDomain::RequestPlan, 1, &[b"plan"])?,
            Digest::sha256(DigestDomain::ProviderOperation, 1, &[b"operation"])?,
            plakous_attempt::ReplaySafety::CostOnly,
        );
        let prefix = ModelEvent::ResponseStarted(ResponseStarted::new(None, binding.plan()));
        let context = ModelCallContext::new(
            AttemptId::from_str("00000000-0000-4000-8000-000000000044")?,
            CancellationToken::new(),
            None,
        );
        let (mut attempt, receiver) = MessagesAttempt::with_event_channel(
            binding,
            context,
            NonZeroUsize::new(4).ok_or("capacity")?,
        );
        let completed = Arc::new(AtomicUsize::new(0));
        let operation_completed = Arc::clone(&completed);
        let operation = Box::pin(async move {
            attempt.events_mut().send(prefix).await?;
            let observation = MessagesAttemptProtocolError::new("accepted stream failed");
            attempt.events_mut().send_error(observation.clone()).await?;
            let facts = TerminalFacts::from_snapshot(ObservationSnapshot {
                physical_attempts: 1,
                provider_acceptance: plakous_attempt::ProviderAcceptance::Accepted,
                visible_output: false,
                replay_safety: plakous_attempt::ReplaySafety::CostOnly,
                hosted_effects_possible: false,
                logical_success: false,
            });
            operation_completed.fetch_add(1, Ordering::SeqCst);
            Err(MessagesAttemptError::protocol(observation, facts))
        });
        let mut stream = JoinedMessagesStream {
            descriptor: crate::chat::test_descriptor(),
            pre_dispatch_diagnostics: VecDeque::new(),
            operation,
            receiver,
            operation_done: false,
            finished: false,
            terminal_error: None,
            observation_error: None,
        };
        assert!(matches!(
            stream.next().await,
            Some(Ok(ModelEvent::ResponseStarted(_)))
        ));
        assert_eq!(completed.load(Ordering::SeqCst), 1);
        let Some(Err(error)) = stream.next().await else {
            return Err("missing terminal error".into());
        };
        assert_eq!(error.kind(), ModelErrorKind::Protocol);
        for _ in 0..3 {
            assert!(stream.next().await.is_none());
        }
        assert_eq!(completed.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[tokio::test]
    async fn replay_strip_diagnostic_precedes_physical_operation_poll()
    -> Result<(), Box<dyn std::error::Error>> {
        let binding = MessagesOperationBinding::new(
            Digest::sha256(DigestDomain::ModelBinding, 1, &[b"target-diagnostic"])?,
            Digest::sha256(DigestDomain::RequestPlan, 1, &[b"plan-diagnostic"])?,
            Digest::sha256(
                DigestDomain::ProviderOperation,
                1,
                &[b"operation-diagnostic"],
            )?,
            plakous_attempt::ReplaySafety::CostOnly,
        );
        let context = ModelCallContext::new(
            AttemptId::from_str("00000000-0000-4000-8000-000000000045")?,
            CancellationToken::new(),
            None,
        );
        let (_attempt, receiver) = MessagesAttempt::with_event_channel(
            binding,
            context,
            NonZeroUsize::new(1).ok_or("capacity")?,
        );
        let polled = Arc::new(AtomicBool::new(false));
        let operation_polled = Arc::clone(&polled);
        let operation: MessagesOperation = Box::pin(std::future::poll_fn(move |_| {
            operation_polled.store(true, Ordering::SeqCst);
            Poll::Pending
        }));
        let mut stream = JoinedMessagesStream {
            descriptor: crate::chat::test_descriptor(),
            pre_dispatch_diagnostics: VecDeque::from([ModelDiagnostic::new(
                "incompatible Messages thinking replay was stripped",
            )]),
            operation,
            receiver,
            operation_done: false,
            finished: false,
            terminal_error: None,
            observation_error: None,
        };

        let Some(Ok(ModelEvent::Diagnostic(diagnostic))) = stream.next().await else {
            return Err("missing replay-strip diagnostic".into());
        };
        assert_eq!(
            diagnostic.message().as_str(),
            "incompatible Messages thinking replay was stripped"
        );
        assert!(!polled.load(Ordering::SeqCst));
        assert!(stream.next().now_or_never().is_none());
        assert!(polled.load(Ordering::SeqCst));
        Ok(())
    }

    #[test]
    fn chat_family_context_is_rejected_for_messages() -> Result<(), Box<dyn std::error::Error>> {
        let route = route()?;
        let mut contract = context(&route, 4_096)?;
        contract = ContextContract::new(
            ProtocolId::new("chat-completions-family")?,
            ACCOUNTING_REVISION,
            PROJECTION_REVISION,
            "openrouter-messages-output-reservation-v1",
            contract.capacity().clone(),
            contract.measurements().to_vec(),
        )?;
        assert!(
            OpenRouterMessagesTarget::gateway_managed(
                route.clone(),
                evidence(&route, 1, 3_600)?,
                contract,
            )
            .is_err()
        );
        Ok(())
    }
}
