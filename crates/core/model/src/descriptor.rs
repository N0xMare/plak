//! Minimal resolved descriptors, capabilities, and immutable request plans.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::{
    CachePolicy, CapabilityRequirement, CapacityRule, ContextAdmission, ContextAdmissionError,
    ContextAdmissionRequirement, ContextCapacity, ContextContract, ContextContractError,
    ContextEvidenceClass, ContextMeasurement, ContextScope, ContextStatus, ContextStatusKind,
    ContextUnavailable, Digest, DigestDomain, DigestError, IntegrationClass, ModelAdapterProfileId,
    ModelFamilyId, ModelId, ModelRequest, NamespaceId, OutputFormat, OutputReservation,
    PreparedModelProjection, PreparedOutputLimitSource, ProtocolId, ReasoningPolicy, RouteId,
    StandardIntegrationAuthority, Timestamp,
};

/// A neutral feature that may affect request preparation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[non_exhaustive]
pub enum Capability {
    /// Structural incremental event delivery.
    Streaming,
    /// Model reasoning controls and replay.
    Reasoning,
    /// Model-visible tool definitions and calls.
    Tools,
    /// Provider-hosted code execution owned entirely by the inference service.
    HostedCodeExecution,
    /// Provider-hosted programs that may call explicitly eligible customer tools.
    HostedProgrammaticToolCalling,
    /// Multiple tool calls that may execute in parallel.
    ParallelTools,
    /// Schema-constrained output.
    StructuredOutput,
    /// Stateful or replay-backed continuation.
    Continuation,
    /// Prompt or prefix caching.
    Caching,
    /// Namespaced native options.
    NativeOptions,
}

/// How a resolved model supplies a capability.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum Support {
    /// The capability is supplied directly.
    Native,
    /// The capability is supplied with equivalent portable semantics.
    Emulated,
    /// The capability is supplied with a documented semantic downgrade.
    Degraded,
    /// The capability is unavailable.
    Unsupported,
}

/// The evidence source for a descriptor or capability fact.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum FactSource {
    /// A versioned built-in catalog.
    BuiltIn,
    /// Runtime model discovery.
    Discovery,
    /// Route-specific conformance evidence.
    RouteEvidence,
    /// An explicit user override.
    UserOverride,
    /// A deterministic test fixture.
    TestFixture,
}

/// Provenance and freshness for one resolved fact.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FactProvenance {
    source: FactSource,
    observed_at: Timestamp,
    expires_at: Option<Timestamp>,
}

impl FactProvenance {
    /// Creates fact provenance.
    #[must_use]
    pub const fn new(
        source: FactSource,
        observed_at: Timestamp,
        expires_at: Option<Timestamp>,
    ) -> Self {
        Self {
            source,
            observed_at,
            expires_at,
        }
    }

    /// Returns the evidence source.
    #[must_use]
    pub const fn source(self) -> FactSource {
        self.source
    }

    /// Returns observation time.
    #[must_use]
    pub const fn observed_at(self) -> Timestamp {
        self.observed_at
    }

    /// Returns evidence expiry when applicable.
    #[must_use]
    pub const fn expires_at(self) -> Option<Timestamp> {
        self.expires_at
    }

    /// Reports whether evidence is expired at `now`.
    #[must_use]
    pub fn is_expired(self, now: Timestamp) -> bool {
        self.expires_at.is_some_and(|expiry| expiry <= now)
    }
}

/// Minimal neutral constraints attached to one capability fact.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct CapabilityConstraints {
    maximum: Option<u64>,
    note: Option<String>,
}

impl CapabilityConstraints {
    /// Creates constraints with an optional numeric maximum and diagnostic note.
    #[must_use]
    pub fn new(maximum: Option<u64>, note: Option<String>) -> Self {
        Self { maximum, note }
    }

    /// Returns the optional numeric maximum.
    #[must_use]
    pub const fn maximum(&self) -> Option<u64> {
        self.maximum
    }

    /// Returns the optional diagnostic note.
    #[must_use]
    pub fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }
}

/// Resolved support, constraints, and evidence for one capability.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FeatureSupport {
    support: Support,
    constraints: CapabilityConstraints,
    provenance: FactProvenance,
}

impl FeatureSupport {
    /// Creates a resolved support fact.
    #[must_use]
    pub const fn new(
        support: Support,
        constraints: CapabilityConstraints,
        provenance: FactProvenance,
    ) -> Self {
        Self {
            support,
            constraints,
            provenance,
        }
    }

    /// Returns support quality.
    #[must_use]
    pub const fn support(&self) -> Support {
        self.support
    }

    /// Returns constraints.
    #[must_use]
    pub const fn constraints(&self) -> &CapabilityConstraints {
        &self.constraints
    }

    /// Returns fact provenance.
    #[must_use]
    pub const fn provenance(&self) -> FactProvenance {
        self.provenance
    }
}

/// An immutable set of resolved capability facts.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct CapabilitySet {
    features: BTreeMap<Capability, FeatureSupport>,
    native_option_namespaces: BTreeMap<NamespaceId, FeatureSupport>,
}

#[derive(Serialize)]
struct SemanticFactSupport<'a> {
    support: Support,
    constraints: &'a CapabilityConstraints,
    source: FactSource,
}

#[derive(Serialize)]
struct SemanticDescriptorBinding<'a> {
    integration_class: IntegrationClass,
    protocol: &'a ProtocolId,
    route: &'a RouteId,
    requested_model: &'a ModelId,
    reported_model: Option<&'a ModelId>,
    #[serde(skip_serializing_if = "<[ModelId]>::is_empty")]
    same_served_models: &'a [ModelId],
    model_family: Option<&'a ModelFamilyId>,
    profile: &'a ModelAdapterProfileId,
    features: BTreeMap<Capability, SemanticFactSupport<'a>>,
    native_option_namespaces: BTreeMap<&'a NamespaceId, SemanticFactSupport<'a>>,
    context_contract: Option<Digest>,
}

