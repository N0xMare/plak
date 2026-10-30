//! Thin dynamic OpenRouter Messages dialect over the shared Messages kernel.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

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
    ConfiguredLanguageModel, ContextContract, ContextEvidenceClass, CredentialSelectionId, Digest,
    DigestDomain, EffectiveProviderConfiguration, FactProvenance, FactSource, FamilyPolicyEnvelope,
    FeatureSupport, LanguageModel, ModelAdapterProfileId, ModelDiagnostic, ModelError,
    ModelErrorKind, ModelFamilyId, ModelImplementationIdentity, ModelPreparationContext,
    ModelPreparationError, ModelPreparationErrorKind, NamespaceId, PreparedModelCall, ProtocolId,
    ProviderAuthorityClassId, ProviderBehaviorConfiguration, ProviderProfileRevisionId,
    ProviderRouteSelection, ProviderSemanticEvidence, ResolvedModelDescriptor,
    ResolvedProviderAuthorityBinding, RuntimeModelAuthority, RuntimeModelAuthorityError,
    RuntimeModelAuthorityOperation, SelectedProviderConfiguration, Support, SystemIdGenerator,
    Timestamp, ValidatedRequestPlan,
};
use thiserror::Error;

use crate::wire::{
    AttemptWire, JoinedStream, WireProjection, body_reports_context_overflow,
    context_overflow_kind, generation_number, inferred_failure_kind, insert_attribution,
    is_sse_content_type, named_refusal_kind, operation_error, post_send_status_outcome, rejection,
    usage_limit_kind,
};
use crate::{
    OpenRouterChatModelEvidence, OpenRouterChatRoute, OpenRouterKeyGeneration,
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
        validate_evidence(&route, &evidence, &context_contract, route.clock.now())?;
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
            generation_number(&generation),
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
        let clock = Arc::clone(&target.route.clock);
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

impl OpenRouterMessagesLanguageModel {
    fn request_policy(
        &self,
        plan: &ValidatedRequestPlan,
        context: &ModelPreparationContext,
        output_bound: std::num::NonZeroU32,
    ) -> Result<MessagesRequestPolicy, ModelPreparationError> {
        let descriptor = self.target.descriptor();
        let identity = self.target.route.identity();
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
        let policy = MessagesRequestPolicy::new(plakous_messages::MessagesRequestPolicyInput {
            protocol: descriptor.protocol().clone(),
            requested_model: descriptor.requested_model().clone(),
            expected_reported_model: None,
            descriptor_binding: plan.binding_digest(),
            semantic_descriptor_binding: descriptor.semantic_binding_digest().map_err(|_| {
                rejection(
                    ModelPreparationErrorKind::InvalidConfiguration,
                    "OpenRouter Messages replay descriptor binding failed",
                )
            })?,
            account_binding: identity.account_binding(),
            selected_provider_configuration: self.configuration.selected_identity(),
            effective_provider_configuration: self.configuration.effective_identity(),
            maximum_output_tokens: output_bound.get(),
            minimum_thinking_budget: None,
            api_version: "openrouter-messages-v1".into(),
            beta_header: String::new(),
        })
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
        Ok(policy)
    }
}

impl LanguageModel for OpenRouterMessagesLanguageModel {
    fn reachability_probe(&self) -> Option<plakous_model::ReachabilityProbe> {
        let origin = Origin::parse(self.target.route.identity().origin().as_str()).ok()?;
        Some(plakous_responses::bind_reachability_probe(
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
        OPENROUTER_MESSAGES_WIRE.admit_plan(descriptor, &self.configuration, &plan, &context)?;
        let identity = self.target.route.identity();
        let output_bound =
            OPENROUTER_MESSAGES_WIRE.admit_route(identity, &self.configuration, &context)?;
        let policy = self.request_policy(&plan, &context, output_bound)?;
        let lowered = lower_request_with_active_artifact(
            &plan,
            &policy,
            ToolOwnership::default(),
            context.active_family_artifact(),
        )
        .map_err(messages_lowering_error)?;
        let replay_diagnostics = lowered
            .replay_diagnostics()
            .iter()
            .map(ModelDiagnostic::new)
            .collect::<VecDeque<_>>();
        let record = OPENROUTER_MESSAGES_WIRE.prepare_record(
            descriptor,
            &plan,
            &context,
            lowered.body(),
            output_bound,
        )?;
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
                Ok(Box::pin(JoinedStream::<MessagesWire, _>::new(
                    descriptor,
                    replay_diagnostics,
                    operation,
                    receiver,
                )) as plakous_model::ModelStream)
            })
        }))
    }
}

