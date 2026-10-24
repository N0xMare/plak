//! Scope, capacity, counts, measurement, output reservation and status kinds.

use serde::{Deserialize, Deserializer, Serialize};

use crate::{
    Digest, DigestDomain, ModelAdapterProfileId, ModelId, PreparedModelProjection,
    PreparedOutputLimitSource, ProtocolId, RouteId,
};

use super::{
    CapacityCertificate, CapacityRule, ContextContractError, ContextEvidenceClass,
    ContextEvidenceSource, ContextNonGuaranteeDisposition, ContextOverflow, CountCertificateKind,
    EvidenceFreshness, MeasurementCertificate, SafeLimit, semantic_digest, validate_label,
};

/// Exact binding and projection scope used by every context fact.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ContextScope {
    protocol_family: ProtocolId,
    protocol: ProtocolId,
    route: RouteId,
    authority_binding: Digest,
    requested_model: ModelId,
    effective_model: ModelId,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    same_served_models: Vec<ModelId>,
    adapter: ModelAdapterProfileId,
    descriptor_binding: Digest,
    context_contract: Digest,
    lowering_revision: String,
    accounting_revision: String,
    projection_revision: String,
    model_input_identity: Digest,
    prepared_token_projection: Digest,
    identity: Digest,
}

#[derive(Serialize)]
struct ContextScopeContent<'a> {
    protocol_family: &'a ProtocolId,
    protocol: &'a ProtocolId,
    route: &'a RouteId,
    authority_binding: Digest,
    requested_model: &'a ModelId,
    effective_model: &'a ModelId,
    #[serde(skip_serializing_if = "<[ModelId]>::is_empty")]
    same_served_models: &'a [ModelId],
    adapter: &'a ModelAdapterProfileId,
    descriptor_binding: Digest,
    context_contract: Digest,
    lowering_revision: &'a str,
    accounting_revision: &'a str,
    projection_revision: &'a str,
    model_input_identity: Digest,
    prepared_token_projection: Digest,
}