fn semantic_fact_support(fact: &FeatureSupport) -> SemanticFactSupport<'_> {
    SemanticFactSupport {
        support: fact.support(),
        constraints: fact.constraints(),
        source: fact.provenance().source(),
    }
}

impl CapabilitySet {
    /// Creates a capability set from feature and native-namespace facts.
    #[must_use]
    pub fn new(
        features: BTreeMap<Capability, FeatureSupport>,
        native_option_namespaces: BTreeMap<NamespaceId, FeatureSupport>,
    ) -> Self {
        Self {
            features,
            native_option_namespaces,
        }
    }

    /// Returns support for a neutral feature.
    #[must_use]
    pub fn get(&self, capability: Capability) -> Option<&FeatureSupport> {
        self.features.get(&capability)
    }

    /// Returns support for one native-option namespace.
    #[must_use]
    pub fn native_namespace(&self, namespace: &NamespaceId) -> Option<&FeatureSupport> {
        self.native_option_namespaces.get(namespace)
    }

    /// Returns all neutral feature facts.
    #[must_use]
    pub const fn features(&self) -> &BTreeMap<Capability, FeatureSupport> {
        &self.features
    }
}

/// A minimal immutable descriptor bound to protocol, route, model, and profile identities.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ResolvedModelDescriptor {
    integration_class: IntegrationClass,
    protocol: ProtocolId,
    route: RouteId,
    requested_model: ModelId,
    reported_model: Option<ModelId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    same_served_models: Vec<ModelId>,
    model_family: Option<ModelFamilyId>,
    profile: ModelAdapterProfileId,
    capabilities: CapabilitySet,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    context_contract: Option<ContextContract>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolvedModelDescriptorWire {
    integration_class: IntegrationClass,
    protocol: ProtocolId,
    route: RouteId,
    requested_model: ModelId,
    reported_model: Option<ModelId>,
    #[serde(default)]
    same_served_models: Vec<ModelId>,
    model_family: Option<ModelFamilyId>,
    profile: ModelAdapterProfileId,
    capabilities: CapabilitySet,
    #[serde(default)]
    context_contract: Option<ContextContract>,
}

impl<'de> Deserialize<'de> for ResolvedModelDescriptor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ResolvedModelDescriptorWire::deserialize(deserializer)?;
        if wire
            .same_served_models
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
        {
            return Err(serde::de::Error::custom(
                "same served models must be sorted and unique",
            ));
        }
        let value = Self {
            integration_class: wire.integration_class,
            protocol: wire.protocol,
            route: wire.route,
            requested_model: wire.requested_model,
            reported_model: wire.reported_model,
            same_served_models: wire.same_served_models,
            model_family: wire.model_family,
            profile: wire.profile,
            capabilities: wire.capabilities,
            context_contract: wire.context_contract,
        };
        value
            .validate_integration_class()
            .map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