#[derive(Clone)]
struct OpenRouterMessagesHooks {
    target: OpenRouterMessagesTarget,
    clock: Arc<dyn Clock>,
    generation: OpenRouterKeyGeneration,
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
            .messages_credential(&physical, &self.generation)
            .map_err(|_| {
                MessagesAttemptProtocolError::new("OpenRouter Messages authentication failed")
            })?;
        let authorization =
            sensitive_header_value(crate::route::bearer_bytes(&key)).map_err(|_| {
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
        insert_attribution(self.target.route.identity(), &mut headers);
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
                outcome: post_send_status_outcome(status),
                provider_accepted: false,
            }));
        }
        let content_type = response
            .headers()
            .get(&HeaderName::from_static("content-type"))
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if !is_sse_content_type(content_type) {
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
        let error = if body_reports_context_overflow(body) {
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
    if current.credential_generation() != generation_number(&hooks.generation) {
        return Err(MessagesAttemptProtocolError::new(
            "OpenRouter Messages dispatch credential changed",
        ));
    }
    Ok(())
}

/// The Messages wire of the shared OpenRouter target skeleton.
enum MessagesWire {}

impl AttemptWire for MessagesWire {
    type Response = plakous_messages::MessagesAttemptResponse;
    type AttemptError = MessagesAttemptError;
    type ProtocolError = MessagesAttemptProtocolError;

    fn model_error(
        descriptor: &ResolvedModelDescriptor,
        error: &MessagesAttemptError,
    ) -> ModelError {
        messages_model_error(descriptor, error)
    }

    fn observation_error(
        _descriptor: &ResolvedModelDescriptor,
        error: &MessagesAttemptProtocolError,
    ) -> ModelError {
        ModelError::new(
            inferred_failure_kind(None, error.exchange_break()),
            format!("OpenRouter Messages protocol failed: {error}"),
        )
    }
}

const OPENROUTER_MESSAGES_WIRE: WireProjection = WireProjection {
    label: "Messages",
    wire_domain: b"plakous-openrouter/messages-wire-v1",
    family_id: MESSAGES_FAMILY_ID,
    lowering_revision: LOWERING_REVISION,
    projection_revision: PROJECTION_REVISION,
    accounting_revision: ACCOUNTING_REVISION,
};

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
    let named = named_refusal_kind(error.provider_refusal(), facts);
    let kind = match error {
        MessagesAttemptError::Cancelled { .. } => ModelErrorKind::Cancelled,
        MessagesAttemptError::Unavailable => ModelErrorKind::Unavailable,
        _ => usage_limit_kind(error.usage_limit(), facts)
            .or_else(|| context_overflow_kind(error.context_overflow(), facts))
            .or(named)
            .unwrap_or_else(|| inferred_failure_kind(facts, error.exchange_break())),
    };
    let model_error = operation_error(
        descriptor,
        kind,
        facts,
        error.http_status().map(plakous_model::ModelHttpStatus::get),
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
    let mut features: BTreeMap<_, _> = [
        Capability::Streaming,
        Capability::Tools,
        Capability::Reasoning,
    ]
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

fn messages_lowering_error(error: plakous_messages::MessagesRequestError) -> ModelPreparationError {
    let kind = match error.kind() {
        MessagesRequestErrorKind::UnsupportedSemantic => {
            ModelPreparationErrorKind::UnsupportedCapability
        }
        MessagesRequestErrorKind::InvalidRequest => ModelPreparationErrorKind::InvalidRequest,
        MessagesRequestErrorKind::ProtectedHistory => ModelPreparationErrorKind::ProtectedHistory,
    };
    rejection(kind, "OpenRouter Messages request lowering failed")
}

#[cfg(test)]
mod tests;