impl ContextScope {
    #[allow(
        clippy::too_many_arguments,
        reason = "the arguments are the exact binding/projection scope"
    )]
    pub(crate) fn new(
        protocol_family: ProtocolId,
        protocol: ProtocolId,
        route: RouteId,
        authority_binding: Digest,
        requested_model: ModelId,
        effective_model: ModelId,
        same_served_models: Vec<ModelId>,
        adapter: ModelAdapterProfileId,
        descriptor_binding: Digest,
        context_contract: Digest,
        lowering_revision: String,
        accounting_revision: String,
        projection_revision: String,
        model_input_identity: Digest,
        prepared_token_projection: Digest,
    ) -> Result<Self, ContextContractError> {
        let content = ContextScopeContent {
            protocol_family: &protocol_family,
            protocol: &protocol,
            route: &route,
            authority_binding,
            requested_model: &requested_model,
            effective_model: &effective_model,
            same_served_models: &same_served_models,
            adapter: &adapter,
            descriptor_binding,
            context_contract,
            lowering_revision: &lowering_revision,
            accounting_revision: &accounting_revision,
            projection_revision: &projection_revision,
            model_input_identity,
            prepared_token_projection,
        };
        let identity = semantic_digest(DigestDomain::ContextBudget, 5, &content)?;
        Ok(Self {
            protocol_family,
            protocol,
            route,
            authority_binding,
            requested_model,
            effective_model,
            same_served_models,
            adapter,
            descriptor_binding,
            context_contract,
            lowering_revision,
            accounting_revision,
            projection_revision,
            model_input_identity,
            prepared_token_projection,
            identity,
        })
    }

    /// Returns the protocol-family identity.
    #[must_use]
    pub const fn protocol_family(&self) -> &ProtocolId {
        &self.protocol_family
    }

    /// Returns the exact protocol/dialect identity.
    #[must_use]
    pub const fn protocol(&self) -> &ProtocolId {
        &self.protocol
    }

    /// Returns the exact route identity.
    #[must_use]
    pub const fn route(&self) -> &RouteId {
        &self.route
    }

    /// Returns the non-secret account/credential-authority binding digest.
    #[must_use]
    pub const fn authority_binding(&self) -> Digest {
        self.authority_binding
    }

    /// Returns the requested model identity.
    #[must_use]
    pub const fn requested_model(&self) -> &ModelId {
        &self.requested_model
    }

    /// Returns the effective model identity.
    #[must_use]
    pub const fn effective_model(&self) -> &ModelId {
        &self.effective_model
    }

    /// Whether a provider-reported served model is this scope's model: the
    /// requested or effective id, or a descriptor-declared same served model.
    #[must_use]
    pub fn serves_same_model(&self, model: &ModelId) -> bool {
        model == &self.requested_model
            || model == &self.effective_model
            || self.same_served_models.contains(model)
    }

    /// Returns the model-adapter profile identity.
    #[must_use]
    pub const fn adapter(&self) -> &ModelAdapterProfileId {
        &self.adapter
    }

    /// Returns the complete resolved-descriptor binding.
    #[must_use]
    pub const fn descriptor_binding(&self) -> Digest {
        self.descriptor_binding
    }

    /// Returns the descriptor-owned context-contract identity.
    #[must_use]
    pub const fn context_contract_identity(&self) -> Digest {
        self.context_contract
    }

    /// Returns the exact provider lowering revision.
    #[must_use]
    pub fn lowering_revision(&self) -> &str {
        &self.lowering_revision
    }

    /// Returns the context-accounting revision.
    #[must_use]
    pub fn accounting_revision(&self) -> &str {
        &self.accounting_revision
    }

    /// Returns the model-projection revision.
    #[must_use]
    pub fn projection_revision(&self) -> &str {
        &self.projection_revision
    }

    /// Returns the exact complete model-input projection identity.
    #[must_use]
    pub const fn model_input_identity(&self) -> Digest {
        self.model_input_identity
    }

    /// Returns the exact complete prepared-token-projection identity.
    #[must_use]
    pub const fn prepared_token_projection(&self) -> Digest {
        self.prepared_token_projection
    }

    /// Returns the complete scope identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
}

/// Descriptor-admitted capacity evidence for one exact scope.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ContextCapacity {
    scope: ContextScope,
    rule: CapacityRule,
    evidence_class: ContextEvidenceClass,
    non_guarantee_disposition: Option<ContextNonGuaranteeDisposition>,
    evidence_id: String,
    evidence_revision: String,
    evidence_source: ContextEvidenceSource,
    freshness: EvidenceFreshness,
    certificate_identity: Digest,
    identity: Digest,
}

#[derive(Serialize)]
struct ContextCapacityContent<'a> {
    scope: Digest,
    rule: CapacityRule,
    evidence_class: &'a ContextEvidenceClass,
    non_guarantee_disposition: Option<ContextNonGuaranteeDisposition>,
    evidence_id: &'a str,
    evidence_revision: &'a str,
    evidence_source: ContextEvidenceSource,
    freshness: EvidenceFreshness,
    certificate_identity: Digest,
}

impl ContextCapacity {
    pub(crate) fn admitted(
        scope: ContextScope,
        certificate: &CapacityCertificate,
    ) -> Result<Self, ContextContractError> {
        let content = ContextCapacityContent {
            scope: scope.identity(),
            rule: certificate.rule(),
            evidence_class: certificate.evidence_class(),
            non_guarantee_disposition: certificate.non_guarantee_disposition(),
            evidence_id: certificate.evidence_id(),
            evidence_revision: certificate.revision(),
            evidence_source: certificate.source(),
            freshness: certificate.freshness(),
            certificate_identity: certificate.identity(),
        };
        let identity = semantic_digest(DigestDomain::ContextBudget, 7, &content)?;
        Ok(Self {
            scope,
            rule: certificate.rule(),
            evidence_class: certificate.evidence_class().clone(),
            non_guarantee_disposition: certificate.non_guarantee_disposition(),
            evidence_id: certificate.evidence_id().to_owned(),
            evidence_revision: certificate.revision().to_owned(),
            evidence_source: certificate.source(),
            freshness: certificate.freshness(),
            certificate_identity: certificate.identity(),
            identity,
        })
    }

