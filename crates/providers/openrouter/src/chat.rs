//! First-class OpenRouter Chat Completions dialect and executable target.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::Stream;
use plakous_attempt::{ExchangeBreak, FailureRisk, PhysicalRequestOutcome, TerminalFacts};
use plakous_chat_completions::{
    ChatAttempt, ChatAttemptError, ChatAttemptProtocolError, ChatDisabledReasoningWire,
    ChatHttpAttemptService, ChatHttpErrorHead, ChatHttpHooks, ChatHttpResponseHead, ChatHttpSafety,
    ChatOperationBinding, ChatOutputLimitField, ChatPhysicalRequestBinding, ChatReasoningReplay,
    ChatRequestPolicy, ChatRouterPolicy, ChatStreamError, ChatStreamExtensions, ChatStreamLimits,
    PreparedChatHttpOperation, PreparedChatStreamBinding, ReasoningAliasPolicy,
    call_chat_operation, lower_request,
};
use plakous_http::{
    Client, Failure as HttpFailure, HeaderMap, HeaderName, HeaderValue, Method, Origin, Request,
    Response, SensitiveBytes, StatusCode, sensitive_header_value,
};
use plakous_model::{
    Capability, CapabilityConstraints, CapabilitySet, CapacityRule, Clock,
    CompiledProviderConfiguration, ConfiguredLanguageModel, ContextContract, ContextContractError,
    ContextEvidenceClass, CountObservation, CredentialSelectionId, Digest, DigestDomain,
    EffectiveProviderConfiguration, FactProvenance, FactSource, FamilyPolicyEnvelope,
    FeatureSupport, LanguageModel, ModelAdapterProfileId, ModelError, ModelErrorContext,
    ModelErrorKind, ModelErrorPhase, ModelEvent, ModelFamilyId, ModelId,
    ModelImplementationIdentity, ModelPreparationContext, ModelPreparationError,
    ModelPreparationErrorKind, ModelRetryRisk, ModelRetryability, NamespaceId, PreparedCacheUse,
    PreparedCallRecord, PreparedModelCall, PreparedModelProjection, PreparedOutputLimit,
    PreparedTokenProjection, ProtocolId, ProviderAuthorityClassId, ProviderBehaviorConfiguration,
    ProviderProfileRevisionId, ProviderRouteSelection, ProviderSemanticEvidence,
    ResolvedModelDescriptor, ResolvedProviderAuthorityBinding, RuntimeModelAuthority,
    RuntimeModelAuthorityError, RuntimeModelAuthorityOperation, SelectedProviderConfiguration,
    Support, SystemIdGenerator, Timestamp, ValidatedRequestPlan,
};
use serde::Serialize;
use thiserror::Error;

use crate::{
    OpenRouterApiKeyGenerationGrant, OpenRouterChatRoute, OpenRouterRetentionPolicy,
    OpenRouterRoutingPolicy,
};

/// Exact OpenRouter Chat Completions resource path.
pub const OPENROUTER_CHAT_COMPLETIONS_PATH: &str = "/api/v1/chat/completions";
/// Explicit protocol/dialect identity for the initial OpenRouter Chat grammar.
pub const OPENROUTER_CHAT_PROTOCOL_ID: &str = "openrouter.chat-completions.v1";
/// Provider-neutral family identity used by prepared projections.
pub const CHAT_COMPLETIONS_FAMILY_ID: &str = "chat-completions-family";
/// OpenRouter Chat dialect revision.
pub(crate) const OPENROUTER_CHAT_DIALECT_REVISION: u32 = 1;
pub(crate) const OPENROUTER_CHAT_PROFILE: &str = "openrouter.chat.dynamic-v1";
const OPENROUTER_CHAT_MODEL_FAMILY: &str = "openrouter.dynamic-chat";
const OPENROUTER_CHAT_IMPLEMENTATION: &str = "plakous.openrouter.chat";
const OPENROUTER_CHAT_PROVENANCE: &str = "caller.openrouter.experimental";
const OPENROUTER_CHAT_ACCOUNTING_REVISION: &str = "openrouter-chat-custom-accounting-v1";
pub(crate) const OPENROUTER_CHAT_LOWERING_REVISION: &str = "openrouter-chat-lowering-v3";
const OPENROUTER_CHAT_PROJECTION_REVISION: &str = "openrouter-chat-canonical-v1";
const OPENROUTER_CHAT_INTEGRATION_REVISION: &str = "openrouter-chat-integration-v1";
const OPENROUTER_CHAT_AUTH_PROFILE: &str = "openrouter.platform-api-key";
const OPENROUTER_CHAT_AUTH_REVISION: &str = "openrouter-platform-api-key-v1";
const OPENROUTER_CHAT_AUTHORITY_CLASS: &str = "platform-api-key";
const OPENROUTER_CHAT_CREDENTIAL_SELECTION: &str = "openrouter-product-key-generation";
pub(crate) const OPENROUTER_CHAT_REVIEWED_AT: u64 = 1_792_540_800;
pub(crate) const OPENROUTER_CHAT_REVIEW_EXPIRES_AT: u64 = OPENROUTER_CHAT_REVIEWED_AT + 30 * 86_400;

/// Provider-owned exact reasoning-detail replay selection.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OpenRouterChatReasoningReplay {
    /// Replay only visible reasoning text.
    #[default]
    VisibleOnly,
    /// Preserve compatible opaque reasoning details exactly.
    ExactDetails,
}

/// Provider-owned response reasoning-field selection.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OpenRouterChatReasoningAlias {
    /// Require the `reasoning` response field.
    Reasoning,
    /// Require the `reasoning_content` response field.
    ReasoningContent,
    /// Bind the first observed supported spelling for exact replay.
    #[default]
    BindFirst,
}

/// Small provider-owned Chat behavior profile for the curated Custom lane.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OpenRouterChatProfile {
    /// Opaque reasoning-detail replay selection.
    pub reasoning_replay: OpenRouterChatReasoningReplay,
    /// Response reasoning-field selection.
    pub reasoning_alias: OpenRouterChatReasoningAlias,
}

impl OpenRouterChatProfile {
    fn kernel_replay(self) -> ChatReasoningReplay {
        match self.reasoning_replay {
            OpenRouterChatReasoningReplay::VisibleOnly => ChatReasoningReplay::VisibleOnly,
            OpenRouterChatReasoningReplay::ExactDetails => ChatReasoningReplay::ExactDetails,
        }
    }

    fn kernel_alias(self) -> ReasoningAliasPolicy {
        match self.reasoning_alias {
            OpenRouterChatReasoningAlias::Reasoning => ReasoningAliasPolicy::Reasoning,
            OpenRouterChatReasoningAlias::ReasoningContent => {
                ReasoningAliasPolicy::ReasoningContent
            }
            OpenRouterChatReasoningAlias::BindFirst => ReasoningAliasPolicy::EitherBindFirst,
        }
    }
}

/// Complete endpoint-routing disposition for one selected OpenRouter tuple.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub enum OpenRouterChatRoutingPolicy {
    /// Reserved exact endpoint pin; currently unavailable for construction.
    StrictPinned(OpenRouterRoutingPolicy),
    /// Explicitly delegate endpoint selection to the gateway.
    GatewayManaged,
}

impl OpenRouterChatRoutingPolicy {
    /// Reports whether another wire protocol can be selected after admission.
    #[must_use]
    pub const fn allows_cross_protocol_fallback(&self) -> bool {
        false
    }

    /// Returns the selected pinned route, when endpoint pinning was requested.
    #[must_use]
    pub const fn pinned(&self) -> Option<&OpenRouterRoutingPolicy> {
        match self {
            Self::StrictPinned(policy) => Some(policy),
            Self::GatewayManaged => None,
        }
    }
}

/// Bounded `supports_tool_choice` modes attested for one endpoint.
#[allow(
    clippy::struct_excessive_bools,
    reason = "OpenRouter attests four independent tool_choice wire modes"
)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct OpenRouterToolChoiceModes {
    /// Upstream honors `tool_choice: "none"`.
    pub none: bool,
    /// Upstream honors `tool_choice: "auto"`.
    pub auto: bool,
    /// Upstream honors `tool_choice: "required"`.
    pub required: bool,
    /// Upstream honors `tool_choice: "function"`.
    pub function: bool,
}

/// Bounded model-level reasoning facts attested by discovery.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct OpenRouterReasoningFacts {
    pub(crate) supported_efforts: Option<Vec<String>>,
    pub(crate) default_effort: Option<String>,
    pub(crate) default_enabled: bool,
    pub(crate) supports_max_tokens: bool,
    pub(crate) mandatory: bool,
}

impl OpenRouterReasoningFacts {
    /// Returns attested effort tokens when the gateway enumerates them.
    #[must_use]
    pub fn supported_efforts(&self) -> Option<&[String]> {
        self.supported_efforts.as_deref()
    }

    /// Returns whether reasoning is enabled by default on this model.
    #[must_use]
    pub const fn default_enabled(&self) -> bool {
        self.default_enabled
    }

    /// Returns whether reasoning is mandatory for this model.
    #[must_use]
    pub const fn mandatory(&self) -> bool {
        self.mandatory
    }

    /// Returns whether the model takes a reasoning token budget.
    #[must_use]
    pub const fn supports_max_tokens(&self) -> bool {
        self.supports_max_tokens
    }
}

/// One bounded endpoint candidate reported for an exact dynamic model.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct OpenRouterChatEndpointCandidate {
    pub(crate) endpoint: String,
    pub(crate) provider_name: String,
    pub(crate) supported_parameters: BTreeSet<String>,
    pub(crate) context_length: Option<u64>,
    pub(crate) max_prompt_tokens: Option<u64>,
    pub(crate) max_completion_tokens: Option<u64>,
    pub(crate) tool_choice: OpenRouterToolChoiceModes,
    pub(crate) supports_implicit_caching: bool,
}

impl OpenRouterChatEndpointCandidate {
    /// Returns the exact endpoint tag used by strict routing.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Returns the provider name reported for this endpoint.
    #[must_use]
    pub fn provider_name(&self) -> &str {
        &self.provider_name
    }

    /// Returns the exact observed parameter names without capability promotion.
    #[must_use]
    pub const fn supported_parameters(&self) -> &BTreeSet<String> {
        &self.supported_parameters
    }

    /// Returns the endpoint-specific total context bound when reported.
    #[must_use]
    pub const fn context_length(&self) -> Option<u64> {
        self.context_length
    }

    /// Returns the endpoint-specific prompt bound when reported.
    #[must_use]
    pub const fn max_prompt_tokens(&self) -> Option<u64> {
        self.max_prompt_tokens
    }

    /// Returns the endpoint-specific completion bound when reported.
    #[must_use]
    pub const fn max_completion_tokens(&self) -> Option<u64> {
        self.max_completion_tokens
    }

    /// Returns attested `supports_tool_choice` modes without promotion.
    #[must_use]
    pub const fn tool_choice(&self) -> OpenRouterToolChoiceModes {
        self.tool_choice
    }
}

/// Fresh authenticated discovery evidence for one literal dynamic model.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct OpenRouterChatModelEvidence {
    model: ModelId,
    catalog_snapshot_identity: Digest,
    route_fingerprint: String,
    account_binding: Digest,
    credential_authority_binding: Digest,
    endpoints: Vec<OpenRouterChatEndpointCandidate>,
    model_supported_parameters: BTreeSet<String>,
    reasoning: Option<OpenRouterReasoningFacts>,
    context_length: u64,
    image_input: bool,
    observed_at: Timestamp,
    expires_at: Timestamp,
}

impl OpenRouterChatModelEvidence {
    pub(crate) fn from_authenticated_discovery(
        model: ModelId,
        route: &crate::OpenRouterChatRouteIdentity,
        endpoints: Vec<OpenRouterChatEndpointCandidate>,
        model_supported_parameters: BTreeSet<String>,
        reasoning: Option<OpenRouterReasoningFacts>,
        context_length: u64,
        image_input: bool,
        observed_at: Timestamp,
        expires_at: Timestamp,
    ) -> Result<Self, OpenRouterChatBuildError> {
        if !concrete_model_id(model.as_str())
            || endpoints.is_empty()
            || context_length == 0
            || expires_at <= observed_at
        {
            return Err(OpenRouterChatBuildError::InvalidDiscoveryEvidence);
        }
        let catalog_identity = crate::discovery::catalog_snapshot_identity(
            route.fingerprint(),
            model.as_str(),
            context_length,
            &model_supported_parameters,
            reasoning.as_ref(),
            image_input,
            &endpoints,
        )
        .map_err(|_| OpenRouterChatBuildError::Identity)?;
        Ok(Self {
            model,
            catalog_snapshot_identity: catalog_identity,
            route_fingerprint: route.fingerprint().to_owned(),
            account_binding: route.account_binding(),
            credential_authority_binding: route.credential_authority_binding(),
            endpoints,
            model_supported_parameters,
            reasoning,
            context_length,
            image_input,
            observed_at,
            expires_at,
        })
    }