impl ResolvedModelDescriptor {
    /// Creates a resolved descriptor from already-resolved facts.
    #[must_use]
    #[allow(
        clippy::too_many_arguments,
        reason = "the authority token is additional to the complete descriptor fact set"
    )]
    pub fn new(
        _authority: &StandardIntegrationAuthority,
        protocol: ProtocolId,
        route: RouteId,
        requested_model: ModelId,
        reported_model: Option<ModelId>,
        model_family: Option<ModelFamilyId>,
        profile: ModelAdapterProfileId,
        capabilities: CapabilitySet,
    ) -> Self {
        Self {
            integration_class: IntegrationClass::Standard,
            protocol,
            route,
            requested_model,
            reported_model,
            same_served_models: Vec::new(),
            model_family,
            profile,
            capabilities,
            context_contract: None,
        }
    }

    /// Creates a resolved descriptor for a caller-supplied integration.
    #[must_use]
    pub fn new_custom(
        protocol: ProtocolId,
        route: RouteId,
        requested_model: ModelId,
        reported_model: Option<ModelId>,
        model_family: Option<ModelFamilyId>,
        profile: ModelAdapterProfileId,
        capabilities: CapabilitySet,
    ) -> Self {
        Self {
            integration_class: IntegrationClass::Custom,
            protocol,
            route,
            requested_model,
            reported_model,
            same_served_models: Vec::new(),
            model_family,
            profile,
            capabilities,
            context_contract: None,
        }
    }

    /// Freezes provider-owned capacity and count evidence into this descriptor.
    ///
    /// The selected [`crate::LanguageModel`] remains the coarse trust boundary:
    /// a consumer count or product override cannot modify this contract after
    /// model resolution, and the contract participates in the descriptor
    /// binding digest.
    #[must_use]
    pub fn with_context_contract(mut self, context_contract: ContextContract) -> Self {
        self.context_contract = Some(context_contract);
        self
    }

    /// Declares other exact served-model ids the provider reports for this
    /// same model, so their usage counts as this binding's context evidence.
    ///
    /// Only the integration's reviewed facts may declare these; they never
    /// make an id selectable, and responses still report the actual id.
    #[must_use]
    pub fn with_same_served_models(mut self, models: impl IntoIterator<Item = ModelId>) -> Self {
        self.same_served_models = models
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        self
    }

    /// Returns the declared additional served-model ids, sorted and unique.
    #[must_use]
    pub fn same_served_models(&self) -> &[ModelId] {
        &self.same_served_models
    }

    /// Whether a provider-reported served model is this binding's model:
    /// the requested id, the reported id, or a declared same served model.
    #[must_use]
    pub fn serves_same_model(&self, model: &ModelId) -> bool {
        model == &self.requested_model
            || Some(model) == self.reported_model.as_ref()
            || self.same_served_models.contains(model)
    }

    /// Returns the protocol dialect identity.
    #[must_use]
    pub const fn protocol(&self) -> &ProtocolId {
        &self.protocol
    }

    /// Returns whether this is a standard or custom integration.
    #[must_use]
    pub const fn integration_class(&self) -> IntegrationClass {
        self.integration_class
    }

    /// Returns the immutable route identity.
    #[must_use]
    pub const fn route(&self) -> &RouteId {
        &self.route
    }

    /// Returns the requested model identity.
    #[must_use]
    pub const fn requested_model(&self) -> &ModelId {
        &self.requested_model
    }

    /// Returns the remotely reported model identity.
    #[must_use]
    pub const fn reported_model(&self) -> Option<&ModelId> {
        self.reported_model.as_ref()
    }

    /// Returns the optional model-family identity.
    #[must_use]
    pub const fn model_family(&self) -> Option<&ModelFamilyId> {
        self.model_family.as_ref()
    }

    /// Returns the frozen model-adapter / admission profile identity.
    #[must_use]
    pub const fn profile(&self) -> &ModelAdapterProfileId {
        &self.profile
    }

    /// Returns resolved capability facts.
    #[must_use]
    pub const fn capabilities(&self) -> &CapabilitySet {
        &self.capabilities
    }

    /// Returns the certified context contract when this descriptor can safely
    /// participate in context-managed admission.
    #[must_use]
    pub const fn context_contract(&self) -> Option<&ContextContract> {
        self.context_contract.as_ref()
    }

    pub(crate) fn validate_integration_class(&self) -> Result<(), ContextContractError> {
        if self.context_contract.as_ref().is_some_and(|contract| {
            !contract
                .capacity()
                .evidence_class()
                .allows_integration_class(self.integration_class)
        }) {
            return Err(ContextContractError::IntegrationClassMismatch);
        }
        Ok(())
    }

    /// Rejects a minimum evidence class policy using only the immutable descriptor.
    ///
    /// This preflight is safe before credential refresh or provider-visible
    /// preparation. Complete projection admission remains mandatory afterward.
    ///
    /// # Errors
    ///
    /// Returns unavailable when release-verified capacity was required but absent.
    pub fn preflight_context_admission_requirement(
        &self,
        requirement: ContextAdmissionRequirement,
    ) -> Result<(), ContextAdmissionError> {
        let actual = self
            .context_contract
            .as_ref()
            .map(|contract| contract.capacity().evidence_class().clone());
        if requirement == ContextAdmissionRequirement::ReleaseVerifiedRequired
            && !matches!(actual, Some(ContextEvidenceClass::ReleaseVerified))
        {
            return Err(ContextAdmissionError::Unavailable(
                ContextUnavailable::EvidenceRequirementNotMet {
                    required: requirement,
                    actual,
                },
            ));
        }
        Ok(())
    }

    /// Computes checked complete-projection context status.
    ///
    /// Capacity comes only from the contract frozen into this descriptor. The
    /// supplied observation is non-authoritative until its certificate,
    /// revision, exact scope, authority binding, and freshness all match.
    /// Missing or unproved facts produce typed unavailable status; this method
    /// never guesses capacity, tokenization, or an output reservation.
    ///
    /// # Errors
    ///
    /// Returns an error only when an immutable identity cannot be constructed.
    #[allow(
        clippy::too_many_lines,
        reason = "the ordered fail-closed descriptor admission procedure stays auditable in one place"
    )]
    pub fn context_status(
        &self,
        plan: &ValidatedRequestPlan,
        prepared: &PreparedModelProjection,
        now: Timestamp,
    ) -> Result<ContextStatus, ContextContractError> {
        let descriptor_binding = self.context_descriptor_binding()?;
        let request = plan.request();
        let model_input_identity = request.model_input_identity();
        if prepared.validate_for_plan(self, plan).is_err() {
            return ContextStatus::unavailable(
                descriptor_binding,
                model_input_identity,
                None,
                None,
                None,
                None,
                ContextUnavailable::ScopeDrift {
                    field: "prepared projection",
                },
            );
        }
        let Some(contract) = &self.context_contract else {
            return ContextStatus::unavailable(
                descriptor_binding,
                model_input_identity,
                None,
                None,
                None,
                None,
                ContextUnavailable::ContractUnavailable,
            );
        };
        if !contract
            .capacity()
            .evidence_class()
            .allows_integration_class(self.integration_class)
        {
            return ContextStatus::unavailable(
                descriptor_binding,
                model_input_identity,
                None,
                None,
                None,
                None,
                ContextUnavailable::ScopeDrift {
                    field: "integration class",
                },
            );
        }
        let effective_model = self
            .reported_model
            .clone()
            .unwrap_or_else(|| self.requested_model.clone());
        if prepared.protocol_family() != contract.protocol_family()
            || prepared.projection_revision() != contract.projection_revision()
            || prepared.accounting_revision() != contract.accounting_revision()
        {
            return ContextStatus::unavailable(
                descriptor_binding,
                model_input_identity,
                None,
                None,
                None,
                None,
                ContextUnavailable::ScopeDrift {
                    field: "prepared context contract",
                },
            );
        }
        let authority_binding = prepared.authority_class();
        let scope = ContextScope::new(
            prepared.protocol_family().clone(),
            self.protocol.clone(),
            self.route.clone(),
            authority_binding,
            self.requested_model.clone(),
            effective_model,
            self.same_served_models.clone(),
            self.profile.clone(),
            descriptor_binding,
            contract.identity(),
            prepared.lowering_revision().to_owned(),
            contract.accounting_revision().to_owned(),
            contract.projection_revision().to_owned(),
            model_input_identity,
            prepared.token_projection().identity(),
        )?;
        let capacity_certificate = contract.capacity();
        if capacity_certificate
            .authority_binding()
            .is_some_and(|expected| expected != authority_binding)
        {
            return ContextStatus::unavailable(
                descriptor_binding,
                model_input_identity,
                Some(scope),
                None,
                None,
                None,
                ContextUnavailable::ScopeDrift {
                    field: "capacity authority binding",
                },
            );
        }
        if let Some(unavailable) = capacity_certificate
            .freshness()
            .unavailable(now, "capacity")
        {
            return ContextStatus::unavailable(
                descriptor_binding,
                model_input_identity,
                Some(scope),
                None,
                None,
                None,
                unavailable,
            );
        }
        let capacity = ContextCapacity::admitted(scope.clone(), capacity_certificate)?;
        let reservation = if matches!(
            capacity_certificate.rule(),
            CapacityRule::ProviderReservedInput { .. }
        ) {
            if prepared.output_limit().is_some_and(|limit| {
                matches!(
                    limit.source(),
                    PreparedOutputLimitSource::ReleaseVerifiedHardCap { .. }
                )
            }) {
                return ContextStatus::unavailable(
                    descriptor_binding,
                    model_input_identity,
                    Some(scope),
                    Some(capacity),
                    None,
                    None,
                    ContextUnavailable::ScopeDrift {
                        field: "reference-client-envelope provider hard-cap authority",
                    },
                );
            }
            None
        } else {
            match OutputReservation::for_prepared(
                &scope,
                prepared,
                capacity_certificate,
                contract.reservation_revision(),
            ) {
                Ok(reservation) => Some(reservation),
                Err(unavailable) => {
                    return ContextStatus::unavailable(
                        descriptor_binding,
                        model_input_identity,
                        Some(scope),
                        Some(capacity),
                        None,
                        None,
                        unavailable,
                    );
                }
            }
        };
        let Some(observation) = prepared.token_projection().observation() else {
            return ContextStatus::unavailable(
                descriptor_binding,
                model_input_identity,
                Some(scope),
                Some(capacity),
                None,
                reservation,
                ContextUnavailable::MeasurementUnavailable,
            );
        };
        let Some(measurement_certificate) = contract.measurement(observation.certificate_id())
        else {
            return ContextStatus::unavailable(
                descriptor_binding,
                model_input_identity,
                Some(scope),
                Some(capacity),
                None,
                reservation,
                ContextUnavailable::UnknownMeasurementCertificate,
            );
        };
        if measurement_certificate.revision() != observation.certificate_revision() {
            return ContextStatus::unavailable(
                descriptor_binding,
                model_input_identity,
                Some(scope),
                Some(capacity),
                None,
                reservation,
                ContextUnavailable::MeasurementCertificateRevisionDrift,
            );
        }
        if measurement_certificate
            .authority_binding()
            .is_some_and(|expected| expected != authority_binding)
        {
            return ContextStatus::unavailable(
                descriptor_binding,
                model_input_identity,
                Some(scope),
                Some(capacity),
                None,
                reservation,
                ContextUnavailable::ScopeDrift {
                    field: "measurement authority binding",
                },
            );
        }
        if let Some(unavailable) = measurement_certificate
            .freshness()
            .unavailable(now, "measurement")
        {
            return ContextStatus::unavailable(
                descriptor_binding,
                model_input_identity,
                Some(scope),
                Some(capacity),
                None,
                reservation,
                unavailable,
            );
        }
        let measurement =
            ContextMeasurement::admitted(scope, measurement_certificate, observation.tokens())?;
        let status = ContextStatus::evaluate(capacity, measurement, reservation, request)?
            .with_semantic_summary_bound(u64::from(
                crate::semantic_compaction_output_bound(
                    request.limits().max_output_tokens(),
                    request.reasoning(),
                )
                .get(),
            ));
        let tokens = prepared.token_projection();
        let status = if let Some(input) = tokens.wire_input() {
            status.evaluate_wire_input(
                request,
                input,
                tokens.request_anchor(),
                self.semantic_binding_digest().map_err(|error| {
                    ContextContractError::DescriptorBinding {
                        reason: error.to_string(),
                    }
                })?,
                tokens.require_input_bound(),
            )?
        } else {
            status
        };
        Ok(status.with_input_parts(tokens.input_parts()))
    }

    /// Checks context and returns a closed whole-request admission witness.
    ///
    /// # Errors
    ///
    /// Returns typed unavailable or overflow denial, including checked
    /// arithmetic failure and protected-only overflow.
    pub fn admit_context(
        &self,
        plan: &ValidatedRequestPlan,
        prepared: &PreparedModelProjection,
        now: Timestamp,
    ) -> Result<ContextAdmission, ContextAdmissionError> {
        self.context_status(plan, prepared, now)?
            .admit(plan.request())
    }

    /// Checks context against a caller-selected minimum evidence class and returns
    /// a closed whole-request admission witness.
    ///
    /// # Errors
    ///
    /// Returns before dispatch when release-verified capacity was required but the
    /// exact descriptor carries only a reference-client-envelope operating envelope.
    pub fn admit_context_with_requirement(
        &self,
        plan: &ValidatedRequestPlan,
        prepared: &PreparedModelProjection,
        now: Timestamp,
        requirement: ContextAdmissionRequirement,
    ) -> Result<ContextAdmission, ContextAdmissionError> {
        self.context_status(plan, prepared, now)?
            .admit_with_requirement(plan.request(), requirement)
    }

    /// Revalidates a prior admission against the current exact descriptor,
    /// authority binding, request projection, certificate set, and time.
    ///
    /// # Errors
    ///
    /// Fails closed on descriptor/request drift, unknown or revised evidence,
    /// expiry, or any status that no longer fits.
    pub fn revalidate_context_admission(
        &self,
        plan: &ValidatedRequestPlan,
        prepared: &PreparedModelProjection,
        admission: &ContextAdmission,
        now: Timestamp,
    ) -> Result<(), ContextAdmissionError> {
        let descriptor_binding = self.context_descriptor_binding()?;
        if descriptor_binding != admission.status().descriptor_binding() {
            return Err(ContextAdmissionError::Unavailable(
                ContextUnavailable::ScopeDrift {
                    field: "descriptor binding",
                },
            ));
        }
        if admission.request() != plan.request()
            || admission.status().model_input_identity() != plan.request().model_input_identity()
        {
            return Err(ContextAdmissionError::Unavailable(
                ContextUnavailable::ScopeDrift {
                    field: "model input",
                },
            ));
        }
        let current = self.context_status(plan, prepared, now)?;
        match current.kind() {
            ContextStatusKind::Unavailable(unavailable) => {
                Err(ContextAdmissionError::Unavailable(unavailable.clone()))
            }
            ContextStatusKind::WouldOverflow(overflow) => {
                Err(ContextAdmissionError::WouldOverflow(*overflow))
            }
            ContextStatusKind::ProtectedOverflow(overflow) => {
                Err(ContextAdmissionError::ProtectedOverflow(*overflow))
            }
            ContextStatusKind::Fits { .. } => {
                let current_admission =
                    current.admit_with_requirement(plan.request(), admission.requirement())?;
                if current_admission.identity() == admission.identity() {
                    Ok(())
                } else {
                    Err(ContextAdmissionError::Unavailable(
                        ContextUnavailable::ScopeDrift {
                            field: "context admission",
                        },
                    ))
                }
            }
        }
    }

    fn context_descriptor_binding(&self) -> Result<Digest, ContextContractError> {
        self.binding_digest()
            .map_err(|error| ContextContractError::DescriptorBinding {
                reason: error.to_string(),
            })
    }

    /// Computes the immutable binding digest used to reject foreign plans.
    ///
    /// # Errors
    ///
    /// Returns an error when the descriptor cannot be serialized or digested.
    pub fn binding_digest(&self) -> Result<Digest, ValidationError> {
        let binding_bytes =
            serde_json::to_vec(self).map_err(|error| ValidationError::Serialization {
                reason: error.to_string(),
            })?;
        Digest::sha256(DigestDomain::ModelBinding, 1, &[&binding_bytes]).map_err(Into::into)
    }

    /// Computes the stable behavior-bearing descriptor binding.
    ///
    /// This binds protocol, route, requested/reported model, family, profile,
    /// capability support/constraints/source, and the context contract's
    /// semantic identity. Observation and expiry timestamps are excluded.
    /// [`Self::binding_digest`] remains the exact freshness-sensitive binding
    /// used for prepared plans and attempts.
    ///
    /// # Errors
    ///
    /// Returns an error when the semantic projection cannot be serialized or
    /// digested.
    pub fn semantic_binding_digest(&self) -> Result<Digest, ValidationError> {
        let features = self
            .capabilities
            .features
            .iter()
            .map(|(capability, fact)| (*capability, semantic_fact_support(fact)))
            .collect();
        let native_option_namespaces = self
            .capabilities
            .native_option_namespaces
            .iter()
            .map(|(namespace, fact)| (namespace, semantic_fact_support(fact)))
            .collect();
        let binding = SemanticDescriptorBinding {
            integration_class: self.integration_class,
            protocol: &self.protocol,
            route: &self.route,
            requested_model: &self.requested_model,
            reported_model: self.reported_model.as_ref(),
            same_served_models: &self.same_served_models,
            model_family: self.model_family.as_ref(),
            profile: &self.profile,
            features,
            native_option_namespaces,
            context_contract: self
                .context_contract
                .as_ref()
                .map(ContextContract::semantic_identity),
        };
        let binding_bytes =
            serde_json::to_vec(&binding).map_err(|error| ValidationError::Serialization {
                reason: error.to_string(),
            })?;
        Digest::sha256(DigestDomain::ModelBinding, 2, &[&binding_bytes]).map_err(Into::into)
    }

    /// Verifies that a prepared plan belongs to this exact resolved binding.
    ///
    /// # Errors
    ///
    /// Returns [`ValidationError::ForeignPlan`] when the plan was prepared by
    /// another protocol, route, model, profile, or capability resolution.
    pub fn validate_plan_binding(
        &self,
        plan: &ValidatedRequestPlan,
    ) -> Result<(), ValidationError> {
        if self.binding_digest()? == plan.binding_digest {
            Ok(())
        } else {
            Err(ValidationError::ForeignPlan)
        }
    }

    /// Verifies that a prepared plan is the exact admission witness for one
    /// submitted request and this resolved model binding.
    ///
    /// # Errors
    ///
    /// Returns [`ValidationError::ForeignPlan`] for another resolved binding,
    /// or [`ValidationError::SubstitutedRequest`] when preparation replaced
    /// any part of the submitted request snapshot.
    pub fn validate_plan_admission(
        &self,
        submitted_request: &ModelRequest,
        plan: &ValidatedRequestPlan,
    ) -> Result<(), ValidationError> {
        self.validate_plan_binding(plan)?;
        if plan.request() == submitted_request {
            Ok(())
        } else {
            Err(ValidationError::SubstitutedRequest)
        }
    }

    /// Validates and freezes an owned request under the selected mode.
    ///
    /// # Errors
    ///
    /// Returns an error when a required capability is unavailable, strict mode
    /// would change requested behavior, or the immutable plan cannot be
    /// integrity-digested.
    pub fn validate_request(
        &self,
        request: ModelRequest,
        mode: ValidationMode,
    ) -> Result<ValidatedRequestPlan, ValidationError> {
        let binding_digest = self.binding_digest()?;
        self.validate_request_for_binding(request, mode, binding_digest)
    }

    /// Validates `request` against this descriptor's capabilities but binds
    /// the plan to `binding_digest`, another observation of this same
    /// semantic descriptor. Only a prepared call record restoring its own
    /// plan may pass a binding other than [`Self::binding_digest`].
    pub(crate) fn validate_request_for_binding(
        &self,
        request: ModelRequest,
        mode: ValidationMode,
        binding_digest: Digest,
    ) -> Result<ValidatedRequestPlan, ValidationError> {
        let requests = requested_capabilities(&request);
        let mut decisions = Vec::with_capacity(requests.len());
        let mut diagnostics = Vec::new();

        for requested in requests {
            let support = self
                .capabilities
                .get(requested.capability)
                .map_or(Support::Unsupported, FeatureSupport::support);
            let outcome = decide(requested, support, mode)?;
            if !matches!(outcome, CapabilityOutcome::Native) {
                diagnostics.push(PlanDiagnostic::new(requested.capability, outcome));
            }
            decisions.push(CapabilityDecision {
                capability: requested.capability,
                requirement: requested.requirement,
                support,
                outcome,
            });
        }

        for namespace in request.native().entries().keys() {
            let support = self
                .capabilities
                .native_namespace(namespace)
                .map_or(Support::Unsupported, FeatureSupport::support);
            let requested = RequestedCapability {
                capability: Capability::NativeOptions,
                requirement: CapabilityRequirement::Required,
            };
            let outcome = decide(requested, support, mode)?;
            decisions.push(CapabilityDecision {
                capability: Capability::NativeOptions,
                requirement: CapabilityRequirement::Required,
                support,
                outcome,
            });
        }

        let request_bytes =
            serde_json::to_vec(&request).map_err(|error| ValidationError::Serialization {
                reason: error.to_string(),
            })?;
        let decision_bytes =
            serde_json::to_vec(&decisions).map_err(|error| ValidationError::Serialization {
                reason: error.to_string(),
            })?;
        let mode_bytes = match mode {
            ValidationMode::Strict => b"strict".as_slice(),
            ValidationMode::AllowDowngrade => b"allow-downgrade".as_slice(),
        };
        let digest = Digest::sha256(
            DigestDomain::RequestPlan,
            3,
            &[
                &request_bytes,
                binding_digest.bytes(),
                &decision_bytes,
                mode_bytes,
            ],
        )?;
        Ok(ValidatedRequestPlan {
            request: Arc::new(request),
            decisions: decisions.into(),
            diagnostics: diagnostics.into(),
            binding_digest,
            digest,
        })
    }
}