    /// Returns the exact binding/projection scope.
    #[must_use]
    pub const fn scope(&self) -> &ContextScope {
        &self.scope
    }

    /// Returns the release-verified capacity rule.
    #[must_use]
    pub const fn rule(&self) -> CapacityRule {
        self.rule
    }

    /// Returns the non-coercible evidence class carried into public status
    /// and admission receipts.
    #[must_use]
    pub const fn evidence_class(&self) -> &ContextEvidenceClass {
        &self.evidence_class
    }

    /// Returns the mandatory warning when this is a reference-client-envelope envelope.
    #[must_use]
    pub const fn non_guarantee_disposition(&self) -> Option<ContextNonGuaranteeDisposition> {
        self.non_guarantee_disposition
    }

    /// Returns the evidence identifier.
    #[must_use]
    pub fn evidence_id(&self) -> &str {
        &self.evidence_id
    }

    /// Returns the evidence revision.
    #[must_use]
    pub fn evidence_revision(&self) -> &str {
        &self.evidence_revision
    }

    /// Returns the evidence source.
    #[must_use]
    pub const fn evidence_source(&self) -> ContextEvidenceSource {
        self.evidence_source
    }

    /// Returns evidence freshness.
    #[must_use]
    pub const fn freshness(&self) -> EvidenceFreshness {
        self.freshness
    }

    /// Returns the admitted capacity identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
}

/// A complete-projection token bound.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub enum CountBound {
    /// The complete model input contains exactly this many tokens.
    Exact(u64),
    /// The complete model input contains at most this many tokens under the
    /// named release-verified proof corpus.
    ReleaseVerifiedUpperBound {
        /// Conservative inclusive upper bound.
        tokens: u64,
        /// Identity of the admitted measurement certificate.
        certificate: Digest,
        /// Identity of the release-owned proof corpus.
        corpus: Digest,
    },
    /// Conservative bound under a reference-client operating envelope; this
    /// is not a provider capacity guarantee.
    ReferenceClientEnvelopeUpperBound {
        /// Conservative inclusive upper bound.
        tokens: u64,
        /// Exact authenticated catalog/reference receipt.
        catalog_receipt: Digest,
    },
    /// Conservative bound declared by the caller. This grants no
    /// provider capacity evidence.
    CustomEnvelopeUpperBound {
        /// Conservative inclusive upper bound.
        tokens: u64,
        /// Immutable caller configuration/provenance identity.
        declaration: Digest,
    },
}

impl CountBound {
    /// Returns the value safe for admission.
    #[must_use]
    pub const fn upper_bound(self) -> u64 {
        match self {
            Self::Exact(tokens)
            | Self::ReleaseVerifiedUpperBound { tokens, .. }
            | Self::ReferenceClientEnvelopeUpperBound { tokens, .. }
            | Self::CustomEnvelopeUpperBound { tokens, .. } => tokens,
        }
    }

    /// Reports whether the count is exact.
    #[must_use]
    pub const fn is_exact(self) -> bool {
        matches!(self, Self::Exact(_))
    }
}

/// A non-authoritative count supplied by a model-owned measurement facet.
///
/// Descriptor admission resolves the named certificate, scope, revision, and
/// freshness before this count can authorize dispatch.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CountObservation {
    certificate_id: String,
    certificate_revision: String,
    tokens: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CountObservationWire {
    certificate_id: String,
    certificate_revision: String,
    tokens: u64,
}

impl<'de> Deserialize<'de> for CountObservation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = CountObservationWire::deserialize(deserializer)?;
        Self::new(wire.certificate_id, wire.certificate_revision, wire.tokens)
            .map_err(serde::de::Error::custom)
    }
}

