//! Executable exact-origin Anthropic API-key Messages binding.
// Claude resizes images to at most 1568 x 1568; tokens are bounded by pixels / 750.
pub(crate) const CLAUDE_IMAGE_MAX_TOKENS: u64 = 3279;

use crate::{
    ANTHROPIC_PLATFORM_ADMISSION_EXPIRES_AT_SECONDS, ANTHROPIC_PLATFORM_ADMISSION_REVISION,
    AnthropicAccountIdentity, AnthropicApiKeySource, AnthropicError, AnthropicErrorKind,
    AnthropicErrorPhase, AnthropicHttpDeadline, AnthropicLocalHttpServiceFactory,
    AnthropicPlatformCapabilityProfile, AnthropicPlatformModelConfig,
    AnthropicPlatformModelResolver, AnthropicPlatformRoute, MessagesRequestTranslator,
    ResolvedAnthropicMessages, ThinkingSupport, ToolOwnership, start_anthropic_messages_model_call,
};
use plakous_http::{Client, Limits, Origin};
use plakous_model::{
    Capability, CapacityCertificate, CapacityRule, Clock, ConfiguredLanguageModel, ContextContract,
    ContextEvidenceSource, ContextInputParts, CountCertificateKind, CountObservation,
    CredentialSelectionId, Digest, DigestDomain, EvidenceFreshness, FactSource, LanguageModel,
    MeasurementCertificate, ModelAdapterProfileId, ModelCacheAffinity, ModelCallContext,
    ModelError, ModelErrorKind, ModelFamilyId, ModelId, ModelPreparationContext,
    ModelPreparationError, ModelPreparationErrorKind, OutputBoundDisposition, PreparedCacheUse,
    PreparedCallRecord, PreparedModelCall, PreparedModelProjection, PreparedOutputLimit,
    PreparedTokenProjection, ProtocolId, ProviderAuthorityClassId, ProviderBehaviorConfiguration,
    ProviderProfileRevisionId, ProviderRouteSelection, ProviderSemanticEvidence,
    ReasoningEffortDisposition, ResolvedModelDescriptor, ResolvedProviderAuthorityBinding,
    RuntimeModelAuthority, RuntimeModelAuthorityError, RuntimeModelAuthorityOperation,
    SelectedProviderConfiguration, Support, SystemClock, SystemIdGenerator, Timestamp,
    ValidatedRequestPlan,
};
use std::{fmt, num::NonZeroU64, sync::Arc};

const WIRE_DOMAIN: &[u8] = b"plakous-anthropic/platform-messages-wire-v1";
const LOWERING_REVISION: &str = "anthropic.platform.lowering.v1";
const PROJECTION_REVISION: &str = "messages.projection.v3";
const ACCOUNTING_REVISION: &str = "messages.accounting.v1";
const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// Production API-key model with an explicit caller-owned input envelope.
pub struct AnthropicLanguageModel {
    resolved: ResolvedAnthropicMessages,
    source: Arc<dyn AnthropicApiKeySource>,
    client: Client,
    authority: RuntimeModelAuthority,
    configuration: plakous_model::CompiledProviderConfiguration,
    dial_origin: Option<Origin>,
}

impl AnthropicLanguageModel {
    /// Builds a stable API-key binding with no OAuth or origin fallback.
    pub fn configured(
        source: Arc<dyn AnthropicApiKeySource>,
        account: &str,
        model: &crate::AnthropicApiModel,
        admitted_facts: &[u8],
        behavior: ProviderBehaviorConfiguration,
        maximum_projected_input: NonZeroU64,
    ) -> Result<ConfiguredLanguageModel, AnthropicError> {
        Self::configured_with_client(
            source,
            account,
            model,
            admitted_facts,
            behavior,
            maximum_projected_input,
            Client::new_http1(Limits::for_streaming_inference())
                .map_err(configuration_error)?
                .pooled(),
        )
    }

    /// Builds the exact-origin executor with an explicit exchange observer.
    pub fn configured_with_exchange_observer(
        source: Arc<dyn AnthropicApiKeySource>,
        account: &str,
        model: &crate::AnthropicApiModel,
        admitted_facts: &[u8],
        behavior: ProviderBehaviorConfiguration,
        maximum_projected_input: NonZeroU64,
        observer: Option<Arc<dyn plakous_http::ExchangeObserver>>,
    ) -> Result<ConfiguredLanguageModel, AnthropicError> {
        Self::configured_with_client(
            source,
            account,
            model,
            admitted_facts,
            behavior,
            maximum_projected_input,
            Client::new_http1(Limits::for_streaming_inference())
                .map_err(configuration_error)?
                .pooled()
                .with_observer(observer),
        )
    }