/// Strict validation or explicit preferred-feature downgrade mode.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ValidationMode {
    /// Reject unsupported and degraded requested behavior.
    #[default]
    Strict,
    /// Permit explicit downgrade or dropping of preferred behavior.
    AllowDowngrade,
}

/// The applied result of one capability decision.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum CapabilityOutcome {
    /// Direct support was selected.
    Native,
    /// Equivalent emulation was selected.
    Emulated,
    /// A documented degraded implementation was selected.
    Degraded,
    /// A preferred unsupported feature was dropped.
    Dropped,
}

/// One inspectable capability decision in a validated request plan.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CapabilityDecision {
    capability: Capability,
    requirement: CapabilityRequirement,
    support: Support,
    outcome: CapabilityOutcome,
}

impl CapabilityDecision {
    /// Returns the requested capability.
    #[must_use]
    pub const fn capability(self) -> Capability {
        self.capability
    }

    /// Returns whether the capability was required or preferred.
    #[must_use]
    pub const fn requirement(self) -> CapabilityRequirement {
        self.requirement
    }

    /// Returns resolved support.
    #[must_use]
    pub const fn support(self) -> Support {
        self.support
    }

    /// Returns the applied decision.
    #[must_use]
    pub const fn outcome(self) -> CapabilityOutcome {
        self.outcome
    }
}