impl CountObservation {
    /// Creates a count candidate naming one descriptor-owned certificate.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid certificate labels.
    pub fn new(
        certificate_id: impl Into<String>,
        certificate_revision: impl Into<String>,
        tokens: u64,
    ) -> Result<Self, ContextContractError> {
        let certificate_id = certificate_id.into();
        let certificate_revision = certificate_revision.into();
        validate_label(&certificate_id, "measurement certificate ID")?;
        validate_label(&certificate_revision, "measurement certificate revision")?;
        Ok(Self {
            certificate_id,
            certificate_revision,
            tokens,
        })
    }

    /// Returns the named certificate ID.
    #[must_use]
    pub fn certificate_id(&self) -> &str {
        &self.certificate_id
    }

    /// Returns the named certificate revision.
    #[must_use]
    pub fn certificate_revision(&self) -> &str {
        &self.certificate_revision
    }

    /// Returns the observed count or upper bound.
    #[must_use]
    pub const fn tokens(&self) -> u64 {
        self.tokens
    }
}

/// Descriptor-admitted input measurement for one exact projection.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ContextMeasurement {
    scope: ContextScope,
    method: String,
    method_revision: String,
    bound: CountBound,
    certificate_id: String,
    certificate_revision: String,
    evidence_source: ContextEvidenceSource,
    freshness: EvidenceFreshness,
    certificate_identity: Digest,
    identity: Digest,
}

#[derive(Serialize)]
struct ContextMeasurementContent<'a> {
    scope: Digest,
    method: &'a str,
    method_revision: &'a str,
    bound: CountBound,
    certificate_id: &'a str,
    certificate_revision: &'a str,
    evidence_source: ContextEvidenceSource,
    freshness: EvidenceFreshness,
    certificate_identity: Digest,
}

impl ContextMeasurement {
    pub(crate) fn admitted(
        scope: ContextScope,
        certificate: &MeasurementCertificate,
        tokens: u64,
    ) -> Result<Self, ContextContractError> {
        let bound = match certificate.kind() {
            CountCertificateKind::ExactTokenizer
            | CountCertificateKind::ExactProviderCounter
            | CountCertificateKind::ExactProviderUsage => CountBound::Exact(tokens),
            CountCertificateKind::ReleaseVerifiedUpperBound { corpus } => {
                CountBound::ReleaseVerifiedUpperBound {
                    tokens,
                    certificate: certificate.identity(),
                    corpus,
                }
            }
            CountCertificateKind::ReferenceClientEnvelopeUpperBound { catalog_receipt } => {
                CountBound::ReferenceClientEnvelopeUpperBound {
                    tokens,
                    catalog_receipt,
                }
            }
            CountCertificateKind::CustomEnvelopeUpperBound { declaration } => {
                CountBound::CustomEnvelopeUpperBound {
                    tokens,
                    declaration,
                }
            }
        };
        let content = ContextMeasurementContent {
            scope: scope.identity(),
            method: certificate.method(),
            method_revision: certificate.method_revision(),
            bound,
            certificate_id: certificate.certificate_id(),
            certificate_revision: certificate.revision(),
            evidence_source: certificate.source(),
            freshness: certificate.freshness(),
            certificate_identity: certificate.identity(),
        };
        let identity = semantic_digest(DigestDomain::TokenMeasurement, 4, &content)?;
        Ok(Self {
            scope,
            method: certificate.method().to_owned(),
            method_revision: certificate.method_revision().to_owned(),
            bound,
            certificate_id: certificate.certificate_id().to_owned(),
            certificate_revision: certificate.revision().to_owned(),
            evidence_source: certificate.source(),
            freshness: certificate.freshness(),
            certificate_identity: certificate.identity(),
            identity,
        })
    }

    /// Returns the exact binding/projection scope.
    #[must_use]
    pub const fn scope(&self) -> &ContextScope {
        &self.scope
    }

    /// Returns the exact measurement method.
    #[must_use]
    pub fn method(&self) -> &str {
        &self.method
    }

