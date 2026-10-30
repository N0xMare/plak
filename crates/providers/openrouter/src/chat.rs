//! First-class OpenRouter Chat Completions dialect and executable target.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use plakous_attempt::FailureRisk;
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
    ContextEvidenceClass, CredentialSelectionId, Digest, DigestDomain,
    EffectiveProviderConfiguration, FactProvenance, FactSource, FamilyPolicyEnvelope,
    FeatureSupport, LanguageModel, ModelAdapterProfileId, ModelError, ModelErrorKind,
    ModelFamilyId, ModelId, ModelImplementationIdentity, ModelPreparationContext,
    ModelPreparationError, ModelPreparationErrorKind, NamespaceId, PreparedModelCall, ProtocolId,
    ProviderAuthorityClassId, ProviderBehaviorConfiguration, ProviderProfileRevisionId,
    ProviderRouteSelection, ProviderSemanticEvidence, ResolvedModelDescriptor,
    ResolvedProviderAuthorityBinding, RuntimeModelAuthority, RuntimeModelAuthorityError,
    RuntimeModelAuthorityOperation, SelectedProviderConfiguration, Support, SystemIdGenerator,
    Timestamp, ValidatedRequestPlan,
};
use serde::Serialize;
use thiserror::Error;

use crate::wire::{
    AttemptWire, JoinedStream, WireProjection, body_reports_context_overflow,
    context_overflow_kind, generation_number, inferred_failure_kind, insert_attribution,
    is_sse_content_type, named_refusal_kind, operation_error, post_send_status_outcome, rejection,
    usage_limit_kind,
};
use crate::{
    OpenRouterChatRoute, OpenRouterKeyGeneration, OpenRouterRetentionPolicy,
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
pub(crate) const OPENROUTER_CHAT_REVIEWED_AT: u64 = 1_793_318_400;
/// When the reviewed Chat Completions facts are due for re-review. It drives
/// only doctor and status notes and the publish floor; the route never
/// refuses on the calendar.
pub(crate) const OPENROUTER_CHAT_REVIEW_DUE_AT: u64 = OPENROUTER_CHAT_REVIEWED_AT + 90 * 86_400;

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

pub(crate) struct OpenRouterChatDiscovery {
    pub(crate) endpoints: Vec<OpenRouterChatEndpointCandidate>,
    pub(crate) model_supported_parameters: BTreeSet<String>,
    pub(crate) reasoning: Option<OpenRouterReasoningFacts>,
    pub(crate) context_length: u64,
    pub(crate) image_input: bool,
    pub(crate) observed_at: Timestamp,
    pub(crate) expires_at: Timestamp,
}

impl OpenRouterChatModelEvidence {
    pub(crate) fn from_authenticated_discovery(
        model: ModelId,
        route: &crate::OpenRouterChatRouteIdentity,
        discovery: OpenRouterChatDiscovery,
    ) -> Result<Self, OpenRouterChatBuildError> {
        let OpenRouterChatDiscovery {
            endpoints,
            model_supported_parameters,
            reasoning,
            context_length,
            image_input,
            observed_at,
            expires_at,
        } = discovery;
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
        let grant = self.route.current_credential_generation_grant()?;
        RuntimeModelAuthority::new(
            &self.descriptor,
            self.route.identity().account_binding(),
            self.route.identity().credential_authority_binding(),
            generation_number(&grant),
            checked_at,
            Some(self.evidence.expires_at()),
        )
        .map_err(|_| OpenRouterChatBuildError::Configuration)
    }
}

fn openrouter_chat_dialect_capabilities(
    evidence: &OpenRouterChatModelEvidence,
) -> Result<CapabilitySet, OpenRouterChatBuildError> {
    let provenance = FactProvenance::new(
        FactSource::BuiltIn,
        Timestamp::new(OPENROUTER_CHAT_REVIEWED_AT, 0)?,
        None,
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

impl OpenRouterChatLanguageModel {
    fn request_policy(
        &self,
        plan: &ValidatedRequestPlan,
        context: &ModelPreparationContext,
        output_bound: std::num::NonZeroU32,
    ) -> Result<ChatRequestPolicy, ModelPreparationError> {
        let descriptor = self.target.descriptor();
        let identity = self.target.route.identity();
        let router = match self.target.routing() {
            OpenRouterChatRoutingPolicy::StrictPinned(_) => {
                return Err(rejection(
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
                        rejection(
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
            rejection(
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
        Ok(policy)
    }
}

impl LanguageModel for OpenRouterChatLanguageModel {
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
                    OpenRouterChatBuildError::StaleEvidence => RuntimeModelAuthorityError::Stale,
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
        OPENROUTER_CHAT_WIRE.admit_plan(descriptor, &self.configuration, &plan, &context)?;
        if context.active_family_artifact().is_some() {
            return Err(rejection(
                ModelPreparationErrorKind::InvalidRequest,
                "stateless Chat has no aggregate family artifact",
            ));
        }
        let identity = self.target.route.identity();
        let output_bound =
            OPENROUTER_CHAT_WIRE.admit_route(identity, &self.configuration, &context)?;
        let policy = self.request_policy(&plan, &context, output_bound)?;
        let replay_target = policy.replay_target();
        let stream_reasoning = policy.reasoning();
        let stream_reasoning_replay = policy.reasoning_replay();
        let stream_reasoning_alias = policy.reasoning_alias();
        let lowered = lower_request(&plan, &policy).map_err(chat_lowering_error)?;
        let record = OPENROUTER_CHAT_WIRE.prepare_record(
            descriptor,
            &plan,
            &context,
            lowered.body(),
            output_bound,
        )?;
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
                rejection(
                    ModelPreparationErrorKind::InvalidConfiguration,
                    "OpenRouter Chat credential generation is unavailable",
                )
            })?,
        };
        let client = self.client.clone();
        let origin = Origin::parse(identity.origin().as_str()).map_err(|_| {
            rejection(
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
                Ok(Box::pin(JoinedStream::<ChatWire, _>::new(
                    descriptor,
                    VecDeque::new(),
                    operation,
                    receiver,
                )) as plakous_model::ModelStream)
            })
        }))
    }
}

#[derive(Clone)]
struct OpenRouterChatHooks {
    target: OpenRouterChatTarget,
    clock: Arc<dyn Clock>,
    generation: OpenRouterKeyGeneration,
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
        if current.credential_generation() != generation_number(&self.generation) {
            return Err(ChatAttemptProtocolError::new(
                "OpenRouter Chat dispatch credential changed",
            ));
        }
        let key = self
            .target
            .route
            .chat_credential(physical, &self.generation)
            .map_err(|_| ChatAttemptProtocolError::new("OpenRouter Chat authentication failed"))?;
        let authorization = sensitive_header_value(crate::route::bearer_bytes(&key))
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
        // A well-formed error object is OpenRouter's own report, not a stream
        // that broke the grammar, and the operator text says which it was.
        let protocol = ChatAttemptProtocolError::new(match error.in_band_detail() {
            Some(detail) => {
                format!(
                    "OpenRouter returned an in-band error after accepting the request: {detail}"
                )
            }
            None => format!("OpenRouter Chat stream validation failed: {error}"),
        });
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

/// The Chat wire of the shared OpenRouter target skeleton.
enum ChatWire {}

impl AttemptWire for ChatWire {
    type Response = plakous_chat_completions::ChatAttemptResponse;
    type AttemptError = ChatAttemptError;
    type ProtocolError = ChatAttemptProtocolError;

    fn model_error(descriptor: &ResolvedModelDescriptor, error: &ChatAttemptError) -> ModelError {
        chat_model_error(descriptor, error)
    }

    fn observation_error(
        descriptor: &ResolvedModelDescriptor,
        error: &ChatAttemptProtocolError,
    ) -> ModelError {
        chat_observation_error(descriptor, error)
    }
}

const OPENROUTER_CHAT_WIRE: WireProjection = WireProjection {
    label: "Chat",
    wire_domain: b"plakous-openrouter/chat-wire-v1",
    family_id: CHAT_COMPLETIONS_FAMILY_ID,
    lowering_revision: OPENROUTER_CHAT_LOWERING_REVISION,
    projection_revision: OPENROUTER_CHAT_PROJECTION_REVISION,
    accounting_revision: OPENROUTER_CHAT_ACCOUNTING_REVISION,
};

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
        error.http_status(),
        format!("OpenRouter Chat protocol failed: {error}"),
    )
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
        error.http_status(),
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
) -> Result<OpenRouterKeyGeneration, crate::OpenRouterAuthError> {
    target.route.current_credential_generation_grant()
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

fn chat_lowering_error(error: plakous_chat_completions::ChatRequestError) -> ModelPreparationError {
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
    rejection(
        kind,
        format!(
            "OpenRouter Chat request lowering failed: {}",
            error.message()
        ),
    )
}

#[cfg(test)]
mod tests;