/// An ordered, non-secret preparation diagnostic.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PlanDiagnostic {
    code: String,
    message: String,
}

impl PlanDiagnostic {
    fn new(capability: Capability, outcome: CapabilityOutcome) -> Self {
        Self {
            code: format!("capability.{capability:?}.{outcome:?}").to_lowercase(),
            message: format!("{capability:?} resolved as {outcome:?}"),
        }
    }

    /// Returns the stable diagnostic code.
    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    /// Returns the human-readable message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// An immutable request snapshot and every decision that affects dispatch.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ValidatedRequestPlan {
    request: Arc<ModelRequest>,
    decisions: Arc<[CapabilityDecision]>,
    diagnostics: Arc<[PlanDiagnostic]>,
    binding_digest: Digest,
    digest: Digest,
}

impl ValidatedRequestPlan {
    /// Returns the exact owned request snapshot.
    #[must_use]
    pub fn request(&self) -> &ModelRequest {
        &self.request
    }

    /// Returns ordered capability decisions.
    #[must_use]
    pub fn decisions(&self) -> &[CapabilityDecision] {
        &self.decisions
    }

    /// Returns ordered downgrade and emulation diagnostics.
    #[must_use]
    pub fn diagnostics(&self) -> &[PlanDiagnostic] {
        &self.diagnostics
    }