    /// Returns the behavior-bearing method revision.
    #[must_use]
    pub fn method_revision(&self) -> &str {
        &self.method_revision
    }

    /// Returns the exact or certified conservative count bound.
    #[must_use]
    pub const fn bound(&self) -> CountBound {
        self.bound
    }

    /// Returns the admitted certificate ID.
    #[must_use]
    pub fn certificate_id(&self) -> &str {
        &self.certificate_id
    }

    /// Returns the admitted certificate revision.
    #[must_use]
    pub fn certificate_revision(&self) -> &str {
        &self.certificate_revision
    }

    /// Returns the admitted evidence source.
    #[must_use]
    pub const fn evidence_source(&self) -> ContextEvidenceSource {
        self.evidence_source
    }

    /// Returns evidence freshness.
    #[must_use]
    pub const fn freshness(&self) -> EvidenceFreshness {
        self.freshness
    }

    /// Returns the descriptor-owned certificate identity.
    #[must_use]
    pub const fn certificate_identity(&self) -> Digest {
        self.certificate_identity
    }

    /// Returns the admitted measurement identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
}

/// Provider-proved output capacity reserved by the exact prepared request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct OutputReservation {
    scope: Digest,
    tokens: u64,
    revision: String,
    prepared_output_limit: Digest,
    identity: Digest,
}

#[derive(Serialize)]
struct OutputReservationContent<'a> {
    scope: Digest,
    tokens: u64,
    revision: &'a str,
    prepared_output_limit: Digest,
}

impl OutputReservation {
    pub(super) const fn scope_identity(&self) -> Digest {
        self.scope
    }

    pub(crate) fn for_prepared(
        scope: &ContextScope,
        projection: &PreparedModelProjection,
        capacity: &CapacityCertificate,
        revision: &str,
    ) -> Result<Self, ContextUnavailable> {
        if scope.model_input_identity != projection.model_input()
            || scope.prepared_token_projection != projection.token_projection().identity()
            || scope.lowering_revision != projection.lowering_revision()
        {
            return Err(ContextUnavailable::ScopeDrift {
                field: "prepared output scope",
            });
        }
        let Some(limit) = projection.output_limit() else {
            return Err(ContextUnavailable::ExplicitOutputReservationUnavailable);
        };
        let tokens = u64::from(limit.tokens().get());
        if let PreparedOutputLimitSource::ReleaseVerifiedHardCap { certificate, .. } =
            limit.source()
        {
            let release_verified_tokens = match capacity.rule() {
                CapacityRule::Separate {
                    output: SafeLimit::Exact(tokens),
                    ..
                }
                | CapacityRule::SharedWindowWithOutputCap {
                    output: SafeLimit::Exact(tokens),
                    ..
                } => Some(tokens),
                CapacityRule::ProviderReservedInput { .. }
                | CapacityRule::SharedWindow { .. }
                | CapacityRule::Separate { .. }
                | CapacityRule::SharedWindowWithOutputCap { .. } => None,
            };
            if *certificate != capacity.identity() || release_verified_tokens != Some(tokens) {
                return Err(ContextUnavailable::ScopeDrift {
                    field: "release-verified output hard cap",
                });
            }
        }
        let content = OutputReservationContent {
            scope: scope.identity(),
            tokens,
            revision,
            prepared_output_limit: limit.identity(),
        };
        let identity = semantic_digest(DigestDomain::ContextBudget, 7, &content)
            .map_err(|_| ContextUnavailable::IdentityUnavailable)?;
        Ok(Self {
            scope: scope.identity(),
            tokens,
            revision: revision.to_owned(),
            prepared_output_limit: limit.identity(),
            identity,
        })
    }

    /// Returns reserved output tokens.
    #[must_use]
    pub const fn tokens(&self) -> u64 {
        self.tokens
    }

    /// Returns the reservation/accounting revision.
    #[must_use]
    pub fn revision(&self) -> &str {
        &self.revision
    }