    /// Builds the same exact-origin executor with an explicit transport client.
    pub fn configured_with_client(
        source: Arc<dyn AnthropicApiKeySource>,
        account: &str,
        model: &crate::AnthropicApiModel,
        admitted_facts: &[u8],
        behavior: ProviderBehaviorConfiguration,
        maximum_projected_input: NonZeroU64,
        client: Client,
    ) -> Result<ConfiguredLanguageModel, AnthropicError> {
        Self::build(
            source,
            account,
            model,
            admitted_facts,
            behavior,
            maximum_projected_input,
            client,
            None,
        )
    }

    /// Runs the production executor through an explicitly trusted HTTPS loopback transport.
    /// Credential authority and request headers remain bound to the official API route.
    #[doc(hidden)]
    #[allow(
        clippy::too_many_arguments,
        reason = "one exact API binding names its whole tuple"
    )]
    pub fn configured_with_local_transport(
        source: Arc<dyn AnthropicApiKeySource>,
        account: &str,
        model: &crate::AnthropicApiModel,
        admitted_facts: &[u8],
        behavior: ProviderBehaviorConfiguration,
        maximum_projected_input: NonZeroU64,
        client: Client,
        dial_origin: Origin,
    ) -> Result<ConfiguredLanguageModel, AnthropicError> {
        let authority = dial_origin.binding_authority();
        let Some((host, port)) = authority.rsplit_once(':') else {
            return Err(configuration_error(
                "Anthropic local transport requires HTTPS loopback with an explicit port",
            ));
        };
        if !matches!(host, "127.0.0.1" | "localhost" | "[::1]")
            || port.parse::<u16>().ok().is_none_or(|port| port == 0)
        {
            return Err(configuration_error(
                "Anthropic local transport requires HTTPS loopback with an explicit port",
            ));
        }
        Self::build(
            source,
            account,
            model,
            admitted_facts,
            behavior,
            maximum_projected_input,
            client,
            Some(dial_origin),
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "one exact API binding names its whole tuple"
    )]
    fn build(
        source: Arc<dyn AnthropicApiKeySource>,
        account: &str,
        model: &crate::AnthropicApiModel,
        admitted_facts: &[u8],
        behavior: ProviderBehaviorConfiguration,
        maximum_projected_input: NonZeroU64,
        client: Client,
        dial_origin: Option<Origin>,
    ) -> Result<ConfiguredLanguageModel, AnthropicError> {
        if model.status() != &crate::AnthropicApiModelStatus::Usable {
            return Err(configuration_error("Anthropic API model is not admitted"));
        }
        let family = model
            .family()
            .ok_or_else(|| configuration_error("Anthropic API model has no reviewed family"))?;
        let admitted_effort = match behavior.reasoning_effort {
            ReasoningEffortDisposition::Enabled(effort) => model.efforts().contains(&effort),
            ReasoningEffortDisposition::Disabled => false,
        };
        if !admitted_effort || !behavior.hosted_tools.is_empty() {
            return Err(configuration_error(
                "Anthropic API requires adaptive thinking at one of the model's published efforts, and local tools",
            ));
        }
        let OutputBoundDisposition::Enforced(maximum_output) = behavior.output_bound else {
            return Err(configuration_error(
                "Anthropic API requires a finite output bound",
            ));
        };
        if maximum_output.get() > model.max_tokens() {
            return Err(configuration_error(
                "Anthropic API output bound exceeds the admitted model's own output limit",
            ));
        }
        let config = AnthropicPlatformModelConfig::new(
            ModelId::new(model.id()).map_err(configuration_error)?,
            ModelFamilyId::new(family).map_err(configuration_error)?,
            maximum_output.get(),
            ModelAdapterProfileId::new(crate::config::ANTHROPIC_API_ADAPTIVE_PROFILE_ID)
                .map_err(configuration_error)?,
            AnthropicPlatformCapabilityProfile::AdaptiveClientTools,
        )?;
        crate::config::preflight_platform_admission(&config, SystemClock.now())?;
        plakous_messages::validate_messages_api_behavior(&behavior).map_err(configuration_error)?;
        let account_identity = AnthropicAccountIdentity::new(account)?;
        let key = source
            .current_key()
            .map_err(|_| configuration_error("Anthropic API credential unavailable"))?;
        let route = AnthropicPlatformRoute::for_account(key, account_identity)?;
        let account_binding = route
            .account()
            .stable_binding()
            .ok_or_else(|| configuration_error("Anthropic account binding unavailable"))?;
        let credential_binding = route.credential_binding()?;
        let now = SystemClock.now();
        let expiry = Timestamp::new(ANTHROPIC_PLATFORM_ADMISSION_EXPIRES_AT_SECONDS, 0)
            .map_err(configuration_error)?;
        let context = caller_context(
            account_binding,
            credential_binding,
            maximum_projected_input,
            now,
            expiry,
        )?;
        let resolved = AnthropicPlatformModelResolver::new(route.clone())
            .resolve(config)?
            .with_context_envelope(context);
        validate_descriptor(&resolved, Some(&route))?;
        let descriptor = resolved.descriptor();
        let authority = RuntimeModelAuthority::new(
            descriptor,
            account_binding,
            credential_binding,
            1,
            now,
            Some(expiry),
        )
        .map_err(configuration_error)?;
        let selected = SelectedProviderConfiguration::new(
            ProviderRouteSelection::new(
                descriptor.protocol().clone(),
                descriptor.route().clone(),
                descriptor.profile().clone(),
                ProviderProfileRevisionId::new("anthropic.platform.messages.v1")
                    .map_err(configuration_error)?,
                ModelAdapterProfileId::new(crate::ANTHROPIC_API_KEY_AUTH_PROFILE_ID)
                    .map_err(configuration_error)?,
                ProviderProfileRevisionId::new(crate::ANTHROPIC_API_KEY_AUTH_REVISION)
                    .map_err(configuration_error)?,
                ProviderAuthorityClassId::new(crate::ANTHROPIC_API_KEY_AUTHORITY_CLASS)
                    .map_err(configuration_error)?,
                CredentialSelectionId::new(crate::ANTHROPIC_API_KEY_CREDENTIAL_SELECTION)
                    .map_err(configuration_error)?,
                descriptor.requested_model().clone(),
            ),
            behavior,
        )
        .map_err(configuration_error)?;
        // Durable replay authority follows the API-key generation even when the
        // product account label remains stable across key rotation.
        // The caller's admitted-facts binding is opaque here: it only makes the
        // facts the session was admitted under identity-bearing.
        let mut evidence = b"anthropic.platform.messages.v4;api-version=2023-06-01;stable;no-oauth;turn-preaccept-retry-once;local-tools;adaptive".to_vec();
        evidence.extend_from_slice(admitted_facts);
        evidence.extend_from_slice(&credential_binding.identity_bytes());
        let semantic = ProviderSemanticEvidence::new(4, evidence).map_err(configuration_error)?;
        let binding = ResolvedProviderAuthorityBinding::new(descriptor, &authority, semantic)
            .map_err(configuration_error)?;
        let configuration = plakous_messages::compile_messages_api_configuration(
            &selected,
            descriptor.clone(),
            binding,
        )
        .map_err(configuration_error)?;
        let model = Arc::new(Self {
            resolved,
            source,
            client,
            authority: authority.clone(),
            configuration: configuration.clone(),
            dial_origin,
        });
        ConfiguredLanguageModel::new(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            model,
            configuration,
            authority,
        )
        .map_err(configuration_error)
    }

    fn validate_preparation(
        &self,
        plan: &ValidatedRequestPlan,
        context: &ModelPreparationContext,
        checked_at: Timestamp,
    ) -> Result<(), ModelPreparationError> {
        let request = plan.request();
        if request.selected_provider_configuration_id() != self.configuration.selected_identity()
            || request.effective_provider_configuration_id()
                != self.configuration.effective_identity()
            || !request_matches_configuration(request, self.configuration.selected().behavior())
        {
            return Err(rejection(
                ModelPreparationErrorKind::InvalidConfiguration,
                "Anthropic API request differs from compiled configuration",
            ));
        }
        self.resolved
            .descriptor()
            .validate_plan_admission(request, plan)?;
        context
            .authority()
            .validate(self.resolved.descriptor(), checked_at)?;
        if !self.authority.same_selected_authority(context.authority()) {
            return Err(RuntimeModelAuthorityError::ForeignAuthority.into());
        }
        Ok(())
    }

    /// Returns the complete resolved provider binding.
    #[must_use]
    pub const fn resolved(&self) -> &ResolvedAnthropicMessages {
        &self.resolved
    }
}