    /// Returns the exact resolved-model binding digest.
    #[must_use]
    pub const fn binding_digest(&self) -> Digest {
        self.binding_digest
    }

    /// Returns the digest of all request-affecting inputs.
    #[must_use]
    pub const fn digest(&self) -> Digest {
        self.digest
    }
}

/// A typed request validation failure.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum ValidationError {
    /// A plan was prepared for another resolved model binding.
    #[error("validated request plan belongs to another resolved model binding")]
    ForeignPlan,
    /// A prepared plan does not retain the exact submitted request.
    #[error("validated request plan substituted the submitted model request")]
    SubstitutedRequest,
    /// A required feature was unsupported or degraded.
    #[error("required capability {capability:?} has support {support:?}")]
    RequiredCapability {
        /// The requested feature.
        capability: Capability,
        /// The resolved support level.
        support: Support,
    },
    /// Strict mode rejected a preferred downgrade.
    #[error(
        "strict validation rejected preferred capability {capability:?} with support {support:?}"
    )]
    StrictPreference {
        /// The requested feature.
        capability: Capability,
        /// The resolved support level.
        support: Support,
    },
    /// A plan input could not be serialized for integrity hashing.
    #[error("request plan serialization failed: {reason}")]
    Serialization {
        /// The serialization failure.
        reason: String,
    },
    /// A digest could not be computed.
    #[error(transparent)]
    Digest(#[from] DigestError),
}