    /// Returns the literal discovered model ID.
    #[must_use]
    pub const fn model(&self) -> &ModelId {
        &self.model
    }

    /// Returns evidence expiry.
    #[must_use]
    pub const fn expires_at(&self) -> Timestamp {
        self.expires_at
    }

    /// Returns bounded endpoint candidates without promoting their facts.
    #[must_use]
    pub fn endpoints(&self) -> &[OpenRouterChatEndpointCandidate] {
        &self.endpoints
    }

    /// Returns the authenticated model-level context bound.
    #[must_use]
    pub const fn context_length(&self) -> u64 {
        self.context_length
    }

    /// Returns exact understood model-level parameter names.
    #[must_use]
    pub const fn supported_parameters(&self) -> &BTreeSet<String> {
        &self.model_supported_parameters
    }

    /// Whether the catalog row's `architecture.input_modalities` lists `image`.
    #[must_use]
    pub const fn image_input(&self) -> bool {
        self.image_input
    }

    /// Returns attested model-level reasoning facts when the gateway reports them.
    #[must_use]
    pub fn reasoning(&self) -> Option<&OpenRouterReasoningFacts> {
        self.reasoning.as_ref()
    }

    pub(crate) fn validates(
        &self,
        route: &crate::OpenRouterChatRouteIdentity,
        now: Timestamp,
    ) -> bool {
        self.route_fingerprint == route.fingerprint()
            && self.account_binding == route.account_binding()
            && self.credential_authority_binding == route.credential_authority_binding()
            && crate::discovery::catalog_snapshot_identity(
                route.fingerprint(),
                self.model.as_str(),
                self.context_length,
                &self.model_supported_parameters,
                self.reasoning.as_ref(),
                self.image_input,
                &self.endpoints,
            )
            .is_ok_and(|snapshot| snapshot == self.catalog_snapshot_identity)
            && self.observed_at <= now
            && now < self.expires_at
    }
}

/// Immutable dynamic OpenRouter Chat target.
#[derive(Clone)]
pub struct OpenRouterChatTarget {
    route: OpenRouterChatRoute,
    routing: OpenRouterChatRoutingPolicy,
    evidence: OpenRouterChatModelEvidence,
    descriptor: ResolvedModelDescriptor,
    target_identity: Digest,
    reasoning_replay: ChatReasoningReplay,
    reasoning_alias: ReasoningAliasPolicy,
}

impl OpenRouterChatTarget {
    /// Creates the curated GatewayManaged Custom target from authenticated discovery.
    pub fn gateway_managed(
        route: OpenRouterChatRoute,
        evidence: OpenRouterChatModelEvidence,
        context_contract: ContextContract,
    ) -> Result<Self, OpenRouterChatBuildError> {
        Self::gateway_managed_with_profile(
            route,
            evidence,
            context_contract,
            OpenRouterChatProfile::default(),
        )
    }

    /// Creates the curated GatewayManaged Custom target with provider-owned behavior choices.
    pub fn gateway_managed_with_profile(
        route: OpenRouterChatRoute,
        evidence: OpenRouterChatModelEvidence,
        context_contract: ContextContract,
        profile: OpenRouterChatProfile,
    ) -> Result<Self, OpenRouterChatBuildError> {
        Self::new(
            route,
            OpenRouterChatRoutingPolicy::GatewayManaged,
            evidence,
            context_contract,
            profile.kernel_replay(),
            profile.kernel_alias(),
        )
    }

    /// Resolves an arbitrary fresh catalog model as Custom/experimental.
    #[doc(hidden)]
    pub fn new(
        route: OpenRouterChatRoute,
        routing: OpenRouterChatRoutingPolicy,
        evidence: OpenRouterChatModelEvidence,
        context_contract: ContextContract,
        reasoning_replay: ChatReasoningReplay,
        reasoning_alias: ReasoningAliasPolicy,
    ) -> Result<Self, OpenRouterChatBuildError> {
        if !matches!(
            context_contract.capacity().evidence_class(),
            ContextEvidenceClass::CustomEnvelope { .. }
        ) {
            return Err(OpenRouterChatBuildError::NonCustomContextAuthority);
        }
        if !context_within_discovery(&context_contract, evidence.context_length) {
            return Err(OpenRouterChatBuildError::ContextExceedsDiscovery);
        }
        let now = route.clock.now();
        if !evidence.validates(route.identity(), now) {
            return Err(OpenRouterChatBuildError::StaleEvidence);
        }
        reviewed_until(evidence.expires_at(), now)?;
        if matches!(routing, OpenRouterChatRoutingPolicy::StrictPinned(_)) {
            return Err(OpenRouterChatBuildError::StrictPinnedUnavailable);
        }
        let protocol = ProtocolId::new(OPENROUTER_CHAT_PROTOCOL_ID)?;
        let profile = ModelAdapterProfileId::new(OPENROUTER_CHAT_PROFILE)?;
        let family = ModelFamilyId::new(OPENROUTER_CHAT_MODEL_FAMILY)?;
        let descriptor = ResolvedModelDescriptor::new_custom(
            protocol,
            route.identity().route_id().clone(),
            evidence.model.clone(),
            None,
            Some(family),
            profile,
            // The reviewed dialect supplies protocol semantics. Dynamic
            // discovery remains only an exact endpoint prerequisite.
            openrouter_chat_dialect_capabilities(&evidence)?,
        )
        .with_context_contract(context_contract);
        let route_binding = route.identity().fingerprint();
        let routing_bytes =
            serde_json::to_vec(&routing).map_err(|_| OpenRouterChatBuildError::Identity)?;
        let target_identity = Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[
                b"plakous-openrouter/chat-target-v1",
                OPENROUTER_CHAT_COMPLETIONS_PATH.as_bytes(),
                OPENROUTER_CHAT_PROTOCOL_ID.as_bytes(),
                route_binding.as_bytes(),
                evidence.model.as_str().as_bytes(),
                &evidence
                    .admission_identity(crate::discovery::AdmissionWire::Chat)?
                    .identity_bytes(),
                &routing_bytes,
                reasoning_replay_label(reasoning_replay).as_bytes(),
                reasoning_alias_label(reasoning_alias).as_bytes(),
                b"integration=custom;support=experimental;cross-protocol-fallback=false",
            ],
        )?;
        Ok(Self {
            route,
            routing,
            evidence,
            descriptor,
            target_identity,
            reasoning_replay,
            reasoning_alias,
        })
    }

    /// Returns the exact dynamic descriptor.
    #[must_use]
    pub const fn descriptor(&self) -> &ResolvedModelDescriptor {
        &self.descriptor
    }

    /// Returns the exact selected routing policy.
    #[must_use]
    pub const fn routing(&self) -> &OpenRouterChatRoutingPolicy {
        &self.routing
    }

    /// Returns the literal selected model.
    #[must_use]
    pub const fn model(&self) -> &ModelId {
        self.evidence.model()
    }

    /// Returns authenticated discovery evidence for the selected model.
    #[must_use]
    pub(crate) const fn evidence(&self) -> &OpenRouterChatModelEvidence {
        &self.evidence
    }

    /// Returns the exact credential-free target identity.
    #[must_use]
    pub const fn target_identity(&self) -> Digest {
        self.target_identity
    }

    fn runtime_authority(
        &self,
        checked_at: Timestamp,
    ) -> Result<RuntimeModelAuthority, OpenRouterChatBuildError> {
        if !self.evidence.validates(self.route.identity(), checked_at) {
            return Err(OpenRouterChatBuildError::StaleEvidence);
        }
        let valid_until = reviewed_until(self.evidence.expires_at(), checked_at)?;
        let grant = self.route.current_credential_generation_grant()?;
        RuntimeModelAuthority::new(
            &self.descriptor,
            self.route.identity().account_binding(),
            self.route.identity().credential_authority_binding(),
            generation_number(grant),
            checked_at,
            Some(valid_until),
        )
        .map_err(|_| OpenRouterChatBuildError::Configuration)
    }
}

/// Bounds an authority by the reviewed Chat Completions pin: once the pin
/// expires the route is refused, as an expired compiled profile is.
pub(crate) fn reviewed_until(
    evidence_expires_at: Timestamp,
    checked_at: Timestamp,
) -> Result<Timestamp, OpenRouterChatBuildError> {
    let review_expires_at = Timestamp::new(OPENROUTER_CHAT_REVIEW_EXPIRES_AT, 0)
        .map_err(|_| OpenRouterChatBuildError::Configuration)?;
    if checked_at >= review_expires_at {
        return Err(OpenRouterChatBuildError::ProfileNotCurrent);
    }
    Ok(evidence_expires_at.min(review_expires_at))
}

fn openrouter_chat_dialect_capabilities(
    evidence: &OpenRouterChatModelEvidence,
) -> Result<CapabilitySet, OpenRouterChatBuildError> {
    let provenance = FactProvenance::new(
        FactSource::BuiltIn,
        Timestamp::new(OPENROUTER_CHAT_REVIEWED_AT, 0)?,
        Some(Timestamp::new(OPENROUTER_CHAT_REVIEW_EXPIRES_AT, 0)?),
    );
    let mut features = BTreeMap::new();
    if let Some(image) = image_input_fact(evidence) {
        features.insert(Capability::ImageInput, image);
    }
    let constraints = CapabilityConstraints::new(
        None,
        Some(
            "reviewed OpenRouter Chat Completions streaming, client function tools, and reasoning grammar; dynamic endpoint facts remain separate prerequisites"
                .to_owned(),
        ),
    );
    features.extend([
        (
            Capability::Streaming,
            FeatureSupport::new(Support::Native, constraints.clone(), provenance),
        ),
        (
            Capability::Tools,
            FeatureSupport::new(Support::Native, constraints.clone(), provenance),
        ),
        (
            Capability::Reasoning,
            FeatureSupport::new(Support::Native, constraints, provenance),
        ),
    ]);
    Ok(CapabilitySet::new(features, BTreeMap::new()))
}

/// The `ImageInput` fact a discovered model row attests, shared by every
/// OpenRouter dialect that lowers inline user images.
pub(crate) fn image_input_fact(evidence: &OpenRouterChatModelEvidence) -> Option<FeatureSupport> {
    evidence.image_input.then(|| {
        FeatureSupport::new(
            Support::Native,
            CapabilityConstraints::new(
                None,
                Some("catalog architecture.input_modalities lists image".to_owned()),
            ),
            FactProvenance::new(
                FactSource::Discovery,
                evidence.observed_at,
                Some(evidence.expires_at),
            ),
        )
    })
}

impl fmt::Debug for OpenRouterChatTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenRouterChatTarget")
            .field("route", &self.route.identity())
            .field("routing", &self.routing)
            .field("model", &self.evidence.model)
            .field("descriptor", &self.descriptor)
            .field("target_identity", &self.target_identity)
            .finish_non_exhaustive()
    }
}

/// Immutable HTTP and decoder bounds for an OpenRouter Chat executable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpenRouterChatHttpConfig {
    /// Per-exchange timeout up to the response head; the streamed body is
    /// bounded by the client's per-pull idle limit only.
    pub operation_timeout: Duration,
    /// Bounded model-event channel capacity.
    pub event_channel_capacity: usize,
    /// Bounded provider error body size.
    pub maximum_error_body_bytes: usize,
    /// Chat SSE and output bounds.
    pub stream_limits: ChatStreamLimits,
}

impl Default for OpenRouterChatHttpConfig {
    fn default() -> Self {
        Self {
            operation_timeout: Duration::from_secs(5 * 60),
            event_channel_capacity: 64,
            maximum_error_body_bytes: 64 * 1024,
            stream_limits: ChatStreamLimits::default(),
        }
    }
}

/// First-class Chat model. Its `LanguageModel` implementation is below the
/// target/configuration construction so all selected authority is immutable.
pub struct OpenRouterChatLanguageModel {
    target: OpenRouterChatTarget,
    client: Client,
    config: OpenRouterChatHttpConfig,
    configuration: CompiledProviderConfiguration,
    clock: Arc<dyn Clock>,
}