impl fmt::Debug for AnthropicLanguageModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnthropicLanguageModel")
            .field("resolved", &self.resolved)
            .finish_non_exhaustive()
    }
}

impl LanguageModel for AnthropicLanguageModel {
    fn reachability_probe(&self) -> Option<plakous_model::ReachabilityProbe> {
        let origin = self
            .dial_origin
            .clone()
            .or_else(|| Origin::parse(self.resolved.route().origin()).ok())?;
        Some(crate::http::bind_reachability_probe(
            self.client.clone(),
            origin,
            self.resolved.descriptor().route().clone(),
        ))
    }

    fn revalidate_runtime_authority(
        self: Arc<Self>,
        checked_at: Timestamp,
    ) -> RuntimeModelAuthorityOperation {
        Box::pin(async move {
            self.resolved
                .ensure_current_admission(checked_at)
                .map_err(|_| RuntimeModelAuthorityError::Stale)?;
            self.resolved
                .route()
                .validate_current_key(self.source.as_ref())
                .map_err(|_| RuntimeModelAuthorityError::Stale)?;
            self.authority
                .validate(self.resolved.descriptor(), checked_at)?;
            Ok(self.authority.clone())
        })
    }
    fn prepare_call(
        self: Arc<Self>,
        plan: ValidatedRequestPlan,
        context: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError> {
        self.validate_preparation(&plan, &context, context.prepared_at())?;
        self.resolved
            .ensure_current_admission(SystemClock.now())
            .map_err(|error| rejection(ModelPreparationErrorKind::InvalidConfiguration, error))?;
        self.resolved
            .route()
            .validate_current_key(self.source.as_ref())
            .map_err(|_| {
                rejection(
                    ModelPreparationErrorKind::InvalidConfiguration,
                    "Anthropic API credential changed or unavailable",
                )
            })?;
        let prepared = MessagesRequestTranslator::new(self.resolved.clone())
            .require_exact_thinking_replay()
            .with_completed_history_prefix(context.completed_history_prefix())
            .with_session_id(context.cache_affinity().map(ModelCacheAffinity::session))
            .translate_with_provenance(
                &plan,
                ToolOwnership::default(),
                context.active_family_artifact(),
                context.previous_response_metadata(),
            )
            .map_err(|error| rejection(error.preparation_error_kind(), error))?;
        let wire_size = u64::try_from(prepared.body().len()).map_err(|_| {
            rejection(
                ModelPreparationErrorKind::ResourceLimit,
                "Anthropic API Messages request exceeded the size bound",
            )
        })?;
        let wire_digest = Digest::sha256(DigestDomain::Blob, 1, &[WIRE_DOMAIN, prepared.body()])
            .map_err(|error| rejection(ModelPreparationErrorKind::InvalidConfiguration, error))?;
        let max_tokens = plan.request().limits().max_output_tokens().ok_or_else(|| {
            rejection(
                ModelPreparationErrorKind::InvalidConfiguration,
                "Anthropic API max_tokens was zero",
            )
        })?;
        let observation = wire_observation(self.resolved.descriptor(), wire_size)?;
        let token_projection =
            PreparedTokenProjection::new(wire_digest, wire_size, Some(observation))
                .map_err(|error| rejection(ModelPreparationErrorKind::InvalidConfiguration, error))?
                .with_input_parts(ContextInputParts::measure_wire(prepared.body()))
                .with_wire_input(prepared.body(), Some(CLAUDE_IMAGE_MAX_TOKENS), &context)?;
        let output_limit = PreparedOutputLimit::enforced_wire_maximum(
            max_tokens,
            "max_tokens",
            wire_digest,
            LOWERING_REVISION,
        )
        .map_err(|error| rejection(ModelPreparationErrorKind::InvalidConfiguration, error))?;
        let protocol = ProtocolId::new("messages")
            .map_err(|error| rejection(ModelPreparationErrorKind::InvalidConfiguration, error))?;
        let projection = PreparedModelProjection::new(
            self.resolved.descriptor(),
            &plan,
            &context,
            protocol,
            LOWERING_REVISION,
            PROJECTION_REVISION,
            ACCOUNTING_REVISION,
            wire_digest,
            wire_size,
            token_projection,
            PreparedCacheUse::None,
            Some(output_limit),
        )
        .map_err(|error| rejection(ModelPreparationErrorKind::InvalidConfiguration, error))?;
        let record = PreparedCallRecord::for_prepared(
            self.resolved.descriptor(),
            &plan,
            projection,
            &context,
        )?;
        let deadline =
            AnthropicHttpDeadline::new(std::time::Duration::from_secs(DEFAULT_TIMEOUT_SECS))
                .map_err(|error| {
                    rejection(ModelPreparationErrorKind::InvalidConfiguration, error)
                })?;
        let descriptor = self.resolved.descriptor().clone();
        let model = self.clone();
        let route = self.resolved.route().clone();
        let clock = Arc::new(SystemClock);
        let ids = Arc::new(SystemIdGenerator);
        Ok(PreparedModelCall::new(
            record,
            move |call_context: ModelCallContext| {
                Box::pin(async move {
                    if call_context.cancellation().is_cancelled() {
                        return Err(ModelError::new(
                            ModelErrorKind::Cancelled,
                            "Anthropic API call cancelled before dispatch",
                        ));
                    }
                    if call_context
                        .deadline()
                        .is_some_and(|deadline| deadline <= SystemClock.now())
                    {
                        return Err(ModelError::new(
                            ModelErrorKind::Timeout,
                            "Anthropic API deadline elapsed before dispatch",
                        ));
                    }
                    model
                        .validate_preparation(&plan, &context, SystemClock.now())
                        .map_err(|_| {
                            ModelError::new(
                                ModelErrorKind::InvalidRequest,
                                "Anthropic API prepared configuration is no longer valid",
                            )
                        })?;
                    model
                        .resolved
                        .ensure_current_admission(SystemClock.now())
                        .map_err(|_| {
                            ModelError::new(
                                ModelErrorKind::Unavailable,
                                "Anthropic admission expired",
                            )
                        })?;
                    route
                        .validate_current_key(model.source.as_ref())
                        .map_err(|_| {
                            ModelError::new(
                                ModelErrorKind::Authentication,
                                "Anthropic API credential changed or unavailable",
                            )
                        })?;
                    let factory = match &model.dial_origin {
                        Some(origin) => AnthropicLocalHttpServiceFactory::with_runtime(
                            model.client.clone(),
                            origin.clone(),
                            route,
                            prepared,
                            clock,
                            ids,
                            deadline,
                        ),
                        None => AnthropicLocalHttpServiceFactory::for_route(
                            model.client.clone(),
                            route,
                            prepared,
                            clock,
                            ids,
                            deadline,
                        ),
                    }
                    .map_err(|error| {
                        ModelError::new(ModelErrorKind::InvalidRequest, error.to_string())
                    })?;
                    let factory = {
                        let retry_model = Arc::clone(&model);
                        let retry_plan = plan.clone();
                        let retry_context = context.clone();
                        factory.with_retry(Arc::new(move |route| {
                            let retry_model = Arc::clone(&retry_model);
                            let retry_plan = retry_plan.clone();
                            let retry_context = retry_context.clone();
                            Box::pin(async move {
                                let checked_at = SystemClock.now();
                                retry_model
                                    .validate_preparation(&retry_plan, &retry_context, checked_at)
                                    .map_err(|_| {
                                        AnthropicError::new(
                                            AnthropicErrorKind::InvalidConfiguration,
                                            AnthropicErrorPhase::Connect,
                                            "Anthropic retry admission changed",
                                        )
                                    })?;
                                retry_model
                                    .resolved
                                    .ensure_current_admission(checked_at)
                                    .map_err(|_| {
                                        AnthropicError::new(
                                            AnthropicErrorKind::InvalidConfiguration,
                                            AnthropicErrorPhase::Connect,
                                            "Anthropic retry admission expired",
                                        )
                                    })?;
                                route
                                    .validate_current_key(retry_model.source.as_ref())
                                    .map_err(|_| {
                                        AnthropicError::new(
                                            AnthropicErrorKind::Authentication,
                                            AnthropicErrorPhase::Connect,
                                            "Anthropic retry credential changed",
                                        )
                                    })?;
                                Ok(crate::AnthropicRetryGrant::for_api_key(route))
                            })
                        }))
                    };
                    start_anthropic_messages_model_call(factory, descriptor, call_context).await
                })
            },
        ))
    }
}

fn request_matches_configuration(
    request: &plakous_model::ModelRequest,
    behavior: &ProviderBehaviorConfiguration,
) -> bool {
    let reasoning = matches!(
        (request.reasoning(), behavior.reasoning_effort),
        (plakous_model::ReasoningPolicy::Enabled { effort: Some(requested), requirement: plakous_model::CapabilityRequirement::Required },
         ReasoningEffortDisposition::Enabled(configured)) if requested == configured
    );
    reasoning
        && request.output() == &behavior.output
        && request.processing_tier() == &behavior.processing_tier
        && request.cache() == behavior.cache
        && behavior.matches_request_execution_controls(request)
        && request.context_admission_requirement() == behavior.context_evidence_requirement
        && request.limits().stop_sequences().is_empty()
        && request.native().entries().is_empty()
        && request.tools() == behavior.tools.as_slice()
        && request.hosted_tools() == behavior.hosted_tools
}

fn configuration_error(error: impl fmt::Display) -> AnthropicError {
    AnthropicError::new(
        AnthropicErrorKind::InvalidConfiguration,
        AnthropicErrorPhase::Prepare,
        error.to_string(),
    )
}
fn rejection(kind: ModelPreparationErrorKind, error: impl fmt::Display) -> ModelPreparationError {
    ModelPreparationError::rejected(kind, error.to_string())
}
fn wire_observation(
    descriptor: &ResolvedModelDescriptor,
    bytes: u64,
) -> Result<CountObservation, ModelPreparationError> {
    let measurement = descriptor
        .context_contract()
        .and_then(|c| c.measurements().first())
        .ok_or_else(|| {
            rejection(
                ModelPreparationErrorKind::InvalidConfiguration,
                "Anthropic context measurement unavailable",
            )
        })?;
    CountObservation::new(measurement.certificate_id(), measurement.revision(), bytes)
        .map_err(|e| rejection(ModelPreparationErrorKind::InvalidConfiguration, e))
}
fn caller_context(
    account: Digest,
    credential: Digest,
    maximum: NonZeroU64,
    now: Timestamp,
    expiry: Timestamp,
) -> Result<ContextContract, AnthropicError> {
    let authority = RuntimeModelAuthority::authority_class_for(account, credential)
        .map_err(configuration_error)?;
    let declaration = Digest::sha256(
        DigestDomain::ProviderSemanticEvidence,
        1,
        &[
            b"anthropic.platform.caller-input-envelope.v1",
            &maximum.get().to_be_bytes(),
        ],
    )
    .map_err(configuration_error)?;
    let freshness = EvidenceFreshness::new(now, Some(expiry)).map_err(configuration_error)?;
    let capacity = CapacityCertificate::custom_envelope(
        "anthropic.platform.caller-capacity",
        "v1",
        CapacityRule::ProviderReservedInput {
            reported_context_window: None,
            maximum_projected_input: maximum.get(),
            auto_compact_input: None,
        },
        authority,
        freshness,
        declaration,
        "v1",
    )
    .map_err(configuration_error)?;
    let measurement = MeasurementCertificate::new(
        "anthropic.platform.wire-bytes",
        "v1",
        "complete-messages-body-byte-upper-bound",
        "v1",
        ContextEvidenceSource::CallerDeclaration,
        CountCertificateKind::CustomEnvelopeUpperBound { declaration },
        Some(authority),
        freshness,
    )
    .map_err(configuration_error)?;
    ContextContract::new(
        ProtocolId::new("messages").map_err(configuration_error)?,
        ACCOUNTING_REVISION,
        PROJECTION_REVISION,
        "anthropic.platform.output-reservation.v1",
        capacity,
        vec![measurement],
    )
    .map_err(configuration_error)
}
#[allow(
    clippy::too_many_lines,
    reason = "one validation boundary keeps descriptor surface, provenance, freshness, and factory-route checks adjacent"
)]
fn validate_descriptor(
    resolved: &ResolvedAnthropicMessages,
    factory_route: Option<&AnthropicPlatformRoute>,
) -> Result<(), AnthropicError> {
    let capabilities = resolved.descriptor().capabilities();
    if capabilities
        .get(Capability::Streaming)
        .is_none_or(|support| support.support() != Support::Native)
    {
        return Err(descriptor_claim_error(
            "Anthropic LanguageModel requires native streaming evidence",
        ));
    }
    for capability in [
        Capability::StructuredOutput,
        Capability::Continuation,
        Capability::NativeOptions,
    ] {
        if capabilities
            .get(capability)
            .is_some_and(|support| support.support() != Support::Unsupported)
        {
            return Err(descriptor_claim_error(
                "Anthropic descriptor claims an adapter-excluded capability",
            ));
        }
    }
    let cache_ok = resolved.dialect().is_claude_oauth()
        && capabilities
            .get(Capability::Caching)
            .is_some_and(|support| support.support() == Support::Native);
    if !cache_ok
        && capabilities
            .get(Capability::Caching)
            .is_some_and(|support| support.support() != Support::Unsupported)
    {
        return Err(descriptor_claim_error(
            "Anthropic descriptor claims an adapter-excluded capability",
        ));
    }
    let capability_wire = serde_json::to_value(capabilities).map_err(|_| {
        descriptor_claim_error("Anthropic descriptor capabilities could not be inspected")
    })?;
    let native_namespaces = capability_wire
        .get("native_option_namespaces")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| {
            descriptor_claim_error("Anthropic descriptor native namespaces were malformed")
        })?;
    for support in native_namespaces.values() {
        let support = support
            .get("support")
            .cloned()
            .and_then(|value| serde_json::from_value::<Support>(value).ok())
            .ok_or_else(|| {
                descriptor_claim_error("Anthropic native namespace support was malformed")
            })?;
        if support != Support::Unsupported {
            return Err(descriptor_claim_error(
                "Anthropic descriptor claims an unsupported native option namespace",
            ));
        }
    }
    if matches!(resolved.model().thinking(), ThinkingSupport::Disabled)
        && capabilities
            .get(Capability::Reasoning)
            .is_some_and(|support| support.support() != Support::Unsupported)
    {
        return Err(descriptor_claim_error(
            "Anthropic descriptor claims reasoning for a thinking-disabled model",
        ));
    }
    for capability in [
        Capability::Tools,
        Capability::Reasoning,
        Capability::ParallelTools,
    ] {
        if capabilities.get(capability).is_some_and(|support| {
            matches!(support.support(), Support::Emulated | Support::Degraded)
        }) {
            return Err(descriptor_claim_error(
                "Anthropic descriptor claims an unimplemented emulated or degraded capability",
            ));
        }
    }
    let positive_facts = capabilities
        .features()
        .values()
        .filter(|support| support.support() != Support::Unsupported);
    match resolved.resolution_trust() {
        crate::AnthropicResolutionTrust::CallerAuthored => {
            if positive_facts
                .clone()
                .any(|support| support.provenance().source() != FactSource::TestFixture)
            {
                return Err(descriptor_claim_error(
                    "caller-authored Anthropic capabilities cannot become routable production facts",
                ));
            }
            if factory_route.is_some() {
                return Err(descriptor_claim_error(
                    "fixture-backed Anthropic descriptors cannot use a credential-bearing production factory",
                ));
            }
        }
        crate::AnthropicResolutionTrust::ReviewedAdmission {
            revision,
            reviewed_at,
            expires_at,
        } => {
            let now = resolved.clock().now();
            resolved.ensure_current_admission(now)?;
            if revision != ANTHROPIC_PLATFORM_ADMISSION_REVISION {
                return Err(descriptor_claim_error(
                    "routable Anthropic model used an unknown admission revision",
                ));
            }
            if positive_facts.clone().any(|support| {
                let provenance = support.provenance();
                provenance.source() != FactSource::BuiltIn
                    || provenance.observed_at() != reviewed_at
                    || provenance.expires_at() != Some(expires_at)
                    || provenance.is_expired(now)
            }) {
                return Err(descriptor_claim_error(
                    "routable Anthropic capabilities require current reviewed built-in admission facts",
                ));
            }
            let factory_route = factory_route.ok_or_else(|| {
                descriptor_claim_error(
                    "routable Anthropic models require an exactly bound Platform factory",
                )
            })?;
            crate::AnthropicMessagesTargetBinding::from_resolved(resolved)?
                .validate_route(factory_route)?;
        }
    }
    Ok(())
}

fn descriptor_claim_error(message: &'static str) -> AnthropicError {
    AnthropicError::new(
        AnthropicErrorKind::InvalidConfiguration,
        AnthropicErrorPhase::Prepare,
        message,
    )
}

#[cfg(test)]
#[path = "model_tests.rs"]
mod tests;