    /// Returns the exact provider-owned output-limit proof identity.
    #[must_use]
    pub const fn prepared_output_limit(&self) -> Digest {
        self.prepared_output_limit
    }

    /// Returns the reservation identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
}

/// Qualification attached to a safe headroom value.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub enum QualifiedTokens {
    /// Exact unused capacity.
    Exact(u64),
    /// Unused capacity is guaranteed to be at least this value.
    GuaranteedMinimum(u64),
}

impl QualifiedTokens {
    /// Returns the value safe to display and plan against.
    #[must_use]
    pub const fn value(self) -> u64 {
        match self {
            Self::Exact(value) | Self::GuaranteedMinimum(value) => value,
        }
    }
}

/// Qualified headroom under the selected capacity rule.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ContextHeadroom {
    /// Remaining input inside a provider/reference-client reserved envelope;
    /// no output capacity or provider hard-cap is claimed.
    ProviderReservedInput {
        /// Qualified remaining input envelope.
        input: QualifiedTokens,
    },
    /// Remaining shared input-plus-output window.
    Shared(QualifiedTokens),
    /// Independently qualified input and output headroom.
    Separate {
        /// Remaining input capacity.
        input: QualifiedTokens,
        /// Remaining output capacity.
        output: QualifiedTokens,
    },
    /// Remaining shared window and independent output cap.
    SharedWithOutputCap {
        /// Remaining shared capacity.
        shared: QualifiedTokens,
        /// Remaining output capacity.
        output: QualifiedTokens,
    },
}

/// Why context status could not be established safely.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ContextUnavailable {
    /// The resolved descriptor has no certified context contract.
    ContractUnavailable,
    /// An exact scope dimension drifted.
    ScopeDrift {
        /// The scope dimension that changed.
        field: &'static str,
    },
    /// A supplied certificate ID is not configured by the descriptor.
    UnknownMeasurementCertificate,
    /// A supplied certificate revision differs from the descriptor.
    MeasurementCertificateRevisionDrift,
    /// Evidence was used before its observation/release boundary.
    EvidenceNotYetValid {
        /// Capacity or measurement evidence.
        evidence: &'static str,
    },
    /// Evidence reached its exclusive expiry boundary.
    EvidenceExpired {
        /// Capacity or measurement evidence.
        evidence: &'static str,
    },
    /// No complete-projection count was available.
    MeasurementUnavailable,
    /// The request omitted a nonzero explicit output maximum.
    ExplicitOutputReservationUnavailable,
    /// Checked token arithmetic overflowed.
    ArithmeticOverflow,
    /// The request history could not satisfy the closed bounded planner policy.
    HistoryPlanningUnavailable,
    /// A status identity could not be constructed.
    IdentityUnavailable,
    /// The bounded evidence class did not meet the caller's minimum requirement.
    EvidenceRequirementNotMet {
        /// The closed caller requirement.
        required: ContextAdmissionRequirement,
        /// The actual evidence class, absent when capacity was unavailable.
        actual: Option<ContextEvidenceClass>,
    },
}

/// Checked status for one exact descriptor/request/authority scope.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ContextStatusKind {
    /// The safe upper bounds fit all safe lower bounds.
    Fits {
        /// Exact or conservatively qualified headroom.
        headroom: ContextHeadroom,
    },
    /// Capacity was exceeded and an eligible compactable range may exist.
    ///
    /// This classification promises only that complete eligible groups exist;
    /// it does not predict token savings or claim that a retained candidate fits.
    WouldOverflow(ContextOverflow),
    /// Capacity was exceeded but only protected material remains.
    ProtectedOverflow(ContextOverflow),
    /// Safe status could not be established.
    Unavailable(ContextUnavailable),
}

/// Caller-selected minimum evidence class for one context admission.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ContextAdmissionRequirement {
    /// Admit any honestly bounded release, reference-client, or custom envelope.
    #[default]
    AnyBounded,
    /// Admit only release-verified capacity guarantees.
    ReleaseVerifiedRequired,
}