impl OpenRouterChatLanguageModel {
    /// Builds the curated GatewayManaged Custom target from complete behavior.
    ///
    /// The provider owns the route identity, so this is the facade-safe path
    /// for consumers that must not reconstruct or inspect provider internals.
    pub fn configured_gateway_managed_behavior(
        route: OpenRouterChatRoute,
        evidence: OpenRouterChatModelEvidence,
        context_contract: ContextContract,
        behavior: ProviderBehaviorConfiguration,
        client: Client,
    ) -> Result<ConfiguredLanguageModel, OpenRouterChatBuildError> {
        Self::configured_gateway_managed_behavior_with_profile(
            route,
            evidence,
            context_contract,
            OpenRouterChatProfile::default(),
            behavior,
            client,
        )
    }

    /// Builds the curated Custom target with explicit Chat behavior.
    pub fn configured_gateway_managed_behavior_with_profile(
        route: OpenRouterChatRoute,
        evidence: OpenRouterChatModelEvidence,
        context_contract: ContextContract,
        profile: OpenRouterChatProfile,
        behavior: ProviderBehaviorConfiguration,
        client: Client,
    ) -> Result<ConfiguredLanguageModel, OpenRouterChatBuildError> {
        let target = OpenRouterChatTarget::gateway_managed_with_profile(
            route,
            evidence,
            context_contract,
            profile,
        )?;
        let descriptor = target.descriptor();
        let selected = SelectedProviderConfiguration::new(
            ProviderRouteSelection::new(
                descriptor.protocol().clone(),
                descriptor.route().clone(),
                descriptor.profile().clone(),
                ProviderProfileRevisionId::new(OPENROUTER_CHAT_INTEGRATION_REVISION)?,
                ModelAdapterProfileId::new(OPENROUTER_CHAT_AUTH_PROFILE)?,
                ProviderProfileRevisionId::new(OPENROUTER_CHAT_AUTH_REVISION)?,
                ProviderAuthorityClassId::new(OPENROUTER_CHAT_AUTHORITY_CLASS)?,
                CredentialSelectionId::new(OPENROUTER_CHAT_CREDENTIAL_SELECTION)?,
                descriptor.requested_model().clone(),
            ),
            behavior,
        )?;
        let model = Arc::new(Self::new(target, selected, client)?);
        let authority = model.target.runtime_authority(model.clock.now())?;
        ConfiguredLanguageModel::custom(model.clone(), model.configuration.clone(), authority)
            .map_err(OpenRouterChatBuildError::from)
    }

    /// Builds the curated GatewayManaged Custom target and seals its configuration.
    pub fn configured_gateway_managed(
        route: OpenRouterChatRoute,
        evidence: OpenRouterChatModelEvidence,
        context_contract: ContextContract,
        selected: SelectedProviderConfiguration,
        client: Client,
    ) -> Result<ConfiguredLanguageModel, OpenRouterChatBuildError> {
        Self::configured_gateway_managed_with_profile(
            route,
            evidence,
            context_contract,
            OpenRouterChatProfile::default(),
            selected,
            client,
        )
    }

    /// Builds the curated Custom target with an explicit provider-owned Chat profile.
    pub fn configured_gateway_managed_with_profile(
        route: OpenRouterChatRoute,
        evidence: OpenRouterChatModelEvidence,
        context_contract: ContextContract,
        profile: OpenRouterChatProfile,
        selected: SelectedProviderConfiguration,
        client: Client,
    ) -> Result<ConfiguredLanguageModel, OpenRouterChatBuildError> {
        let target = OpenRouterChatTarget::gateway_managed_with_profile(
            route,
            evidence,
            context_contract,
            profile,
        )?;
        let model = Arc::new(Self::new(target, selected, client)?);
        let authority = model.target.runtime_authority(model.clock.now())?;
        ConfiguredLanguageModel::custom(model.clone(), model.configuration.clone(), authority)
            .map_err(OpenRouterChatBuildError::from)
    }

    /// Binds one exact dynamic target and selected configuration.
    pub fn new(
        target: OpenRouterChatTarget,
        selected: SelectedProviderConfiguration,
        client: Client,
    ) -> Result<Self, OpenRouterChatBuildError> {
        Self::with_config(
            target,
            selected,
            client,
            OpenRouterChatHttpConfig::default(),
        )
    }

    /// Binds one exact dynamic target with explicit HTTP bounds.
    pub fn with_config(
        target: OpenRouterChatTarget,
        selected: SelectedProviderConfiguration,
        client: Client,
        config: OpenRouterChatHttpConfig,
    ) -> Result<Self, OpenRouterChatBuildError> {
        if config.operation_timeout.is_zero()
            || config.event_channel_capacity == 0
            || config.maximum_error_body_bytes == 0
        {
            return Err(OpenRouterChatBuildError::InvalidPolicy);
        }
        if matches!(
            target.routing(),
            OpenRouterChatRoutingPolicy::GatewayManaged
        ) && !gateway_retention_is_explicit(target.route.identity().retention_policy())
        {
            return Err(OpenRouterChatBuildError::MissingRetentionPolicy);
        }
        let clock = Arc::clone(&target.route.clock);
        let runtime = target.runtime_authority(clock.now())?;
        let configuration = compile_chat_configuration(&target, selected, &runtime)?;
        Ok(Self {
            target,
            client,
            config,
            configuration,
            clock,
        })
    }

    /// Returns the immutable selected/effective configuration binding.
    #[must_use]
    pub const fn configuration(&self) -> &CompiledProviderConfiguration {
        &self.configuration
    }
}

impl fmt::Debug for OpenRouterChatLanguageModel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenRouterChatLanguageModel")
            .field("target", &self.target)
            .field("config", &self.config)
            .field("configuration", &self.configuration)
            .finish_non_exhaustive()
    }
}