#[derive(Clone, Copy)]
struct RequestedCapability {
    capability: Capability,
    requirement: CapabilityRequirement,
}

fn requested_capabilities(request: &ModelRequest) -> Vec<RequestedCapability> {
    let mut requested = vec![RequestedCapability {
        capability: Capability::Streaming,
        requirement: CapabilityRequirement::Required,
    }];
    if !request.tools().is_empty() {
        requested.push(RequestedCapability {
            capability: Capability::Tools,
            requirement: CapabilityRequirement::Required,
        });
    }
    if request
        .hosted_tools()
        .contains(&crate::HostedTool::CodeInterpreter)
    {
        requested.push(RequestedCapability {
            capability: Capability::HostedCodeExecution,
            requirement: CapabilityRequirement::Required,
        });
    }
    if request
        .hosted_tools()
        .contains(&crate::HostedTool::ProgrammaticComposition)
    {
        requested.push(RequestedCapability {
            capability: Capability::HostedProgrammaticToolCalling,
            requirement: CapabilityRequirement::Required,
        });
    }
    if let ReasoningPolicy::Enabled { requirement, .. } = request.reasoning() {
        requested.push(RequestedCapability {
            capability: Capability::Reasoning,
            requirement,
        });
    }
    if matches!(request.output().format(), OutputFormat::Structured(_)) {
        requested.push(RequestedCapability {
            capability: Capability::StructuredOutput,
            requirement: request.output().requirement(),
        });
    }
    if let CachePolicy::Enabled { requirement } = request.cache() {
        requested.push(RequestedCapability {
            capability: Capability::Caching,
            requirement,
        });
    }
    requested
}