impl LanguageModel for OpenRouterChatLanguageModel {
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
                    OpenRouterChatBuildError::StaleEvidence
                    | OpenRouterChatBuildError::ProfileNotCurrent => {
                        RuntimeModelAuthorityError::Stale
                    }
                    OpenRouterChatBuildError::Authentication(_) => {
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
            return Err(preparation_rejection(
                ModelPreparationErrorKind::InvalidConfiguration,
                "OpenRouter Chat request configuration is foreign",
            ));
        }
        descriptor.validate_plan_admission(plan.request(), &plan)?;
        context
            .authority()
            .validate(descriptor, context.prepared_at())?;
        if context.active_family_artifact().is_some() {
            return Err(preparation_rejection(
                ModelPreparationErrorKind::InvalidRequest,
                "stateless Chat has no aggregate family artifact",
            ));
        }
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
                preparation_rejection(
                    ModelPreparationErrorKind::InvalidConfiguration,
                    "OpenRouter Chat requires an enforced output bound",
                )
            })?;
        let router = match self.target.routing() {
            OpenRouterChatRoutingPolicy::StrictPinned(_) => {
                return Err(preparation_rejection(
                    ModelPreparationErrorKind::InvalidConfiguration,
                    "OpenRouter Chat strict pinning is unavailable",
                ));
            }
            OpenRouterChatRoutingPolicy::GatewayManaged => {
                let retention = identity.retention_policy();
                Some(
                    ChatRouterPolicy::gateway_managed(
                        retention
                            .and_then(OpenRouterRetentionPolicy::data_collection)
                            .map(str::to_owned),
                        retention.and_then(OpenRouterRetentionPolicy::zdr),
                    )
                    .map_err(|_| {
                        preparation_rejection(
                            ModelPreparationErrorKind::InvalidConfiguration,
                            "OpenRouter Chat gateway routing is invalid",
                        )
                    })?,
                )
            }
        };
        let reasoning_policy = plan.request().reasoning();
        let disabled_reasoning_wire =
            openrouter_disabled_reasoning_wire(self.target.evidence(), reasoning_policy);
        let policy = ChatRequestPolicy::new(
            descriptor.protocol().clone(),
            descriptor.requested_model().clone(),
            plan.binding_digest(),
            identity.account_binding(),
            self.configuration.selected_identity(),
            self.configuration.effective_identity(),
            output_bound.get(),
            ChatOutputLimitField::MaxTokens,
            self.target.reasoning_replay,
            router,
            reasoning_policy,
            self.target.reasoning_alias,
        )
        .map_err(|_| {
            preparation_rejection(
                ModelPreparationErrorKind::InvalidConfiguration,
                "OpenRouter Chat lowering policy is invalid",
            )
        })?
        .with_disabled_reasoning_wire(disabled_reasoning_wire)
        .with_tool_choice_none_omits_tools(true)
        .with_tool_choice_auto_uses_default(true)
        .with_plugins(plakous_chat_completions::OPENROUTER_DISABLE_CONTEXT_COMPRESSION_PLUGINS);
        let policy = if let Some(affinity) = context
            .cache_affinity()
            .map(plakous_model::ModelCacheAffinity::identity)
        {
            policy.with_gateway_session_key(affinity.to_string())
        } else {
            policy
        };
        let replay_target = policy.replay_target();
        let stream_reasoning = policy.reasoning();
        let stream_reasoning_replay = policy.reasoning_replay();
        let stream_reasoning_alias = policy.reasoning_alias();
        let lowered = lower_request(&plan, &policy).map_err(|error| {
            let kind = match error.kind() {
                plakous_chat_completions::ChatRequestErrorKind::UnsupportedSemantic => {
                    ModelPreparationErrorKind::UnsupportedCapability
                }
                plakous_chat_completions::ChatRequestErrorKind::InvalidRequest => {
                    ModelPreparationErrorKind::InvalidRequest
                }
                plakous_chat_completions::ChatRequestErrorKind::ProtectedHistory => {
                    ModelPreparationErrorKind::ProtectedHistory
                }
            };
            preparation_rejection(
                kind,
                format!(
                    "OpenRouter Chat request lowering failed: {}",
                    error.message()
                ),
            )
        })?;
        let wire_digest = Digest::sha256(
            DigestDomain::Blob,
            1,
            &[b"plakous-openrouter/chat-wire-v1", lowered.body()],
        )
        .map_err(|_| {
            preparation_rejection(
                ModelPreparationErrorKind::InvalidConfiguration,
                "OpenRouter Chat wire identity failed",
            )
        })?;
        let wire_size = u64::try_from(lowered.body().len()).map_err(|_| {
            preparation_rejection(
                ModelPreparationErrorKind::ResourceLimit,
                "OpenRouter Chat request is too large",
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
                preparation_rejection(
                    ModelPreparationErrorKind::InvalidConfiguration,
                    "OpenRouter Chat context observation is invalid",
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
            OPENROUTER_CHAT_LOWERING_REVISION,
        )?;
        let projection = PreparedModelProjection::new(
            descriptor,
            &plan,
            &context,
            ProtocolId::new(CHAT_COMPLETIONS_FAMILY_ID).map_err(|_| {
                preparation_rejection(
                    ModelPreparationErrorKind::InvalidConfiguration,
                    "Chat family identity is invalid",
                )
            })?,
            OPENROUTER_CHAT_LOWERING_REVISION,
            OPENROUTER_CHAT_PROJECTION_REVISION,
            OPENROUTER_CHAT_ACCOUNTING_REVISION,
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
        let stream_binding = PreparedChatStreamBinding::new(
            plan.digest(),
            descriptor.protocol().clone(),
            descriptor.requested_model().clone(),
            replay_target,
            stream_reasoning,
            stream_reasoning_replay,
            stream_reasoning_alias,
            self.config.stream_limits,
        )
        .with_extensions(ChatStreamExtensions::OpenRouterV1);
        let operation_binding = ChatOperationBinding::new(
            self.target.target_identity(),
            plan.digest(),
            lowered.operation_digest(),
            lowered.replay_safety(),
        );
        let operation =
            PreparedChatHttpOperation::new(operation_binding.clone(), lowered, stream_binding);
        let hooks = OpenRouterChatHooks {
            target: self.target.clone(),
            clock: Arc::clone(&self.clock),
            generation: current_generation(&self.target).map_err(|_| {
                preparation_rejection(
                    ModelPreparationErrorKind::InvalidConfiguration,
                    "OpenRouter Chat credential generation is unavailable",
                )
            })?,
        };
        let client = self.client.clone();
        let origin = Origin::parse(identity.origin().as_str()).map_err(|_| {
            preparation_rejection(
                ModelPreparationErrorKind::InvalidConfiguration,
                "OpenRouter Chat origin is invalid",
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
                    ChatAttempt::with_event_channel(operation_binding, call_context, capacity);
                let mut service = ChatHttpAttemptService::new(
                    client,
                    origin,
                    operation,
                    hooks,
                    Arc::clone(&clock),
                    Arc::new(SystemIdGenerator),
                    config.operation_timeout,
                    config.maximum_error_body_bytes,
                );
                if turn_rooted {
                    service = service.with_turn_retry();
                }
                let operation =
                    Box::pin(async move { call_chat_operation(&mut service, attempt).await });
                Ok(Box::pin(JoinedChatStream {
                    descriptor,
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
struct OpenRouterChatHooks {
    target: OpenRouterChatTarget,
    clock: Arc<dyn Clock>,
    generation: OpenRouterApiKeyGenerationGrant,
}

#[derive(Clone)]
struct ErrorMetadata {
    status: u16,
}

impl ChatHttpHooks for OpenRouterChatHooks {
    type ErrorMetadata = ErrorMetadata;

    fn build_request(
        &self,
        client: &Client,
        dial_origin: Origin,
        physical: &ChatPhysicalRequestBinding,
        operation: &PreparedChatHttpOperation,
    ) -> Result<Request, ChatAttemptProtocolError> {
        let current = self
            .target
            .runtime_authority(self.clock.now())
            .map_err(|_| {
                ChatAttemptProtocolError::new("OpenRouter Chat dispatch authority unavailable")
            })?;
        if current.credential_generation() != generation_number(self.generation) {
            return Err(ChatAttemptProtocolError::new(
                "OpenRouter Chat dispatch credential changed",
            ));
        }
        let key = self
            .target
            .route
            .chat_credential(physical, self.generation)
            .map_err(|_| ChatAttemptProtocolError::new("OpenRouter Chat authentication failed"))?;
        let authorization = sensitive_header_value(key.bearer_bytes())
            .map_err(|_| ChatAttemptProtocolError::new("OpenRouter Chat authorization failed"))?;
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
        headers.insert(
            HeaderName::from_static("x-openrouter-metadata"),
            HeaderValue::from_static("enabled"),
        );
        insert_attribution(self.target.route.identity(), &mut headers);
        Request::new(
            dial_origin,
            Method::POST,
            OPENROUTER_CHAT_COMPLETIONS_PATH,
            headers,
            SensitiveBytes::new(operation.body().to_vec()).into_bytes(),
            client.limits(),
        )
        .map_err(|_| ChatAttemptProtocolError::new("OpenRouter Chat request binding failed"))
    }

    fn classify_response_head(
        &self,
        response: &Response,
    ) -> Result<ChatHttpResponseHead<Self::ErrorMetadata>, ChatAttemptProtocolError> {
        let status = response.status();
        if status != StatusCode::OK {
            return Ok(ChatHttpResponseHead::Error(ChatHttpErrorHead {
                metadata: ErrorMetadata {
                    status: status.as_u16(),
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
            return Err(ChatAttemptProtocolError::new(
                "OpenRouter Chat success was not an SSE response",
            ));
        }
        let request_id = openrouter_request_id(|name| response.headers().get(name))
            .ok_or_else(|| ChatAttemptProtocolError::new("OpenRouter Chat request ID is absent"))?;
        let request_id = plakous_model::ModelProviderRequestId::new(request_id)
            .map_err(|_| ChatAttemptProtocolError::new("OpenRouter Chat request ID is invalid"))?;
        Ok(ChatHttpResponseHead::Stream(Some(request_id)))
    }

    fn classify_error_response(
        &self,
        metadata: Self::ErrorMetadata,
        body: &[u8],
        safety: ChatHttpSafety,
    ) -> ChatAttemptProtocolError {
        let disposition =
            if matches!(metadata.status, 401 | 403 | 429 | 503 | 529) && !safety.visible_output {
                "rejected-before-acceptance"
            } else if safety.visible_output {
                "post-acceptance"
            } else if safety.provider_accepted {
                "acceptance-unknown"
            } else {
                "rejected"
            };
        let error = ChatAttemptProtocolError::new(match openrouter_error_detail(body) {
            Some(detail) => format!(
                "OpenRouter Chat HTTP {} ({disposition}): {detail}",
                metadata.status
            ),
            None => format!("OpenRouter Chat HTTP {} ({disposition})", metadata.status),
        })
        .with_http_status(metadata.status);
        let error = if body_reports_context_overflow(body) {
            error.with_context_overflow()
        } else {
            error
        };
        // The status and the disposition above are diagnostic text. Carry the
        // classification itself forward so nothing upstream has to parse a
        // message to learn what the provider said.
        match plakous_attempt::ProviderRefusal::from_http_status(metadata.status) {
            Some(refusal) => error.with_refusal(refusal),
            None => error,
        }
    }

    fn classify_transport_failure(
        &self,
        failure: HttpFailure,
        _request_id: Option<&str>,
    ) -> ChatAttemptProtocolError {
        let cause = plakous_exchange::network_failure(failure).map_or_else(
            || "network exchange failed".to_owned(),
            |cause| cause.to_string(),
        );
        let error =
            ChatAttemptProtocolError::new(format!("OpenRouter Chat transport failed: {cause}"))
                .with_detail(failure.to_string());
        match transport_refusal(failure) {
            Some(refusal) => error.with_refusal(refusal),
            None => error,
        }
    }

    fn cancellation_error(&self) -> ChatAttemptProtocolError {
        ChatAttemptProtocolError::new("OpenRouter Chat operation cancelled")
    }

    fn classify_stream_error(&self, error: ChatStreamError) -> ChatAttemptProtocolError {
        let protocol = ChatAttemptProtocolError::new(format!(
            "OpenRouter Chat stream validation failed: {error}"
        ));
        // OpenRouter commits HTTP 200 at acceptance and reports an upstream
        // failure inside the stream, carrying the status it would have sent had
        // it failed earlier. That is the provider naming its own refusal, and
        // it reads the same whether it arrived before the stream or inside it.
        let Some(status) = error.http_status() else {
            return protocol;
        };
        let protocol = protocol.with_http_status(status);
        match plakous_attempt::ProviderRefusal::from_http_status(status) {
            Some(refusal) => protocol.with_refusal(refusal),
            None => protocol,
        }
    }
}

/// The refusal a physical transport failure names, when it names one.
///
/// A deadline is the one transport outcome the client itself decided: it stopped
/// waiting. That is a definite end to this attempt, and saying so is what keeps
/// a post-acceptance timeout from escalating as an unknown outcome. Every other
/// transport failure leaves the question open and is deliberately unnamed here.
/// The route's truthful attribution headers, when it has attribution.
pub(crate) fn insert_attribution(
    identity: &crate::OpenRouterChatRouteIdentity,
    headers: &mut HeaderMap,
) {
    if let Some(attribution) = identity.attribution() {
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
}

fn transport_refusal(failure: HttpFailure) -> Option<plakous_attempt::ProviderRefusal> {
    matches!(failure.kind(), plakous_http::ErrorKind::DeadlineExceeded)
        .then_some(plakous_attempt::ProviderRefusal::Deadline)
}

fn openrouter_request_id<'a>(
    mut get: impl FnMut(&HeaderName) -> Option<&'a HeaderValue>,
) -> Option<&'a str> {
    ["x-generation-id", "request-id", "x-request-id"]
        .into_iter()
        .find_map(|name| get(&HeaderName::from_static(name)).and_then(|value| value.to_str().ok()))
}

struct JoinedChatStream {
    descriptor: ResolvedModelDescriptor,
    operation: Pin<
        Box<
            dyn Future<
                    Output = Result<
                        plakous_chat_completions::ChatAttemptResponse,
                        ChatAttemptError,
                    >,
                > + Send,
        >,
    >,
    receiver: plakous_chat_completions::ChatAttemptEventReceiver,
    operation_done: bool,
    finished: bool,
    terminal_error: Option<ModelError>,
    observation_error: Option<ChatAttemptProtocolError>,
}

impl Stream for JoinedChatStream {
    type Item = Result<ModelEvent, ModelError>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.finished {
            return Poll::Ready(None);
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
                        .map(|error| chat_model_error(&self.descriptor, &error));
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
            self.observation_error
                .take()
                .map(|error| chat_observation_error(&self.descriptor, &error))
        });
        Poll::Ready(error.map(Err))
    }
}

fn chat_observation_error(
    descriptor: &ResolvedModelDescriptor,
    error: &ChatAttemptProtocolError,
) -> ModelError {
    let facts = error.terminal_facts();
    let kind = usage_limit_kind(error.usage_limit(), facts)
        .or_else(|| context_overflow_kind(error.context_overflow(), facts))
        .or_else(|| named_refusal_kind(error.refusal(), facts))
        .unwrap_or_else(|| inferred_failure_kind(facts, error.exchange_break()));
    operation_error(
        descriptor,
        kind,
        facts,
        format!("OpenRouter Chat protocol failed: {error}"),
    )
}

/// Classifies a failure the provider did not name.
///
/// Only a possible hosted effect leaves the outcome unknown. A break after
/// acceptance without one is a definite failure of its own kind: the partial
/// output is discarded with the turn and nothing replays it.
pub(crate) fn inferred_failure_kind(
    facts: Option<TerminalFacts>,
    exchange_break: ExchangeBreak,
) -> ModelErrorKind {
    if facts.map(TerminalFacts::failure_risk) == Some(FailureRisk::HostedEffect) {
        return ModelErrorKind::OutcomeUnknown;
    }
    match exchange_break {
        ExchangeBreak::Transport => ModelErrorKind::Transport,
        ExchangeBreak::Protocol => ModelErrorKind::Protocol,
    }
}

/// Builds a gateway operation error whose retryability states the risk its
/// facts leave behind, so no caller mistakes a post-acceptance failure for a
/// replayable one.
pub(crate) fn operation_error(
    descriptor: &ResolvedModelDescriptor,
    kind: ModelErrorKind,
    facts: Option<TerminalFacts>,
    message: String,
) -> ModelError {
    let retryability = match (kind, facts.map(TerminalFacts::failure_risk)) {
        (ModelErrorKind::Cancelled, _) | (_, None | Some(FailureRisk::None)) => {
            ModelRetryability::Permanent
        }
        (_, Some(FailureRisk::DuplicateInferenceOrBilling)) => ModelRetryability::Unsafe {
            risk: ModelRetryRisk::DuplicateInferenceOrBilling,
        },
        (_, Some(FailureRisk::HostedEffect)) => ModelRetryability::Unsafe {
            risk: ModelRetryRisk::ProviderHostedEffect,
        },
    };
    ModelErrorContext::new(
        descriptor.protocol().clone(),
        descriptor.route().clone(),
        descriptor.requested_model().clone(),
        ModelErrorPhase::Terminal,
        retryability,
    )
    .and_then(|context| ModelError::contextual(kind, message.clone(), context))
    .unwrap_or_else(|_| ModelError::new(kind, message))
}

/// Reads a refusal the provider named as the definite outcome it is.
///
/// No hosted effect ran and the provider said why it did no work, so there is
/// nothing to reconcile and nothing to recover: no response was committed and
/// the canonical history is untouched. Acceptance does not change that, and on
/// this route it especially does not — `OpenRouter` commits HTTP 200 at
/// acceptance and then reports upstream failures inside the stream, so every
/// in-band refusal is post-acceptance by construction.
///
/// Visible output does not change it either. The live shape had a chunk stream
/// before the `520` arrived; that text is discarded with the turn and creates
/// no state anything could fail to reconcile. Only a hosted effect does.
///
/// `None` means this failure names no refusal, and the caller's own reading
/// stands.
fn named_refusal_kind(
    refusal: Option<plakous_attempt::ProviderRefusal>,
    facts: Option<TerminalFacts>,
) -> Option<ModelErrorKind> {
    if facts.is_some_and(TerminalFacts::hosted_effects_possible) {
        return None;
    }
    match refusal? {
        plakous_attempt::ProviderRefusal::RateLimit => Some(ModelErrorKind::RateLimited),
        plakous_attempt::ProviderRefusal::Overloaded => Some(ModelErrorKind::Unavailable),
        plakous_attempt::ProviderRefusal::Deadline => Some(ModelErrorKind::Timeout),
    }
}

/// Exhausted entitlement is terminal unless hosted effects make the outcome unknown.
pub(crate) fn usage_limit_kind(
    usage_limit: bool,
    facts: Option<TerminalFacts>,
) -> Option<ModelErrorKind> {
    (usage_limit && !facts.is_some_and(TerminalFacts::hosted_effects_possible))
        .then_some(ModelErrorKind::QuotaExceeded)
}

/// A rejection of the input as over the model's window is its own kind: no
/// inference ran, and the runtime answers it by compacting and sending a new,
/// smaller request rather than by retrying this one.
pub(crate) fn context_overflow_kind(
    context_overflow: bool,
    facts: Option<TerminalFacts>,
) -> Option<ModelErrorKind> {
    (context_overflow && !facts.is_some_and(TerminalFacts::hosted_effects_possible))
        .then_some(ModelErrorKind::ContextLimit)
}

/// Whether an OpenRouter error body names a context-window overflow, in its
/// own message, the upstream's raw message, or its code.
pub(crate) fn body_reports_context_overflow(body: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    let code = value
        .pointer("/error/code")
        .and_then(serde_json::Value::as_str);
    ["/error/message", "/error/metadata/raw", "/message"]
        .into_iter()
        .filter_map(|pointer| value.pointer(pointer).and_then(serde_json::Value::as_str))
        .chain(std::iter::once(""))
        .any(|message| plakous_model::provider_rejection_is_context_overflow(code, message))
}

fn is_sse_content_type(value: &str) -> bool {
    value
        .split_once(';')
        .map_or(value, |(media_type, _)| media_type)
        .trim()
        .eq_ignore_ascii_case("text/event-stream")
}

/// Classifies a non-200 head on every OpenRouter wire.
pub(crate) fn post_send_status_outcome(status: StatusCode) -> PhysicalRequestOutcome {
    // OpenRouter commits HTTP 200 at provider acceptance. Later policy or
    // rate-limit errors are carried inside that stream, not initial 403/429,
    // and an initial 401 is the gateway refusing the credential itself before
    // acceptance. A 503 (no provider available) or 529 is a refusal before
    // any upstream took the request.
    if matches!(status.as_u16(), 401 | 403 | 429 | 503 | 529) {
        PhysicalRequestOutcome::DefiniteRejection
    } else {
        PhysicalRequestOutcome::AmbiguousPostSend
    }
}

fn gateway_retention_is_explicit(retention: Option<OpenRouterRetentionPolicy>) -> bool {
    retention.is_some()
}

fn chat_model_error(descriptor: &ResolvedModelDescriptor, error: &ChatAttemptError) -> ModelError {
    let facts = error.facts();
    let named = usage_limit_kind(error.usage_limit(), facts)
        .or_else(|| context_overflow_kind(error.context_overflow(), facts))
        .or_else(|| named_refusal_kind(error.provider_refusal(), facts));
    let kind = match error {
        ChatAttemptError::Cancelled { .. } => ModelErrorKind::Cancelled,
        ChatAttemptError::Unavailable => ModelErrorKind::Unavailable,
        _ if error.http_status() == Some(401)
            && facts.is_some_and(|facts| facts.failure_risk() == FailureRisk::None) =>
        {
            ModelErrorKind::Authentication
        }
        _ => named.unwrap_or_else(|| inferred_failure_kind(facts, error.exchange_break())),
    };
    let model_error = operation_error(
        descriptor,
        kind,
        facts,
        format!("OpenRouter Chat operation failed: {error}"),
    )
    .with_attempt_summary(facts.map_or_else(String::new, |facts| facts.summary().to_string()));
    match error {
        ChatAttemptError::Protocol { source, .. } => source.detail().map_or_else(
            || model_error.clone(),
            |detail| model_error.clone().with_source(detail),
        ),
        _ => model_error,
    }
}

fn current_generation(
    target: &OpenRouterChatTarget,
) -> Result<OpenRouterApiKeyGenerationGrant, crate::OpenRouterAuthError> {
    target.route.current_credential_generation_grant()
}

fn preparation_rejection(
    kind: ModelPreparationErrorKind,
    message: impl AsRef<str>,
) -> ModelPreparationError {
    ModelPreparationError::rejected(kind, message)
}

fn openrouter_disabled_reasoning_wire(
    evidence: &OpenRouterChatModelEvidence,
    reasoning: plakous_model::ReasoningPolicy,
) -> ChatDisabledReasoningWire {
    if matches!(reasoning, plakous_model::ReasoningPolicy::Disabled)
        && evidence
            .reasoning()
            .is_some_and(|facts| facts.default_enabled() && !facts.mandatory())
    {
        ChatDisabledReasoningWire::EnabledFalse
    } else {
        ChatDisabledReasoningWire::Omit
    }
}

fn compile_chat_configuration(
    target: &OpenRouterChatTarget,
    selected: SelectedProviderConfiguration,
    runtime: &RuntimeModelAuthority,
) -> Result<CompiledProviderConfiguration, OpenRouterChatBuildError> {
    if !matches!(
        selected.behavior().tool_choice,
        plakous_model::ToolChoice::Auto | plakous_model::ToolChoice::None
    ) {
        return Err(OpenRouterChatBuildError::Configuration);
    }
    let descriptor = target.descriptor();
    let implementation = ModelImplementationIdentity::custom(
        NamespaceId::new(OPENROUTER_CHAT_IMPLEMENTATION)?,
        1,
        NamespaceId::new(OPENROUTER_CHAT_PROVENANCE)?,
    )?;
    let semantic_bytes = serde_json::to_vec(&(
        "plakous-openrouter/chat-semantic-v1",
        OPENROUTER_CHAT_COMPLETIONS_PATH,
        OPENROUTER_CHAT_PROTOCOL_ID,
        OPENROUTER_CHAT_DIALECT_REVISION,
        target.model(),
        target.routing(),
        target
            .evidence
            .admission_identity(crate::discovery::AdmissionWire::Chat)?,
        reasoning_replay_label(target.reasoning_replay),
        reasoning_alias_label(target.reasoning_alias),
        target
            .route
            .identity()
            .retention_policy()
            .map(OpenRouterRetentionPolicy::data_collection),
        target
            .route
            .identity()
            .retention_policy()
            .and_then(OpenRouterRetentionPolicy::zdr),
        false,
    ))
    .map_err(|_| OpenRouterChatBuildError::Identity)?;
    let authority = ResolvedProviderAuthorityBinding::new(
        descriptor,
        runtime,
        ProviderSemanticEvidence::new(1, semantic_bytes)?,
    )?;
    let family = descriptor
        .model_family()
        .cloned()
        .ok_or(OpenRouterChatBuildError::Configuration)?;
    let policy_bytes = serde_json::to_vec(&(
        target.routing(),
        reasoning_replay_label(target.reasoning_replay),
        reasoning_alias_label(target.reasoning_alias),
        "aggregate-artifact=none",
        "cross-protocol-fallback=false",
    ))
    .map_err(|_| OpenRouterChatBuildError::Identity)?;
    let family_policy =
        FamilyPolicyEnvelope::new(family, 1, 1, OPENROUTER_CHAT_DIALECT_REVISION, policy_bytes)?;
    let effective = EffectiveProviderConfiguration::compile(
        &selected,
        descriptor.clone(),
        implementation,
        authority,
        selected.behavior().clone(),
        family_policy,
    )?;
    CompiledProviderConfiguration::new(selected, effective).map_err(OpenRouterChatBuildError::from)
}

const fn reasoning_replay_label(policy: ChatReasoningReplay) -> &'static str {
    match policy {
        ChatReasoningReplay::VisibleOnly => "reasoning-replay=visible-only",
        ChatReasoningReplay::ExactDetails => "reasoning-replay=exact-details",
    }
}

const fn reasoning_alias_label(policy: ReasoningAliasPolicy) -> &'static str {
    match policy {
        ReasoningAliasPolicy::Reasoning => "reasoning-alias=reasoning",
        ReasoningAliasPolicy::ReasoningContent => "reasoning-alias=reasoning-content",
        ReasoningAliasPolicy::EitherBindFirst => "reasoning-alias=either-bind-first",
    }
}

/// Provider-facing OpenRouter error text only. Drops secrets and oversized bodies.
fn openrouter_error_detail(body: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let message = value
        .pointer("/error/message")
        .or_else(|| value.get("message"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|message| !message.is_empty());
    let raw = value
        .pointer("/error/metadata/raw")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|raw| !raw.is_empty());
    let combined = match (message, raw) {
        (Some(message), Some(raw)) if message.eq_ignore_ascii_case("provider returned error") => {
            format!("{message}: {raw}")
        }
        (Some(message), Some(raw)) => format!("{message}: {raw}"),
        (Some(message), None) => message.to_owned(),
        (None, Some(raw)) => raw.to_owned(),
        (None, None) => return None,
    };
    let lower = combined.to_ascii_lowercase();
    if lower.contains("bearer")
        || lower.contains("api-key")
        || lower.contains("api_key")
        || lower.contains("authorization")
        || lower.contains("sk-")
    {
        return None;
    }
    Some(combined.chars().take(240).collect())
}

fn concrete_model_id(value: &str) -> bool {
    let Some((author, model)) = value.split_once('/') else {
        return false;
    };
    if author.is_empty()
        || model.is_empty()
        || model.contains('/')
        || value.len() > 256
        || value.bytes().any(|byte| {
            !(byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'-' | b'_' | b'.' | b'/' | b':'))
        })
        || value.matches(':').count() > 1
    {
        return false;
    }
    true
}

fn generation_number(grant: OpenRouterApiKeyGenerationGrant) -> u64 {
    let fingerprint = grant.fingerprint();
    let bytes = fingerprint.as_bytes();
    u64::from_be_bytes(bytes[..8].try_into().unwrap_or([0; 8])).max(1)
}

pub(crate) fn context_within_discovery(contract: &ContextContract, discovered: u64) -> bool {
    match contract.capacity().rule() {
        CapacityRule::ProviderReservedInput {
            reported_context_window,
            maximum_projected_input,
            ..
        } => {
            reported_context_window.is_none_or(|reported| reported <= discovered)
                && maximum_projected_input <= discovered
        }
        CapacityRule::SharedWindow { total } => total.lower_bound() <= discovered,
        CapacityRule::Separate { input, output } => input
            .lower_bound()
            .checked_add(output.lower_bound())
            .is_some_and(|total| total <= discovered),
        CapacityRule::SharedWindowWithOutputCap { total, output } => {
            total.lower_bound() <= discovered && output.lower_bound() <= discovered
        }
        _ => false,
    }
}

/// Safe construction failure for a dynamic Chat target/model.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum OpenRouterChatBuildError {
    /// Model discovery identity, freshness, or literal model was invalid.
    #[error("OpenRouter Chat discovery evidence is invalid")]
    InvalidDiscoveryEvidence,
    /// An eligible endpoint does not prove the requested output reservation.
    #[error("OpenRouter endpoint discovery does not prove the configured completion bound")]
    UnprovenCompletionBound,
    /// An eligible endpoint does not prove a positive input guarantee.
    #[error("OpenRouter endpoint discovery does not prove a positive prompt/context bound")]
    UnprovenPromptBound,
    /// No eligible endpoint proves a bounded request.
    #[error("OpenRouter endpoint discovery does not admit a bounded selected-wire request")]
    UnboundedAdmission,
    /// Discovery evidence expired.
    #[error("OpenRouter Chat discovery evidence is stale")]
    StaleEvidence,
    /// The reviewed Chat Completions pin expired.
    #[error("OpenRouter Chat compatibility profile is not currently admissible")]
    ProfileNotCurrent,
    /// The descriptor did not carry CustomEnvelope context authority.
    #[error("OpenRouter Chat requires CustomEnvelope context authority")]
    NonCustomContextAuthority,
    /// Caller Custom context exceeds or contradicts authenticated discovery.
    #[error("OpenRouter Chat Custom context exceeds authenticated discovery")]
    ContextExceedsDiscovery,
    /// Strict pin and route identity disagreed.
    #[error("OpenRouter Chat strict routing does not match the configured route")]
    RoutingMismatch,
    /// Exact provider enforcement and terminal endpoint proof are not enrolled.
    #[error("OpenRouter Chat strict pinning is unavailable")]
    StrictPinnedUnavailable,
    /// Gateway-managed dispatch lacked an explicit retention/ZDR policy.
    #[error("OpenRouter Chat gateway routing requires an explicit retention policy")]
    MissingRetentionPolicy,
    /// An HTTP bound was zero.
    #[error("OpenRouter Chat HTTP bounds must be nonzero")]
    InvalidPolicy,
    /// Selected/effective configuration was inconsistent.
    #[error("OpenRouter Chat provider configuration is inconsistent")]
    Configuration,
    /// Identity serialization failed.
    #[error("OpenRouter Chat identity construction failed")]
    Identity,
    /// A neutral identity was invalid.
    #[error(transparent)]
    NeutralIdentity(#[from] plakous_model::IdentityError),
    /// A neutral configuration was invalid.
    #[error(transparent)]
    ProviderConfiguration(#[from] plakous_model::ProviderConfigurationError),
    /// Provider-owned Custom context construction failed.
    #[error(transparent)]
    ContextContract(#[from] ContextContractError),
    /// Credential authority was unavailable.
    #[error(transparent)]
    Authentication(#[from] crate::OpenRouterAuthError),
    /// Stable runtime authority-class construction failed.
    #[error(transparent)]
    RuntimeAuthority(#[from] RuntimeModelAuthorityError),
    /// Digest construction failed.
    #[error(transparent)]
    Digest(#[from] plakous_model::DigestError),
}

/// A descriptor for classifier tests; classification never reads it.
#[allow(clippy::expect_used, reason = "fixed literal identities")]
#[cfg(test)]
pub(crate) fn test_descriptor() -> ResolvedModelDescriptor {
    ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        ProtocolId::new("openrouter.chat").expect("protocol"),
        plakous_model::RouteId::new("openrouter.chat").expect("route"),
        ModelId::new("vendor/model").expect("model"),
        None,
        None,
        ModelAdapterProfileId::new("openrouter.chat.test").expect("profile"),
        CapabilitySet::new(BTreeMap::new(), BTreeMap::new()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;

    use crate::discovery::{AdmissionWire, catalog_snapshot_identity};

    fn admission_fixture() -> Result<OpenRouterChatModelEvidence, Box<dyn std::error::Error>> {
        let route = OpenRouterChatRoute::new_local_fixture_with_retention(
            Arc::new(crate::StaticOpenRouterApiKeySource::new(
                crate::OpenRouterApiKey::new("offline-admission-key")?,
                "offline-account",
                "offline-authority",
            )?),
            crate::OpenRouterOrigin::new("https://127.0.0.1:18443")?,
            OpenRouterRetentionPolicy::strict(),
        )?
        .with_clock(Arc::new(plakous_model::FixedClock::new(Timestamp::new(
            OPENROUTER_CHAT_REVIEWED_AT + 60,
            0,
        )?)));
        let now =
            plakous_model::FixedClock::new(Timestamp::new(OPENROUTER_CHAT_REVIEWED_AT + 60, 0)?)
                .now();
        Ok(OpenRouterChatModelEvidence::from_authenticated_discovery(
            ModelId::new("vendor/model")?,
            route.identity(),
            vec![OpenRouterChatEndpointCandidate {
                endpoint: "provider/first".to_owned(),
                provider_name: "First".to_owned(),
                supported_parameters: BTreeSet::from(
                    ["tools", "max_tokens", "tool_choice", "reasoning", "stop"].map(str::to_owned),
                ),
                context_length: Some(131_072),
                max_prompt_tokens: Some(100_000),
                max_completion_tokens: Some(65_536),
                tool_choice: OpenRouterToolChoiceModes {
                    auto: true,
                    ..OpenRouterToolChoiceModes::default()
                },
                supports_implicit_caching: false,
            }],
            BTreeSet::from(
                ["tools", "max_tokens", "tool_choice", "reasoning", "stop"].map(str::to_owned),
            ),
            Some(OpenRouterReasoningFacts {
                supported_efforts: Some(vec!["medium".to_owned()]),
                default_effort: Some("medium".to_owned()),
                default_enabled: true,
                supports_max_tokens: false,
                mandatory: false,
            }),
            131_072,
            false,
            now,
            Timestamp::new(now.seconds() + 3600, now.nanoseconds())?,
        )?)
    }

    #[test]
    fn admission_projection_ignores_endpoint_names_order_and_irrelevant_parameters()
    -> Result<(), Box<dyn std::error::Error>> {
        let base = admission_fixture()?;
        for wire in [
            AdmissionWire::Chat,
            AdmissionWire::Responses,
            AdmissionWire::Messages,
        ] {
            let identity = base.admission_identity(wire)?;
            let mut added = base.clone();
            let mut endpoint = added.endpoints()[0].clone();
            endpoint.endpoint = "provider/second".to_owned();
            endpoint.provider_name = "Second".to_owned();
            endpoint
                .supported_parameters
                .insert("future_parameter".to_owned());
            added.endpoints.push(endpoint);
            added
                .model_supported_parameters
                .insert("future_parameter".to_owned());
            assert_eq!(identity, added.admission_identity(wire)?);
            added.endpoints.reverse();
            assert_eq!(identity, added.admission_identity(wire)?);
            added.endpoints.remove(1);
            assert_eq!(identity, added.admission_identity(wire)?);
            assert_ne!(
                catalog_snapshot_identity(
                    &base.route_fingerprint,
                    base.model().as_str(),
                    base.context_length(),
                    base.supported_parameters(),
                    base.reasoning(),
                    base.image_input(),
                    base.endpoints()
                )?,
                catalog_snapshot_identity(
                    &base.route_fingerprint,
                    added.model().as_str(),
                    added.context_length(),
                    added.supported_parameters(),
                    added.reasoning(),
                    added.image_input(),
                    added.endpoints()
                )?
            );
        }
        Ok(())
    }

    #[test]
    fn admission_projection_rotates_on_each_model_and_endpoint_fact()
    -> Result<(), Box<dyn std::error::Error>> {
        let base = admission_fixture()?;
        for wire in [
            AdmissionWire::Chat,
            AdmissionWire::Responses,
            AdmissionWire::Messages,
        ] {
            let identity = base.admission_identity(wire)?;
            let mut changed = Vec::new();
            let mut value = base.clone();
            value.context_length = 131_070;
            changed.push(("model context", value));
            let mut value = base.clone();
            value.model = ModelId::new("vendor/other")?;
            changed.push(("model id", value));
            let mut value = base.clone();
            value.model_supported_parameters.remove("max_tokens");
            changed.push(("model required parameter", value));
            let mut value = base.clone();
            value.model_supported_parameters.remove("stop");
            changed.push(("model wire parameter", value));
            for fact in [
                "efforts",
                "default_effort",
                "default_enabled",
                "supports_max_tokens",
                "mandatory",
            ] {
                let mut value = base.clone();
                let reasoning = value.reasoning.as_mut().ok_or("reasoning")?;
                match fact {
                    "efforts" => {
                        reasoning.supported_efforts =
                            Some(vec!["low".to_owned(), "medium".to_owned()]);
                    }
                    "default_effort" => reasoning.default_effort = None,
                    "default_enabled" => reasoning.default_enabled = false,
                    "supports_max_tokens" => reasoning.supports_max_tokens = true,
                    _ => reasoning.mandatory = true,
                }
                changed.push((fact, value));
            }
            for fact in [
                "prompt",
                "completion",
                "context",
                "tools",
                "max_tokens",
                "reasoning",
                "stop",
                "caching",
                "output rung",
                "safe input",
            ] {
                let mut value = base.clone();
                let endpoint = &mut value.endpoints[0];
                match fact {
                    "prompt" => endpoint.max_prompt_tokens = Some(99_000),
                    "completion" => endpoint.max_completion_tokens = Some(40_000),
                    "context" => endpoint.context_length = Some(120_000),
                    "caching" => endpoint.supports_implicit_caching = true,
                    "output rung" => endpoint.max_completion_tokens = Some(16_384),
                    "safe input" => endpoint.max_prompt_tokens = Some(32_768),
                    parameter => {
                        endpoint.supported_parameters.remove(parameter);
                    }
                }
                changed.push((fact, value));
            }
            for (fact, value) in changed {
                if matches!(wire, AdmissionWire::Responses)
                    && matches!(fact, "stop" | "model wire parameter")
                {
                    assert_eq!(
                        identity,
                        value.admission_identity(wire)?,
                        "Responses never sends stop"
                    );
                } else {
                    assert_ne!(
                        identity,
                        value.admission_identity(wire)?,
                        "{fact}: {wire:?}"
                    );
                }
            }
            for fact in ["auto", "none", "required", "function", "tool_choice"] {
                let mut value = base.clone();
                let endpoint = &mut value.endpoints[0];
                match fact {
                    "auto" => endpoint.tool_choice.auto = false,
                    "none" => endpoint.tool_choice.none = true,
                    "required" => endpoint.tool_choice.required = true,
                    "function" => endpoint.tool_choice.function = true,
                    _ => {
                        endpoint.supported_parameters.remove("tool_choice");
                    }
                }
                if matches!(wire, AdmissionWire::Chat) {
                    assert_eq!(
                        identity,
                        value.admission_identity(wire)?,
                        "Chat does not emit {fact}"
                    );
                } else {
                    assert_ne!(identity, value.admission_identity(wire)?, "explicit {fact}");
                }
            }
            let mut paired = base.clone();
            let mut second = paired.endpoints[0].clone();
            second.endpoint = "provider/second".to_owned();
            second.supported_parameters.remove("stop");
            second.supports_implicit_caching = true;
            paired.endpoints.push(second);
            let mut crossed = paired.clone();
            crossed.endpoints[0].supports_implicit_caching = true;
            crossed.endpoints[1].supports_implicit_caching = false;
            if matches!(wire, AdmissionWire::Responses) {
                assert_eq!(
                    paired.admission_identity(wire)?,
                    crossed.admission_identity(wire)?,
                    "Responses never sends stop"
                );
                continue;
            }
            assert_ne!(
                paired.admission_identity(wire)?,
                crossed.admission_identity(wire)?,
                "per-endpoint parameter/behaviour correlation: {wire:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn chat_tool_omitting_envelopes_bind_every_endpoint_fact()
    -> Result<(), Box<dyn std::error::Error>> {
        let base = admission_fixture()?;
        for reasoning in [false, true] {
            let mut added = base.clone();
            let mut endpoint = base.endpoints[0].clone();
            endpoint.endpoint = "provider/no-tools".to_owned();
            endpoint.supported_parameters = BTreeSet::from(["max_tokens".to_owned()]);
            if reasoning {
                endpoint.supported_parameters.insert("reasoning".to_owned());
            }
            endpoint.max_completion_tokens = Some(4_096);
            endpoint.max_prompt_tokens = Some(32_768);
            added.endpoints.push(endpoint);
            let identity = added.admission_identity(AdmissionWire::Chat)?;
            assert_ne!(base.admission_identity(AdmissionWire::Chat)?, identity);
            for fact in [
                "prompt",
                "completion",
                "context",
                "max_tokens",
                "reasoning",
                "stop",
                "caching",
            ] {
                let mut changed = added.clone();
                let endpoint = &mut changed.endpoints[1];
                match fact {
                    "prompt" => endpoint.max_prompt_tokens = Some(30_000),
                    "completion" => endpoint.max_completion_tokens = Some(5_000),
                    "context" => endpoint.context_length = Some(120_000),
                    "max_tokens" => {
                        endpoint.supported_parameters.remove("max_tokens");
                    }
                    "reasoning" => {
                        if reasoning {
                            endpoint.supported_parameters.remove("reasoning");
                        } else {
                            endpoint.supported_parameters.insert("reasoning".to_owned());
                        }
                    }
                    "stop" => {
                        endpoint.supported_parameters.insert("stop".to_owned());
                    }
                    _ => endpoint.supports_implicit_caching = true,
                }
                assert_ne!(
                    identity,
                    changed.admission_identity(AdmissionWire::Chat)?,
                    "{fact}, reasoning={reasoning}"
                );
            }
        }
        // A weaker non-reasoning endpoint hides this change in the omitted
        // envelope; only the reasoning-on-wire tool-omitting envelope sees it.
        let mut paired = base.clone();
        let mut weaker = base.endpoints[0].clone();
        weaker.supported_parameters = BTreeSet::from(["max_tokens".to_owned()]);
        weaker.max_completion_tokens = Some(4_096);
        let mut reasoning = weaker.clone();
        reasoning
            .supported_parameters
            .insert("reasoning".to_owned());
        reasoning.max_completion_tokens = Some(8_192);
        paired.endpoints.extend([weaker, reasoning]);
        let identity = paired.admission_identity(AdmissionWire::Chat)?;
        paired.endpoints[2].max_completion_tokens = Some(16_384);
        assert_ne!(identity, paired.admission_identity(AdmissionWire::Chat)?);
        Ok(())
    }

    #[test]
    fn chat_tool_omitting_endpoints_constrain_gateway_proofs()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut evidence = admission_fixture()?;
        let mut endpoint = evidence.endpoints[0].clone();
        endpoint.endpoint = "provider/no-tools".to_owned();
        endpoint.supported_parameters = BTreeSet::from(["max_tokens".to_owned()]);
        endpoint.max_completion_tokens = Some(4_096);
        endpoint.max_prompt_tokens = Some(30_000);
        evidence.endpoints.push(endpoint);
        assert_eq!(
            evidence
                .gateway_output_bound(OPENROUTER_CHAT_PROTOCOL_ID, false)?
                .get(),
            4_096
        );
        assert_eq!(
            evidence
                .gateway_output_bound(OPENROUTER_CHAT_PROTOCOL_ID, true)?
                .get(),
            32_768
        );
        assert_eq!(
            evidence.gateway_maximum_projected_input(
                OPENROUTER_CHAT_PROTOCOL_ID,
                false,
                NonZeroU32::new(4_096).ok_or("output")?
            )?,
            30_000
        );
        assert!(matches!(
            evidence.gateway_maximum_projected_input(
                OPENROUTER_CHAT_PROTOCOL_ID,
                false,
                NonZeroU32::new(32_768).ok_or("output")?
            ),
            Err(OpenRouterChatBuildError::UnprovenCompletionBound)
        ));
        evidence.endpoints[1].max_prompt_tokens = None;
        evidence.endpoints[1].context_length = None;
        assert!(matches!(
            evidence.gateway_maximum_projected_input(
                OPENROUTER_CHAT_PROTOCOL_ID,
                false,
                NonZeroU32::new(4_096).ok_or("output")?
            ),
            Err(OpenRouterChatBuildError::UnprovenPromptBound)
        ));
        evidence.endpoints[1].max_prompt_tokens = Some(30_000);
        evidence.endpoints[1]
            .supported_parameters
            .insert("reasoning".to_owned());
        assert_eq!(
            evidence
                .gateway_output_bound(OPENROUTER_CHAT_PROTOCOL_ID, true)?
                .get(),
            4_096
        );
        assert_eq!(
            evidence.gateway_maximum_projected_input(
                OPENROUTER_CHAT_PROTOCOL_ID,
                true,
                NonZeroU32::new(4_096).ok_or("output")?
            )?,
            30_000
        );
        Ok(())
    }

    #[test]
    fn admission_projection_uses_the_complete_wire_eligibility_predicate()
    -> Result<(), Box<dyn std::error::Error>> {
        let base = admission_fixture()?;
        let output = NonZeroU32::new(32_768).ok_or("output")?;
        for protocol in [
            OPENROUTER_CHAT_PROTOCOL_ID,
            crate::OPENROUTER_RESPONSES_PROTOCOL_ID,
            crate::OPENROUTER_MESSAGES_PROTOCOL_ID,
        ] {
            assert_eq!(
                base.gateway_maximum_projected_input(protocol, false, output)?,
                98_304
            );
            for parameter in ["tools", "max_tokens", "reasoning", "tool_choice"] {
                let mut changed = base.clone();
                changed.endpoints[0].supported_parameters.remove(parameter);
                let result = changed.gateway_maximum_projected_input(
                    protocol,
                    parameter == "reasoning",
                    output,
                );
                if parameter == "tool_choice" && protocol == OPENROUTER_CHAT_PROTOCOL_ID {
                    assert_eq!(result?, 98_304, "Chat uses the automatic gateway default");
                } else {
                    assert!(
                        matches!(result, Err(OpenRouterChatBuildError::UnboundedAdmission)),
                        "{protocol}: {parameter}"
                    );
                }
            }
            let mut changed = base.clone();
            changed.endpoints[0].tool_choice.auto = false;
            let result = changed.gateway_maximum_projected_input(protocol, false, output);
            if protocol == crate::OPENROUTER_MESSAGES_PROTOCOL_ID {
                assert!(result.is_err());
            } else {
                assert_eq!(result?, 98_304);
            }
        }
        assert!(
            base.gateway_output_bound("foreign.protocol", false)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn admission_projection_derivation_keeps_defaults_floor_and_all_endpoint_proofs()
    -> Result<(), Box<dyn std::error::Error>> {
        for protocol in [
            OPENROUTER_CHAT_PROTOCOL_ID,
            crate::OPENROUTER_RESPONSES_PROTOCOL_ID,
            crate::OPENROUTER_MESSAGES_PROTOCOL_ID,
        ] {
            let base = admission_fixture()?;
            let bound = base.gateway_output_bound(protocol, false)?;
            assert_eq!(bound.get(), 32_768);
            let mut unknown = base.clone();
            unknown.endpoints[0].max_completion_tokens = None;
            assert_eq!(unknown.gateway_output_bound(protocol, false)?.get(), 16_384);
            let mut plain = unknown.clone();
            plain.endpoints[0].supported_parameters.remove("reasoning");
            assert_eq!(plain.gateway_output_bound(protocol, false)?.get(), 4_096);
            assert_eq!(unknown.gateway_output_bound(protocol, true)?.get(), 16_384);
            assert!(
                unknown
                    .gateway_maximum_projected_input(protocol, false, bound)
                    .is_err()
            );
            let mut constrained = base.clone();
            constrained.context_length = 1_000_000;
            constrained.endpoints[0].max_prompt_tokens = Some(100_000);
            constrained.endpoints[0].context_length = Some(100_000);
            assert_eq!(
                constrained.gateway_maximum_projected_input(protocol, false, bound)?,
                67_232
            );
            let mut large = base.clone();
            large.context_length = 1_000_000;
            large.endpoints[0].max_prompt_tokens = None;
            large.endpoints[0].context_length = Some(1_000_000);
            assert_eq!(
                large.gateway_maximum_projected_input(protocol, false, bound)?,
                967_232
            );
            let mut small = base.clone();
            small.context_length = 16_384;
            assert_eq!(
                small.gateway_maximum_projected_input(
                    protocol,
                    false,
                    NonZeroU32::new(8_192).ok_or("output")?
                )?,
                8_192
            );
            assert_eq!(small.gateway_output_bound(protocol, false)?.get(), 8_192);
            small.context_length = 3_000;
            assert_eq!(small.gateway_output_bound(protocol, false)?.get(), 4_096);
            assert!(
                small
                    .gateway_maximum_projected_input(
                        protocol,
                        false,
                        small.gateway_output_bound(protocol, false)?
                    )
                    .is_err()
            );
            let mut mixed = base.clone();
            let mut endpoint = mixed.endpoints[0].clone();
            endpoint.endpoint = "provider/second".to_owned();
            endpoint.max_prompt_tokens = Some(40_000);
            mixed.endpoints.push(endpoint);
            assert_eq!(
                mixed.gateway_maximum_projected_input(protocol, false, bound)?,
                40_000
            );
            mixed.endpoints[1].max_prompt_tokens = None;
            mixed.endpoints[1].context_length = Some(60_000);
            assert_eq!(
                mixed.gateway_maximum_projected_input(protocol, false, bound)?,
                27_232
            );
            mixed.endpoints[1].context_length = None;
            assert!(
                mixed
                    .gateway_maximum_projected_input(protocol, false, bound)
                    .is_err()
            );
            mixed.endpoints[1].max_prompt_tokens = Some(40_000);
            mixed.endpoints[1].max_completion_tokens = Some(128);
            assert!(
                mixed
                    .gateway_maximum_projected_input(protocol, false, bound)
                    .is_err()
            );
        }
        Ok(())
    }

    #[test]
    fn admission_projection_does_not_authorize_tampered_or_foreign_snapshot()
    -> Result<(), Box<dyn std::error::Error>> {
        let evidence = admission_fixture()?;
        let route = OpenRouterChatRoute::new_local_fixture_with_retention(
            Arc::new(crate::StaticOpenRouterApiKeySource::new(
                crate::OpenRouterApiKey::new("offline-admission-key")?,
                "offline-account",
                "offline-authority",
            )?),
            crate::OpenRouterOrigin::new("https://127.0.0.1:18443")?,
            OpenRouterRetentionPolicy::strict(),
        )?
        .with_clock(Arc::new(plakous_model::FixedClock::new(Timestamp::new(
            OPENROUTER_CHAT_REVIEWED_AT + 60,
            0,
        )?)));
        let now =
            plakous_model::FixedClock::new(Timestamp::new(OPENROUTER_CHAT_REVIEWED_AT + 60, 0)?)
                .now();
        assert!(evidence.validates(route.identity(), now));
        let identity = evidence.admission_identity(AdmissionWire::Chat)?;
        assert_ne!(identity, evidence.catalog_snapshot_identity);
        let mut tampered = evidence.clone();
        tampered.endpoints[0].provider_name = "Unattested".to_owned();
        assert_eq!(identity, tampered.admission_identity(AdmissionWire::Chat)?);
        assert!(!tampered.validates(route.identity(), now));
        assert!(!evidence.validates(route.identity(), evidence.expires_at()));
        let foreign = OpenRouterChatRoute::new_local_fixture_with_retention(
            Arc::new(crate::StaticOpenRouterApiKeySource::new(
                crate::OpenRouterApiKey::new("offline-admission-key")?,
                "foreign-account",
                "offline-authority",
            )?),
            crate::OpenRouterOrigin::new("https://127.0.0.1:18443")?,
            OpenRouterRetentionPolicy::strict(),
        )?
        .with_clock(Arc::new(plakous_model::FixedClock::new(Timestamp::new(
            OPENROUTER_CHAT_REVIEWED_AT + 60,
            0,
        )?)));
        assert!(!evidence.validates(foreign.identity(), now));
        let moved = OpenRouterChatRoute::new_local_fixture_with_retention(
            Arc::new(crate::StaticOpenRouterApiKeySource::new(
                crate::OpenRouterApiKey::new("offline-admission-key")?,
                "offline-account",
                "offline-authority",
            )?),
            crate::OpenRouterOrigin::new("https://127.0.0.1:18444")?,
            OpenRouterRetentionPolicy::strict(),
        )?
        .with_clock(Arc::new(plakous_model::FixedClock::new(Timestamp::new(
            OPENROUTER_CHAT_REVIEWED_AT + 60,
            0,
        )?)));
        assert!(!evidence.validates(moved.identity(), now));
        Ok(())
    }

    #[test]
    fn admission_projection_pins_every_envelope_and_derivation_input()
    -> Result<(), Box<dyn std::error::Error>> {
        let evidence = admission_fixture()?;
        let identities = [
            AdmissionWire::Chat,
            AdmissionWire::Responses,
            AdmissionWire::Messages,
        ]
        .map(|wire| {
            evidence
                .admission_identity(wire)
                .map(|identity| identity.to_string())
        });
        let identities: Vec<_> = identities.into_iter().collect::<Result<_, _>>()?;
        assert_eq!(
            identities,
            [
                "f314755328e541f9c39e5826b03b3395270d9415fc8fb6f4596a347bac5d5a98",
                "e4b2fb777f9be495f723d05ecee0fb7a76b32a29962453fcc9eab4336fe867f2",
                "de12dc9d0dc7dad4a12e61abf43ea5db8172101dc6e72b134824ce31068c61f8"
            ]
        );
        Ok(())
    }

    #[test]
    fn an_openrouter_context_overflow_body_is_classified_as_its_own_kind() {
        use plakous_attempt::TerminalFacts;
        assert!(body_reports_context_overflow(
            br#"{"error":{"code":400,"message":"This endpoint's maximum context length is 131072 tokens. However, you requested about 140000 tokens (139000 of text input, 1000 in the output)."}}"#
        ));
        assert!(body_reports_context_overflow(
            br#"{"error":{"code":400,"message":"Provider returned error","metadata":{"raw":"{\"error\":{\"code\":\"context_length_exceeded\"}}"}}}"#
        ));
        assert!(!body_reports_context_overflow(
            br#"{"error":{"code":400,"message":"No endpoints found that support the parameters requested."}}"#
        ));
        assert!(!body_reports_context_overflow(b"not json"));
        let facts = TerminalFacts::preacceptance_test();
        assert_eq!(
            context_overflow_kind(true, Some(facts)),
            Some(ModelErrorKind::ContextLimit)
        );
        assert_eq!(context_overflow_kind(false, Some(facts)), None);
    }

    #[tokio::test]
    async fn joined_error_follows_prefix_queued_by_the_same_operation_poll()
    -> Result<(), Box<dyn std::error::Error>> {
        use futures_util::StreamExt as _;
        use plakous_attempt::{ObservationSnapshot, ProviderAcceptance, TerminalFacts};
        use plakous_model::{AttemptId, CancellationToken, ModelCallContext, ResponseStarted};
        use std::str::FromStr as _;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let binding = ChatOperationBinding::new(
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
        let (mut attempt, receiver) = ChatAttempt::with_event_channel(
            binding,
            context,
            NonZeroUsize::new(4).ok_or("capacity")?,
        );
        let completed = Arc::new(AtomicUsize::new(0));
        let operation_completed = Arc::clone(&completed);
        let operation = Box::pin(async move {
            attempt.events_mut().send(prefix).await?;
            let observation = ChatAttemptProtocolError::new("accepted stream failed");
            attempt.events_mut().send_error(observation.clone()).await?;
            let facts = TerminalFacts::from_snapshot(ObservationSnapshot {
                physical_attempts: 1,
                provider_acceptance: ProviderAcceptance::Accepted,
                visible_output: false,
                replay_safety: plakous_attempt::ReplaySafety::CostOnly,
                hosted_effects_possible: false,
                logical_success: false,
            });
            operation_completed.fetch_add(1, Ordering::SeqCst);
            Err(ChatAttemptError::protocol(observation, facts))
        });
        let mut stream = JoinedChatStream {
            descriptor: test_descriptor(),
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
        // Accepted, no hosted effect, nothing named: a definite failure that
        // states its duplicate risk.
        assert_eq!(error.kind(), ModelErrorKind::Protocol);
        assert_eq!(
            error.context().map(ModelErrorContext::retryability),
            Some(ModelRetryability::Unsafe {
                risk: ModelRetryRisk::DuplicateInferenceOrBilling
            })
        );
        for _ in 0..3 {
            assert!(stream.next().await.is_none());
        }
        assert_eq!(completed.load(Ordering::SeqCst), 1);
        Ok(())
    }

    fn assert_language_model<T: LanguageModel>() {}

    #[test]
    fn bounded_literals_have_no_syntactic_catalog_privilege() {
        assert!(concrete_model_id("vendor/new-model_2026.08"));
        assert!(concrete_model_id("nvidia/model-v3:free"));
        assert!(concrete_model_id("openrouter/auto"));
        assert!(concrete_model_id("vendor/model:beta"));
        assert!(concrete_model_id("vendor/model-latest"));
        assert!(!concrete_model_id("another-vendor/deep/model-v3"));
    }

    #[test]
    fn routing_choices_are_explicit_and_never_protocol_fallback()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_language_model::<OpenRouterChatLanguageModel>();
        let strict = OpenRouterChatRoutingPolicy::StrictPinned(OpenRouterRoutingPolicy::strict(
            "vendor/endpoint-v1",
            "Vendor",
        )?);
        assert!(strict.pinned().is_some());
        assert!(!strict.allows_cross_protocol_fallback());
        let managed = OpenRouterChatRoutingPolicy::GatewayManaged;
        assert!(managed.pinned().is_none());
        assert!(!managed.allows_cross_protocol_fallback());
        assert_eq!(OPENROUTER_CHAT_COMPLETIONS_PATH, "/api/v1/chat/completions");
        assert_ne!(
            OPENROUTER_CHAT_PROTOCOL_ID,
            crate::OpenRouterDialect::StatelessResponsesBeta2026_07.protocol_id()
        );
        Ok(())
    }

    #[test]
    fn sse_content_type_accepts_parameters_but_not_other_media_types() {
        assert!(is_sse_content_type("text/event-stream"));
        assert!(is_sse_content_type("Text/Event-Stream; charset=utf-8"));
        assert!(!is_sse_content_type("application/json"));
    }

    #[test]
    fn openrouter_generation_header_is_primary_request_correlation() {
        let mut headers = HeaderMap::new();
        headers.insert("x-request-id", HeaderValue::from_static("legacy-request"));
        headers.insert("request-id", HeaderValue::from_static("request"));
        headers.insert("x-generation-id", HeaderValue::from_static("generation"));
        assert_eq!(
            openrouter_request_id(|name| headers.get(name)),
            Some("generation")
        );
        headers.remove("x-generation-id");
        assert_eq!(
            openrouter_request_id(|name| headers.get(name)),
            Some("request")
        );
    }

    #[test]
    fn initial_policy_and_rate_limits_reject_while_other_statuses_fail_closed() {
        for status in [StatusCode::BAD_REQUEST, StatusCode::INTERNAL_SERVER_ERROR] {
            assert_eq!(
                post_send_status_outcome(status),
                PhysicalRequestOutcome::AmbiguousPostSend
            );
        }
        assert_eq!(
            post_send_status_outcome(StatusCode::UNAUTHORIZED),
            PhysicalRequestOutcome::DefiniteRejection
        );
        assert_eq!(
            post_send_status_outcome(StatusCode::TOO_MANY_REQUESTS),
            PhysicalRequestOutcome::DefiniteRejection
        );
        assert_eq!(
            post_send_status_outcome(StatusCode::FORBIDDEN),
            PhysicalRequestOutcome::DefiniteRejection
        );
        assert!(!gateway_retention_is_explicit(None));
        let default = OpenRouterRetentionPolicy::gateway_default();
        assert_eq!(default.data_collection(), None);
        assert_eq!(default.zdr(), None);
        let strict = OpenRouterRetentionPolicy::strict();
        assert_eq!(strict.data_collection(), Some("deny"));
        assert_eq!(strict.zdr(), Some(true));
    }

    /// The `OpenRouter` Chat row of the provider-error matrix.
    ///
    /// The columns are the ways a provider can fail, and every one of them must
    /// be definite unless a hosted effect may have run. This route is the
    /// hardest case for that rule: it commits HTTP 200 at acceptance and then
    /// carries failures inside the stream, so acceptance can never separate
    /// them.
    #[test]
    fn the_openrouter_chat_row_of_the_provider_failure_matrix_is_definite() {
        use plakous_attempt::ProviderRefusal as Refusal;

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
            // Facts as the adapter reports them for an accepted stream that had
            // already emitted output: the live shape.
            let kind = named_refusal_kind(Some(refusal), None);
            assert_eq!(kind, Some(expected), "{name}");
            let kind = kind.unwrap_or(ModelErrorKind::Protocol);
            assert_ne!(kind, ModelErrorKind::OutcomeUnknown, "{name}");
            // The same column with a hosted effect in flight is the one thing
            // that stays undetermined, and the facts carrying it say so.
            assert_eq!(
                named_refusal_kind(Some(refusal), Some(hosted_effect_facts())),
                None,
                "{name} with a hosted effect must not be classified definitely"
            );
        }
        // A failure the gateway did not name keeps the conservative reading.
        assert_eq!(named_refusal_kind(None, None), None);
    }

    /// An initial `401` is an authentication rejection only while the facts
    /// prove the provider never accepted the request and no hosted effect can
    /// be in flight; the same status with possible hosted effects stays
    /// undetermined.
    #[test]
    fn an_initial_401_is_authentication_only_without_acceptance_or_hosted_effects() {
        let rejected = ChatAttemptError::protocol(
            ChatAttemptProtocolError::new("OpenRouter Chat HTTP 401 (rejected-before-acceptance)")
                .with_http_status(401),
            TerminalFacts::preacceptance_test(),
        );
        assert_eq!(
            chat_model_error(&test_descriptor(), &rejected).kind(),
            ModelErrorKind::Authentication
        );
        let hosted = ChatAttemptError::protocol(
            ChatAttemptProtocolError::new("OpenRouter Chat HTTP 401 (post-acceptance)")
                .with_http_status(401),
            hosted_effect_facts(),
        );
        assert_eq!(
            chat_model_error(&test_descriptor(), &hosted).kind(),
            ModelErrorKind::OutcomeUnknown
        );
    }

    /// Terminal facts for an accepted exchange with a hosted effect in flight.
    fn hosted_effect_facts() -> TerminalFacts {
        TerminalFacts::from_snapshot(plakous_attempt::ObservationSnapshot {
            physical_attempts: 1,
            provider_acceptance: plakous_attempt::ProviderAcceptance::Accepted,
            visible_output: true,
            replay_safety: plakous_attempt::ReplaySafety::ServerEffect,
            hosted_effects_possible: true,
            logical_success: false,
        })
    }

    #[test]
    fn openrouter_error_detail_takes_message_and_drops_secrets() {
        assert_eq!(
            openrouter_error_detail(
                br#"{"error":{"message":"No endpoints found that support the parameters requested."}}"#
            )
            .as_deref(),
            Some("No endpoints found that support the parameters requested.")
        );
        assert_eq!(
            openrouter_error_detail(
                br#"{"error":{"message":"Provider returned error","metadata":{"raw":"Unknown parameter: 'session_id'"}}}"#
            )
            .as_deref(),
            Some("Provider returned error: Unknown parameter: 'session_id'")
        );
        assert_eq!(
            openrouter_error_detail(br#"{"error":{"message":"Authorization Bearer sk-test"}}"#),
            None
        );
        assert_eq!(openrouter_error_detail(br"not-json"), None);
    }

    #[test]
    fn reviewed_chat_pin_bounds_authority_and_refuses_after_expiry()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(
            OPENROUTER_CHAT_REVIEW_EXPIRES_AT - OPENROUTER_CHAT_REVIEWED_AT,
            30 * 86_400
        );
        let expiry = Timestamp::new(OPENROUTER_CHAT_REVIEW_EXPIRES_AT, 0)?;
        let before = Timestamp::new(OPENROUTER_CHAT_REVIEW_EXPIRES_AT - 60, 0)?;
        let short = Timestamp::new(OPENROUTER_CHAT_REVIEW_EXPIRES_AT - 30, 0)?;
        let long = Timestamp::new(OPENROUTER_CHAT_REVIEW_EXPIRES_AT + 3_600, 0)?;
        assert_eq!(reviewed_until(short, before)?, short);
        assert_eq!(reviewed_until(long, before)?, expiry);
        for at in [expiry, long] {
            let refused = reviewed_until(long, at);
            assert!(matches!(
                refused,
                Err(OpenRouterChatBuildError::ProfileNotCurrent)
            ));
            assert_eq!(
                refused.err().map(|error| error.to_string()),
                Some(
                    "OpenRouter Chat compatibility profile is not currently admissible".to_owned()
                )
            );
        }
        Ok(())
    }

    #[test]
    fn disabled_reasoning_wire_follows_openrouter_reasoning_facts()
    -> Result<(), Box<dyn std::error::Error>> {
        let default_on = OpenRouterReasoningFacts {
            supported_efforts: Some(vec!["medium".to_owned()]),
            default_effort: None,
            default_enabled: true,
            supports_max_tokens: false,
            mandatory: false,
        };
        let evidence = OpenRouterChatModelEvidence {
            model: ModelId::new("vendor/model")?,
            catalog_snapshot_identity: Digest::sha256(
                DigestDomain::ProviderSemanticEvidence,
                1,
                &[b"x"],
            )?,
            route_fingerprint: "route".to_owned(),
            account_binding: Digest::sha256(DigestDomain::ModelBinding, 1, &[b"account"])?,
            credential_authority_binding: Digest::sha256(
                DigestDomain::ModelBinding,
                1,
                &[b"auth"],
            )?,
            endpoints: vec![],
            model_supported_parameters: BTreeSet::default(),
            reasoning: Some(default_on),
            context_length: 1,
            image_input: false,
            observed_at: Timestamp::new(1, 0)?,
            expires_at: Timestamp::new(2, 0)?,
        };
        assert_eq!(
            openrouter_disabled_reasoning_wire(&evidence, plakous_model::ReasoningPolicy::Disabled),
            ChatDisabledReasoningWire::EnabledFalse
        );

        let mandatory = OpenRouterReasoningFacts {
            mandatory: true,
            default_enabled: true,
            supported_efforts: None,
            default_effort: None,
            supports_max_tokens: false,
        };
        let evidence = OpenRouterChatModelEvidence {
            reasoning: Some(mandatory),
            ..evidence
        };
        assert_eq!(
            openrouter_disabled_reasoning_wire(&evidence, plakous_model::ReasoningPolicy::Disabled),
            ChatDisabledReasoningWire::Omit
        );

        assert_eq!(
            openrouter_disabled_reasoning_wire(
                &OpenRouterChatModelEvidence {
                    reasoning: None,
                    ..evidence
                },
                plakous_model::ReasoningPolicy::Disabled
            ),
            ChatDisabledReasoningWire::Omit
        );
        Ok(())
    }
}