fn decide(
    requested: RequestedCapability,
    support: Support,
    mode: ValidationMode,
) -> Result<CapabilityOutcome, ValidationError> {
    match support {
        Support::Native => Ok(CapabilityOutcome::Native),
        Support::Emulated => Ok(CapabilityOutcome::Emulated),
        Support::Degraded | Support::Unsupported
            if requested.requirement == CapabilityRequirement::Required =>
        {
            Err(ValidationError::RequiredCapability {
                capability: requested.capability,
                support,
            })
        }
        Support::Degraded | Support::Unsupported if mode == ValidationMode::Strict => {
            Err(ValidationError::StrictPreference {
                capability: requested.capability,
                support,
            })
        }
        Support::Degraded => Ok(CapabilityOutcome::Degraded),
        Support::Unsupported => Ok(CapabilityOutcome::Dropped),
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::{
        AttemptId, ConversationView, EffectiveProviderConfigurationId, IdentityError,
        RequestMetadata, RunId, SelectedProviderConfigurationId, Timestamp, TurnId,
    };
    use uuid::Uuid;

    fn descriptor(reasoning: Support) -> Result<ResolvedModelDescriptor, IdentityError> {
        let timestamp = Timestamp::new(1, 0).map_err(|_| IdentityError::InvalidNanoseconds)?;
        let provenance = FactProvenance::new(FactSource::TestFixture, timestamp, None);
        let mut features = BTreeMap::new();
        features.insert(
            Capability::Streaming,
            FeatureSupport::new(
                Support::Native,
                CapabilityConstraints::default(),
                provenance,
            ),
        );
        features.insert(
            Capability::Reasoning,
            FeatureSupport::new(reasoning, CapabilityConstraints::default(), provenance),
        );
        Ok(ResolvedModelDescriptor::new(
            &StandardIntegrationAuthority::release_owned(),
            ProtocolId::new("test-protocol")?,
            RouteId::new("test-route")?,
            ModelId::new("test-model")?,
            None,
            None,
            ModelAdapterProfileId::new("test-profile")?,
            CapabilitySet::new(features, BTreeMap::new()),
        ))
    }

    fn request(
        requirement: CapabilityRequirement,
    ) -> Result<ModelRequest, Box<dyn std::error::Error>> {
        let metadata = RequestMetadata::for_turn(
            RunId::try_from(Uuid::from_u128(1))?,
            TurnId::try_from(Uuid::from_u128(2))?,
            AttemptId::try_from(Uuid::from_u128(3))?,
            Timestamp::new(1, 0)?,
        );
        let selected = SelectedProviderConfigurationId::new(Digest::sha256(
            DigestDomain::SelectedProviderConfiguration,
            5,
            &[b"descriptor-test-selected"],
        )?)?;
        let effective = EffectiveProviderConfigurationId::new(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"descriptor-test-effective"],
        )?)?;
        Ok(
            ModelRequest::builder(selected, effective, ConversationView::default(), metadata)
                .reasoning(ReasoningPolicy::Enabled {
                    effort: Some(crate::ReasoningEffort::Low),
                    requirement,
                })
                .build()?,
        )
    }

    #[test]
    fn strict_rejects_preferred_downgrade() -> Result<(), Box<dyn std::error::Error>> {
        let error = descriptor(Support::Unsupported)?
            .validate_request(
                request(CapabilityRequirement::Preferred)?,
                ValidationMode::Strict,
            )
            .err();
        assert!(matches!(
            error,
            Some(ValidationError::StrictPreference { .. })
        ));
        Ok(())
    }

    #[test]
    fn explicit_mode_records_ordered_downgrade() -> Result<(), Box<dyn std::error::Error>> {
        let plan = descriptor(Support::Unsupported)?.validate_request(
            request(CapabilityRequirement::Preferred)?,
            ValidationMode::AllowDowngrade,
        )?;
        assert_eq!(plan.decisions()[1].outcome(), CapabilityOutcome::Dropped);
        assert_eq!(plan.diagnostics().len(), 1);
        assert_eq!(plan.digest().domain(), DigestDomain::RequestPlan);
        Ok(())
    }

    #[test]
    fn required_is_never_dropped() -> Result<(), Box<dyn std::error::Error>> {
        let error = descriptor(Support::Unsupported)?
            .validate_request(
                request(CapabilityRequirement::Required)?,
                ValidationMode::AllowDowngrade,
            )
            .err();
        assert!(matches!(
            error,
            Some(ValidationError::RequiredCapability { .. })
        ));
        Ok(())
    }

    #[test]
    fn same_served_models_bind_only_when_declared() -> Result<(), Box<dyn std::error::Error>> {
        let plain = descriptor(Support::Native)?;
        let encoded = serde_json::to_value(&plain)?;
        assert!(encoded.get("same_served_models").is_none());
        let unchanged = plain.clone().with_same_served_models([]);
        assert_eq!(
            unchanged.semantic_binding_digest()?,
            plain.semantic_binding_digest()?
        );

        let declared = plain.clone().with_same_served_models([
            ModelId::new("served-b")?,
            ModelId::new("served-a")?,
            ModelId::new("served-b")?,
        ]);
        assert_eq!(
            declared
                .same_served_models()
                .iter()
                .map(ModelId::as_str)
                .collect::<Vec<_>>(),
            ["served-a", "served-b"]
        );
        assert_ne!(
            declared.semantic_binding_digest()?,
            plain.semantic_binding_digest()?
        );
        assert!(declared.serves_same_model(&ModelId::new("served-a")?));
        assert!(declared.serves_same_model(plain.requested_model()));
        assert!(!declared.serves_same_model(&ModelId::new("served-c")?));
        assert!(!plain.serves_same_model(&ModelId::new("served-a")?));

        let mut encoded = serde_json::to_value(&declared)?;
        assert_eq!(
            serde_json::from_value::<ResolvedModelDescriptor>(encoded.clone())?,
            declared
        );
        encoded["same_served_models"] = serde_json::json!(["served-b", "served-a"]);
        assert!(serde_json::from_value::<ResolvedModelDescriptor>(encoded).is_err());
        Ok(())
    }

    #[test]
    fn foreign_model_binding_rejects_prepared_plan() -> Result<(), Box<dyn std::error::Error>> {
        let first = descriptor(Support::Native)?;
        let plan = first.validate_request(
            request(CapabilityRequirement::Preferred)?,
            ValidationMode::Strict,
        )?;
        let second = ResolvedModelDescriptor::new(
            &StandardIntegrationAuthority::release_owned(),
            first.protocol().clone(),
            RouteId::new("other-route")?,
            first.requested_model().clone(),
            first.reported_model().cloned(),
            first.model_family().cloned(),
            first.profile().clone(),
            first.capabilities().clone(),
        );
        assert_eq!(
            second.validate_plan_binding(&plan),
            Err(ValidationError::ForeignPlan)
        );
        Ok(())
    }

    #[test]
    fn semantic_binding_ignores_fact_freshness_but_binds_behavior()
    -> Result<(), Box<dyn std::error::Error>> {
        let original = descriptor(Support::Native)?;
        let mut refreshed_features = original.capabilities().features().clone();
        for fact in refreshed_features.values_mut() {
            *fact = FeatureSupport::new(
                fact.support(),
                fact.constraints().clone(),
                FactProvenance::new(
                    FactSource::TestFixture,
                    Timestamp::new(20, 0)?,
                    Some(Timestamp::new(30, 0)?),
                ),
            );
        }
        let refreshed = ResolvedModelDescriptor::new(
            &StandardIntegrationAuthority::release_owned(),
            original.protocol().clone(),
            original.route().clone(),
            original.requested_model().clone(),
            original.reported_model().cloned(),
            original.model_family().cloned(),
            original.profile().clone(),
            CapabilitySet::new(refreshed_features.clone(), BTreeMap::new()),
        );
        assert_ne!(original.binding_digest()?, refreshed.binding_digest()?);
        assert_eq!(
            original.semantic_binding_digest()?,
            refreshed.semantic_binding_digest()?
        );

        let reasoning = refreshed_features
            .get_mut(&Capability::Reasoning)
            .expect("reasoning fact");
        *reasoning = FeatureSupport::new(
            Support::Degraded,
            reasoning.constraints().clone(),
            reasoning.provenance(),
        );
        let changed = ResolvedModelDescriptor::new(
            &StandardIntegrationAuthority::release_owned(),
            original.protocol().clone(),
            original.route().clone(),
            original.requested_model().clone(),
            original.reported_model().cloned(),
            original.model_family().cloned(),
            original.profile().clone(),
            CapabilitySet::new(refreshed_features, BTreeMap::new()),
        );
        assert_ne!(
            original.semantic_binding_digest()?,
            changed.semantic_binding_digest()?
        );
        Ok(())
    }

    #[test]
    fn exact_admission_rejects_substituted_request() -> Result<(), Box<dyn std::error::Error>> {
        let descriptor = descriptor(Support::Native)?;
        let submitted = request(CapabilityRequirement::Preferred)?;
        let substituted = request(CapabilityRequirement::Required)?;
        let plan = descriptor.validate_request(substituted, ValidationMode::Strict)?;
        assert_eq!(
            descriptor.validate_plan_admission(&submitted, &plan),
            Err(ValidationError::SubstitutedRequest)
        );
        Ok(())
    }
}
