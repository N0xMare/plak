//! Provider-neutral, descriptor-bound context capacity and admission.

use std::collections::BTreeSet;

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::{
    ContextHistoryPlan, ContextPlannerDisposition, ContextPlannerDispositionKind, Digest,
    DigestDomain, DigestError, ModelAdapterProfileId, ModelId, ModelRequest,
    PreparedModelProjection, PreparedOutputLimitSource, ProtocolId, RouteId, Timestamp,
};

/// Conservative per-image default when a dialect has no tighter documented bound.
pub const CONSERVATIVE_IMAGE_MAX_TOKENS: u64 = 8192;

const MAX_CONTEXT_LABEL_BYTES: usize = 128;
const MAX_MEASUREMENT_CERTIFICATES: usize = 16;

/// A safe capacity limit.
///
/// Admission may use the numeric value in either case. A lower bound only
/// authorizes a qualified minimum headroom; it must never be presented as an
/// exact provider capacity.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub enum SafeLimit {
    /// The exact limit is known for the certified scope.
    Exact(u64),
    /// The real limit is guaranteed to be at least this value.
    GuaranteedLowerBound(u64),
}

impl SafeLimit {
    /// Returns the value safe for admission.
    #[must_use]
    pub const fn lower_bound(self) -> u64 {
        match self {
            Self::Exact(value) | Self::GuaranteedLowerBound(value) => value,
        }
    }

    /// Reports whether the limit is exact.
    #[must_use]
    pub const fn is_exact(self) -> bool {
        matches!(self, Self::Exact(_))
    }

    fn validate(self, field: &'static str) -> Result<(), ContextContractError> {
        if self.lower_bound() == 0 {
            Err(ContextContractError::ZeroLimit { field })
        } else {
            Ok(())
        }
    }
}

/// How a model accounts for input and output capacity.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub enum CapacityRule {
    /// A provider/reference client reserves an input-only operating envelope.
    ///
    /// This is not a provider capacity guarantee and grants no output bound.
    ProviderReservedInput {
        /// Raw model context window when reported by authenticated evidence.
        ///
        /// This is display metadata and the denominator for the reference
        /// envelope, not a provider hard-cap or capacity guarantee. Custom
        /// declarations which lack authenticated provider metadata use `None`.
        reported_context_window: Option<u64>,
        /// Maximum complete prepared input admitted by the reference envelope.
        maximum_projected_input: u64,
        /// Occupancy at which idle and mid-turn compact fire, when the
        /// reference client publishes a compact trigger distinct from admit.
        ///
        /// Grok OAuth's Grok Build row is 80% of the 500k window (400k): admit
        /// may use the window, compact is the trigger. `None` uses the
        /// ceiling-minus-headroom fence. Omitted on the wire for every other route.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auto_compact_input: Option<u64>,
    },
    /// Input and output consume one shared window.
    SharedWindow {
        /// Total input-plus-output capacity.
        total: SafeLimit,
    },
    /// Input and output have independently enforced capacities.
    Separate {
        /// Input capacity.
        input: SafeLimit,
        /// Output capacity.
        output: SafeLimit,
    },
    /// Input and output share a window and output also has its own cap.
    SharedWindowWithOutputCap {
        /// Total input-plus-output capacity.
        total: SafeLimit,
        /// Independent output cap.
        output: SafeLimit,
    },
}

impl CapacityRule {
    /// Occupancy that schedules idle and mid-turn compact, when the envelope
    /// publishes a trigger distinct from the admitted ceiling.
    #[must_use]
    pub const fn auto_compact_input(self) -> Option<u64> {
        match self {
            Self::ProviderReservedInput {
                auto_compact_input, ..
            } => auto_compact_input,
            _ => None,
        }
    }

    /// Input ceiling before any shared-window output reservation.
    #[must_use]
    pub const fn input_ceiling(self) -> u64 {
        match self {
            Self::ProviderReservedInput {
                maximum_projected_input,
                ..
            } => maximum_projected_input,
            Self::SharedWindow { total } | Self::SharedWindowWithOutputCap { total, .. } => {
                total.lower_bound()
            }
            Self::Separate { input, .. } => input.lower_bound(),
        }
    }

    /// Floor-aware trigger for the admitted input envelope, including fold output.
    #[must_use]
    pub fn compaction_trigger_tokens(
        self,
        reserved_output: u64,
        stable_prefix_tokens: u64,
        summary_tokens: u64,
    ) -> u64 {
        let reserved = match self {
            Self::SharedWindow { .. } | Self::SharedWindowWithOutputCap { .. } => reserved_output,
            _ => 0,
        };
        let ceiling = self.input_ceiling().saturating_sub(reserved);
        let headroom = summary_tokens
            .saturating_add(4096)
            .max(ceiling.div_ceil(10));
        let rule_trigger = self
            .auto_compact_input()
            .unwrap_or_else(|| ceiling.saturating_sub(headroom));
        rule_trigger
            .max(
                stable_prefix_tokens
                    .saturating_add(summary_tokens)
                    .saturating_add(4096),
            )
            .min(ceiling.saturating_sub(4096))
    }

    fn validate(self) -> Result<(), ContextContractError> {
        match self {
            Self::ProviderReservedInput {
                reported_context_window,
                maximum_projected_input,
                auto_compact_input,
            } => {
                if maximum_projected_input == 0 {
                    Err(ContextContractError::ZeroLimit {
                        field: "provider-reserved input",
                    })
                } else if reported_context_window
                    .is_some_and(|window| window < maximum_projected_input)
                    || auto_compact_input.is_some_and(|tokens| {
                        tokens == 0
                            || reported_context_window
                                .map_or(tokens > maximum_projected_input, |window| tokens > window)
                    })
                {
                    Err(ContextContractError::InvalidCapacityRelationship)
                } else {
                    Ok(())
                }
            }
            Self::SharedWindow { total } => total.validate("shared window"),
            Self::Separate { input, output } => {
                input.validate("input capacity")?;
                output.validate("output capacity")
            }
            Self::SharedWindowWithOutputCap { total, output } => {
                total.validate("shared window")?;
                output.validate("output capacity")
            }
        }
    }
}

/// Whether an integration is release-maintained or caller-supplied.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub enum IntegrationClass {
    /// A release-maintained integration with release-owned policy.
    Standard,
    /// A caller-supplied integration whose declarations grant no provider evidence.
    Custom,
}

/// Opaque authority to construct release-maintained integration facts.
///
/// Safe consumer code cannot create this value. Release-maintained provider
/// crates create it at their narrow construction boundary and pass it through
/// every API that can mint `Standard` integration or release-owned evidence.
#[derive(Clone, Debug)]
pub struct StandardIntegrationAuthority {
    _private: (),
}

impl StandardIntegrationAuthority {
    /// Returns the singleton authority owned by release integration plumbing.
    ///
    /// This low-level seam is intentionally omitted from the curated facade;
    /// its opaque value cannot be reconstructed from consumer data.
    #[doc(hidden)]
    #[must_use]
    pub const fn release_owned() -> Self {
        Self { _private: () }
    }
}

/// Closed evidence class for context admission.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ContextEvidenceClass {
    /// Release-owned exact-scope evidence satisfying the release contract.
    ReleaseVerified,
    /// An authenticated, revision-pinned reference-client operating envelope.
    /// This is explicitly not a provider hard-cap or capacity guarantee.
    ReferenceClientEnvelope {
        /// Exact authenticated catalog/reference receipt.
        catalog_receipt: Digest,
        /// Reference-client/API profile revision.
        reference_revision: String,
    },
    /// A bounded input envelope declared by the caller.
    ///
    /// This grants no provider capacity evidence or guarantee.
    CustomEnvelope {
        /// Immutable caller configuration/provenance identity.
        declaration: Digest,
        /// Revision of the declaration contract.
        declaration_revision: String,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
enum ContextEvidenceClassWire {
    ReleaseVerified,
    ReferenceClientEnvelope {
        catalog_receipt: Digest,
        reference_revision: String,
    },
    CustomEnvelope {
        declaration: Digest,
        declaration_revision: String,
    },
}

impl ContextEvidenceClass {
    /// Whether an implementation class may carry this capacity evidence.
    ///
    /// Maintained integrations may accept a weaker caller-declared envelope;
    /// caller implementations cannot acquire maintained evidence authority.
    #[must_use]
    pub const fn allows_integration_class(&self, integration: IntegrationClass) -> bool {
        match integration {
            IntegrationClass::Standard => true,
            IntegrationClass::Custom => matches!(self, Self::CustomEnvelope { .. }),
        }
    }
}

impl<'de> Deserialize<'de> for ContextEvidenceClass {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match ContextEvidenceClassWire::deserialize(deserializer)? {
            ContextEvidenceClassWire::ReleaseVerified => Ok(Self::ReleaseVerified),
            ContextEvidenceClassWire::ReferenceClientEnvelope {
                catalog_receipt,
                reference_revision,
            } => {
                validate_label(&reference_revision, "reference revision")
                    .map_err(serde::de::Error::custom)?;
                Ok(Self::ReferenceClientEnvelope {
                    catalog_receipt,
                    reference_revision,
                })
            }
            ContextEvidenceClassWire::CustomEnvelope {
                declaration,
                declaration_revision,
            } => {
                validate_label(&declaration_revision, "declaration revision")
                    .map_err(serde::de::Error::custom)?;
                Ok(Self::CustomEnvelope {
                    declaration,
                    declaration_revision,
                })
            }
        }
    }
}

/// A source allowed to contribute evidence to a resolved model descriptor.
///
/// These values become admission authority only after they are frozen into the
/// descriptor returned by the selected [`crate::LanguageModel`].
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ContextEvidenceSource {
    /// A reviewed, versioned built-in catalog row.
    BuiltInCatalog,
    /// Authenticated discovery scoped to one non-secret authority binding.
    AuthenticatedDiscovery,
    /// A release-owned certificate backed by a checked evidence corpus.
    ReleaseCertificate,
    /// A bounded capacity declaration supplied by the caller.
    CallerDeclaration,
}

/// Observation and optional expiry bounds for context evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct EvidenceFreshness {
    observed_at: Timestamp,
    expires_at: Option<Timestamp>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EvidenceFreshnessWire {
    observed_at: Timestamp,
    expires_at: Option<Timestamp>,
}

impl<'de> Deserialize<'de> for EvidenceFreshness {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = EvidenceFreshnessWire::deserialize(deserializer)?;
        Self::new(wire.observed_at, wire.expires_at).map_err(serde::de::Error::custom)
    }
}

/// Machine-readable warning carried by non-guaranteed operating envelopes.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ContextNonGuaranteeDisposition {
    /// The bound reproduces a reviewed reference-client envelope and is not a
    /// provider hard limit or capacity guarantee.
    ReferenceClientOperatingEnvelope,
    /// The bound is a custom caller declaration, not provider evidence.
    CustomCallerOperatingEnvelope,
    /// No credible anchor; the current provider decides a plausible estimate.
    UnanchoredProviderDecision,
}

impl EvidenceFreshness {
    /// Creates explicit evidence freshness.
    ///
    /// # Errors
    ///
    /// Returns an error when expiry does not follow observation.
    pub fn new(
        observed_at: Timestamp,
        expires_at: Option<Timestamp>,
    ) -> Result<Self, ContextContractError> {
        if expires_at.is_some_and(|expiry| expiry <= observed_at) {
            return Err(ContextContractError::InvalidFreshness);
        }
        Ok(Self {
            observed_at,
            expires_at,
        })
    }

    /// Returns when the evidence was observed or released.
    #[must_use]
    pub const fn observed_at(self) -> Timestamp {
        self.observed_at
    }

    /// Returns the exclusive expiry boundary, if any.
    #[must_use]
    pub const fn expires_at(self) -> Option<Timestamp> {
        self.expires_at
    }

    pub(crate) fn unavailable(
        self,
        now: Timestamp,
        evidence: &'static str,
    ) -> Option<ContextUnavailable> {
        if now < self.observed_at {
            Some(ContextUnavailable::EvidenceNotYetValid { evidence })
        } else if self.expires_at.is_some_and(|expiry| expiry <= now) {
            Some(ContextUnavailable::EvidenceExpired { evidence })
        } else {
            None
        }
    }
}

/// Capacity evidence configured by a model integration.
///
/// Constructing this value does not itself authorize dispatch. It becomes
/// authoritative only when included in a resolved descriptor and subsequently
/// matched at descriptor admission.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CapacityCertificate {
    evidence_id: String,
    revision: String,
    source: ContextEvidenceSource,
    rule: CapacityRule,
    authority_binding: Option<Digest>,
    freshness: EvidenceFreshness,
    evidence_class: ContextEvidenceClass,
    non_guarantee_disposition: Option<ContextNonGuaranteeDisposition>,
    identity: Digest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CapacityCertificateWire {
    evidence_id: String,
    revision: String,
    source: ContextEvidenceSource,
    rule: CapacityRule,
    authority_binding: Option<Digest>,
    freshness: EvidenceFreshness,
    evidence_class: ContextEvidenceClass,
    non_guarantee_disposition: Option<ContextNonGuaranteeDisposition>,
    identity: Digest,
}

#[derive(Serialize)]
struct CapacityCertificateContent<'a> {
    evidence_id: &'a str,
    revision: &'a str,
    source: ContextEvidenceSource,
    rule: CapacityRule,
    authority_binding: Option<Digest>,
    freshness: EvidenceFreshness,
    evidence_class: &'a ContextEvidenceClass,
    non_guarantee_disposition: Option<ContextNonGuaranteeDisposition>,
}

impl CapacityCertificate {
    /// Creates a versioned capacity certificate.
    ///
    /// Authenticated discovery must name the exact non-secret account or
    /// credential-authority binding and must expire.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid labels, limits, discovery scope, or digest
    /// construction.
    pub fn new(
        _authority: &StandardIntegrationAuthority,
        evidence_id: impl Into<String>,
        revision: impl Into<String>,
        source: ContextEvidenceSource,
        rule: CapacityRule,
        authority_binding: Option<Digest>,
        freshness: EvidenceFreshness,
    ) -> Result<Self, ContextContractError> {
        let evidence_id = evidence_id.into();
        let revision = revision.into();
        validate_label(&evidence_id, "capacity evidence ID")?;
        validate_label(&revision, "capacity evidence revision")?;
        rule.validate()?;
        if source == ContextEvidenceSource::AuthenticatedDiscovery
            && (authority_binding.is_none() || freshness.expires_at().is_none())
        {
            return Err(ContextContractError::UnscopedDiscovery);
        }
        Self::with_evidence_class(
            evidence_id,
            revision,
            source,
            rule,
            authority_binding,
            freshness,
            ContextEvidenceClass::ReleaseVerified,
            None,
        )
    }

    /// Creates authenticated reference-client-envelope input-envelope evidence.
    ///
    /// # Errors
    ///
    /// Returns an error when the labels, input-only rule, authenticated
    /// authority, finite freshness, or reconstructive identity is invalid.
    #[allow(
        clippy::too_many_arguments,
        reason = "the exact evidence receipt is reconstructive"
    )]
    pub fn reference_client_envelope(
        _authority: &StandardIntegrationAuthority,
        evidence_id: impl Into<String>,
        revision: impl Into<String>,
        rule: CapacityRule,
        authority_binding: Digest,
        freshness: EvidenceFreshness,
        catalog_receipt: Digest,
        reference_revision: impl Into<String>,
    ) -> Result<Self, ContextContractError> {
        let reference_revision = reference_revision.into();
        validate_label(&reference_revision, "reference revision")?;
        if !matches!(rule, CapacityRule::ProviderReservedInput { .. })
            || freshness.expires_at().is_none()
        {
            return Err(ContextContractError::InvalidReferenceClientEnvelopeEvidence);
        }
        Self::with_evidence_class(
            evidence_id.into(),
            revision.into(),
            ContextEvidenceSource::AuthenticatedDiscovery,
            rule,
            Some(authority_binding),
            freshness,
            ContextEvidenceClass::ReferenceClientEnvelope {
                catalog_receipt,
                reference_revision,
            },
            Some(ContextNonGuaranteeDisposition::ReferenceClientOperatingEnvelope),
        )
    }

    /// Creates a caller-declared input envelope without provider capacity authority.
    ///
    /// # Errors
    ///
    /// Returns an error unless the declaration is input-only, exactly bound,
    /// revisioned, and reconstructive.
    #[allow(
        clippy::too_many_arguments,
        reason = "the custom declaration binds every reconstructive evidence axis"
    )]
    pub fn custom_envelope(
        evidence_id: impl Into<String>,
        revision: impl Into<String>,
        rule: CapacityRule,
        authority_binding: Digest,
        freshness: EvidenceFreshness,
        declaration: Digest,
        declaration_revision: impl Into<String>,
    ) -> Result<Self, ContextContractError> {
        let declaration_revision = declaration_revision.into();
        validate_label(&declaration_revision, "declaration revision")?;
        if !matches!(rule, CapacityRule::ProviderReservedInput { .. }) {
            return Err(ContextContractError::InvalidCustomEnvelopeEvidence);
        }
        Self::with_evidence_class(
            evidence_id.into(),
            revision.into(),
            ContextEvidenceSource::CallerDeclaration,
            rule,
            Some(authority_binding),
            freshness,
            ContextEvidenceClass::CustomEnvelope {
                declaration,
                declaration_revision,
            },
            Some(ContextNonGuaranteeDisposition::CustomCallerOperatingEnvelope),
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the private constructor hashes every reconstructive evidence axis"
    )]
    fn with_evidence_class(
        evidence_id: String,
        revision: String,
        source: ContextEvidenceSource,
        rule: CapacityRule,
        authority_binding: Option<Digest>,
        freshness: EvidenceFreshness,
        evidence_class: ContextEvidenceClass,
        non_guarantee_disposition: Option<ContextNonGuaranteeDisposition>,
    ) -> Result<Self, ContextContractError> {
        validate_label(&evidence_id, "capacity evidence ID")?;
        validate_label(&revision, "capacity evidence revision")?;
        rule.validate()?;
        validate_evidence_authority(source, authority_binding, freshness)?;
        match &evidence_class {
            ContextEvidenceClass::ReleaseVerified => {
                if matches!(rule, CapacityRule::ProviderReservedInput { .. })
                    || non_guarantee_disposition.is_some()
                {
                    return Err(ContextContractError::EvidenceClassMismatch);
                }
            }
            ContextEvidenceClass::ReferenceClientEnvelope {
                reference_revision, ..
            } => {
                validate_label(reference_revision, "reference revision")?;
                if source != ContextEvidenceSource::AuthenticatedDiscovery
                    || !matches!(rule, CapacityRule::ProviderReservedInput { .. })
                    || authority_binding.is_none()
                    || freshness.expires_at().is_none()
                    || non_guarantee_disposition
                        != Some(ContextNonGuaranteeDisposition::ReferenceClientOperatingEnvelope)
                {
                    return Err(ContextContractError::InvalidReferenceClientEnvelopeEvidence);
                }
            }
            ContextEvidenceClass::CustomEnvelope {
                declaration_revision,
                ..
            } => {
                validate_label(declaration_revision, "declaration revision")?;
                if source != ContextEvidenceSource::CallerDeclaration
                    || !matches!(rule, CapacityRule::ProviderReservedInput { .. })
                    || authority_binding.is_none()
                    || non_guarantee_disposition
                        != Some(ContextNonGuaranteeDisposition::CustomCallerOperatingEnvelope)
                {
                    return Err(ContextContractError::InvalidCustomEnvelopeEvidence);
                }
            }
        }
        let content = CapacityCertificateContent {
            evidence_id: &evidence_id,
            revision: &revision,
            source,
            rule,
            authority_binding,
            freshness,
            evidence_class: &evidence_class,
            non_guarantee_disposition,
        };
        let identity = semantic_digest(DigestDomain::ContextBudget, 3, &content)?;
        Ok(Self {
            evidence_id,
            revision,
            source,
            rule,
            authority_binding,
            freshness,
            evidence_class,
            non_guarantee_disposition,
            identity,
        })
    }

    /// Returns the stable evidence identifier.
    #[must_use]
    pub fn evidence_id(&self) -> &str {
        &self.evidence_id
    }

    /// Returns the evidence revision.
    #[must_use]
    pub fn revision(&self) -> &str {
        &self.revision
    }

    /// Returns the evidence source.
    #[must_use]
    pub const fn source(&self) -> ContextEvidenceSource {
        self.source
    }

    /// Returns the release-verified accounting rule.
    #[must_use]
    pub const fn rule(&self) -> CapacityRule {
        self.rule
    }

    /// Returns an exact authority binding required by this evidence.
    #[must_use]
    pub const fn authority_binding(&self) -> Option<Digest> {
        self.authority_binding
    }

    /// Returns evidence freshness.
    #[must_use]
    pub const fn freshness(&self) -> EvidenceFreshness {
        self.freshness
    }

    /// Returns the closed evidence class; reference-client-envelope is never certified.
    #[must_use]
    pub const fn evidence_class(&self) -> &ContextEvidenceClass {
        &self.evidence_class
    }

    /// Returns the mandatory warning for a reference-client-envelope envelope.
    #[must_use]
    pub const fn non_guarantee_disposition(&self) -> Option<ContextNonGuaranteeDisposition> {
        self.non_guarantee_disposition
    }

    /// Returns the certificate identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
}

impl<'de> Deserialize<'de> for CapacityCertificate {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = CapacityCertificateWire::deserialize(deserializer)?;
        let certificate = Self::with_evidence_class(
            wire.evidence_id,
            wire.revision,
            wire.source,
            wire.rule,
            wire.authority_binding,
            wire.freshness,
            wire.evidence_class,
            wire.non_guarantee_disposition,
        )
        .map_err(serde::de::Error::custom)?;
        if certificate.identity != wire.identity {
            return Err(serde::de::Error::custom(
                ContextContractError::IntegrityMismatch {
                    field: "capacity certificate",
                },
            ));
        }
        Ok(certificate)
    }
}

/// The certified behavior of one token-counting method.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub enum CountCertificateKind {
    /// An exact tokenizer over the complete family-lowered projection.
    ExactTokenizer,
    /// An exact provider count endpoint for the complete projection.
    ExactProviderCounter,
    /// Provider-reported usage for this identical already-bound projection.
    ExactProviderUsage,
    /// A conservative algorithm certified by a release-owned proof corpus.
    ReleaseVerifiedUpperBound {
        /// Immutable identity of the conformance/proof corpus.
        corpus: Digest,
    },
    /// A conservative complete-prepared-projection observation pinned to an
    /// authenticated reference-client receipt.
    ReferenceClientEnvelopeUpperBound {
        /// Exact authenticated catalog/reference receipt.
        catalog_receipt: Digest,
    },
    /// A conservative complete-projection bound paired with a custom caller
    /// declaration. This is not provider evidence.
    CustomEnvelopeUpperBound {
        /// Immutable caller configuration/provenance identity.
        declaration: Digest,
    },
}

/// A release-owned certificate for one complete-projection count method.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MeasurementCertificate {
    certificate_id: String,
    revision: String,
    method: String,
    method_revision: String,
    source: ContextEvidenceSource,
    kind: CountCertificateKind,
    authority_binding: Option<Digest>,
    freshness: EvidenceFreshness,
    identity: Digest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MeasurementCertificateWire {
    certificate_id: String,
    revision: String,
    method: String,
    method_revision: String,
    source: ContextEvidenceSource,
    kind: CountCertificateKind,
    authority_binding: Option<Digest>,
    freshness: EvidenceFreshness,
    identity: Digest,
}

#[derive(Serialize)]
struct MeasurementCertificateContent<'a> {
    certificate_id: &'a str,
    revision: &'a str,
    method: &'a str,
    method_revision: &'a str,
    source: ContextEvidenceSource,
    kind: CountCertificateKind,
    authority_binding: Option<Digest>,
    freshness: EvidenceFreshness,
}

impl MeasurementCertificate {
    /// Creates a complete-projection measurement certificate.
    ///
    /// A conservative upper-bound certificate must be release-owned.
    /// Authenticated discovery is exact-authority-scoped and expiring.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid labels, evidence/source mismatch,
    /// unscoped discovery, or digest construction.
    #[allow(
        clippy::too_many_arguments,
        reason = "the arguments are the reconstruction-complete certificate"
    )]
    pub fn new(
        certificate_id: impl Into<String>,
        revision: impl Into<String>,
        method: impl Into<String>,
        method_revision: impl Into<String>,
        source: ContextEvidenceSource,
        kind: CountCertificateKind,
        authority_binding: Option<Digest>,
        freshness: EvidenceFreshness,
    ) -> Result<Self, ContextContractError> {
        if !matches!(kind, CountCertificateKind::CustomEnvelopeUpperBound { .. }) {
            return Err(ContextContractError::StandardAuthorityRequired);
        }
        Self::build(
            certificate_id,
            revision,
            method,
            method_revision,
            source,
            kind,
            authority_binding,
            freshness,
        )
    }

    /// Creates release-owned or standard reference-client measurement evidence.
    ///
    /// # Errors
    ///
    /// Rejects custom evidence kinds and invalid or inconsistent certificate facts.
    #[allow(
        clippy::too_many_arguments,
        reason = "the arguments are the reconstruction-complete certificate"
    )]
    pub fn standard(
        _authority: &StandardIntegrationAuthority,
        certificate_id: impl Into<String>,
        revision: impl Into<String>,
        method: impl Into<String>,
        method_revision: impl Into<String>,
        source: ContextEvidenceSource,
        kind: CountCertificateKind,
        authority_binding: Option<Digest>,
        freshness: EvidenceFreshness,
    ) -> Result<Self, ContextContractError> {
        if matches!(kind, CountCertificateKind::CustomEnvelopeUpperBound { .. }) {
            return Err(ContextContractError::IntegrationClassMismatch);
        }
        Self::build(
            certificate_id,
            revision,
            method,
            method_revision,
            source,
            kind,
            authority_binding,
            freshness,
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the arguments are the reconstruction-complete certificate"
    )]
    fn build(
        certificate_id: impl Into<String>,
        revision: impl Into<String>,
        method: impl Into<String>,
        method_revision: impl Into<String>,
        source: ContextEvidenceSource,
        kind: CountCertificateKind,
        authority_binding: Option<Digest>,
        freshness: EvidenceFreshness,
    ) -> Result<Self, ContextContractError> {
        let certificate_id = certificate_id.into();
        let revision = revision.into();
        let method = method.into();
        let method_revision = method_revision.into();
        validate_label(&certificate_id, "measurement certificate ID")?;
        validate_label(&revision, "measurement certificate revision")?;
        validate_label(&method, "measurement method")?;
        validate_label(&method_revision, "measurement method revision")?;
        validate_evidence_authority(source, authority_binding, freshness)?;
        if matches!(kind, CountCertificateKind::ReleaseVerifiedUpperBound { .. })
            && source != ContextEvidenceSource::ReleaseCertificate
        {
            return Err(ContextContractError::UpperBoundNotReleaseVerified);
        }
        if matches!(
            kind,
            CountCertificateKind::ReferenceClientEnvelopeUpperBound { .. }
        ) && source != ContextEvidenceSource::AuthenticatedDiscovery
        {
            return Err(ContextContractError::InvalidReferenceClientEnvelopeEvidence);
        }
        if matches!(kind, CountCertificateKind::CustomEnvelopeUpperBound { .. })
            && source != ContextEvidenceSource::CallerDeclaration
        {
            return Err(ContextContractError::InvalidCustomEnvelopeEvidence);
        }
        let content = MeasurementCertificateContent {
            certificate_id: &certificate_id,
            revision: &revision,
            method: &method,
            method_revision: &method_revision,
            source,
            kind,
            authority_binding,
            freshness,
        };
        let identity = semantic_digest(DigestDomain::TokenMeasurement, 3, &content)?;
        Ok(Self {
            certificate_id,
            revision,
            method,
            method_revision,
            source,
            kind,
            authority_binding,
            freshness,
            identity,
        })
    }

    /// Returns the stable certificate identifier.
    #[must_use]
    pub fn certificate_id(&self) -> &str {
        &self.certificate_id
    }

    /// Returns the certificate revision.
    #[must_use]
    pub fn revision(&self) -> &str {
        &self.revision
    }

    /// Returns the count algorithm or endpoint name.
    #[must_use]
    pub fn method(&self) -> &str {
        &self.method
    }

    /// Returns the behavior-bearing count-method revision.
    #[must_use]
    pub fn method_revision(&self) -> &str {
        &self.method_revision
    }

    /// Returns the certificate source.
    #[must_use]
    pub const fn source(&self) -> ContextEvidenceSource {
        self.source
    }

    /// Returns the release-verified count behavior.
    #[must_use]
    pub const fn kind(&self) -> CountCertificateKind {
        self.kind
    }

    /// Returns an exact authority binding required by this certificate.
    #[must_use]
    pub const fn authority_binding(&self) -> Option<Digest> {
        self.authority_binding
    }

    /// Returns certificate freshness.
    #[must_use]
    pub const fn freshness(&self) -> EvidenceFreshness {
        self.freshness
    }

    /// Returns the certificate identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
}

impl<'de> Deserialize<'de> for MeasurementCertificate {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = MeasurementCertificateWire::deserialize(deserializer)?;
        let certificate = Self::build(
            wire.certificate_id,
            wire.revision,
            wire.method,
            wire.method_revision,
            wire.source,
            wire.kind,
            wire.authority_binding,
            wire.freshness,
        )
        .map_err(serde::de::Error::custom)?;
        if certificate.identity != wire.identity {
            return Err(serde::de::Error::custom(
                ContextContractError::IntegrityMismatch {
                    field: "measurement certificate",
                },
            ));
        }
        Ok(certificate)
    }
}

/// A complete revisioned context contract frozen into a model descriptor.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ContextContract {
    protocol_family: ProtocolId,
    accounting_revision: String,
    projection_revision: String,
    reservation_revision: String,
    capacity: CapacityCertificate,
    measurements: Vec<MeasurementCertificate>,
    identity: Digest,
    #[serde(skip)]
    semantic_identity: Digest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextContractWire {
    protocol_family: ProtocolId,
    accounting_revision: String,
    projection_revision: String,
    reservation_revision: String,
    capacity: CapacityCertificate,
    measurements: Vec<MeasurementCertificate>,
    identity: Digest,
}

#[derive(Serialize)]
struct ContextContractContent<'a> {
    protocol_family: &'a ProtocolId,
    accounting_revision: &'a str,
    projection_revision: &'a str,
    reservation_revision: &'a str,
    capacity: &'a CapacityCertificate,
    measurements: &'a [MeasurementCertificate],
}

#[derive(Serialize)]
enum SemanticContextEvidenceClass<'a> {
    ReleaseVerified,
    ReferenceClientEnvelope {
        reference_revision: &'a str,
    },
    CustomEnvelope {
        declaration: Digest,
        declaration_revision: &'a str,
    },
}

#[derive(Serialize)]
enum SemanticCountCertificateKind {
    ExactTokenizer,
    ExactProviderCounter,
    ExactProviderUsage,
    ReleaseVerifiedUpperBound { corpus: Digest },
    ReferenceClientEnvelopeUpperBound,
    CustomEnvelopeUpperBound,
}

#[derive(Serialize)]
struct SemanticCapacityCertificate<'a> {
    evidence_id: &'a str,
    revision: &'a str,
    source: ContextEvidenceSource,
    rule: CapacityRule,
    authority_binding: Option<Digest>,
    evidence_class: SemanticContextEvidenceClass<'a>,
    non_guarantee_disposition: Option<ContextNonGuaranteeDisposition>,
}

#[derive(Serialize)]
struct SemanticMeasurementCertificate<'a> {
    certificate_id: &'a str,
    revision: &'a str,
    method: &'a str,
    method_revision: &'a str,
    source: ContextEvidenceSource,
    kind: SemanticCountCertificateKind,
    authority_binding: Option<Digest>,
}

#[derive(Serialize)]
struct SemanticContextContractContent<'a> {
    protocol_family: &'a ProtocolId,
    accounting_revision: &'a str,
    projection_revision: &'a str,
    reservation_revision: &'a str,
    capacity: SemanticCapacityCertificate<'a>,
    measurements: Vec<SemanticMeasurementCertificate<'a>>,
}

impl ContextContract {
    /// Creates a descriptor-owned context contract.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid revisions, duplicate/oversized certificate
    /// sets, or digest construction.
    pub fn new(
        protocol_family: ProtocolId,
        accounting_revision: impl Into<String>,
        projection_revision: impl Into<String>,
        reservation_revision: impl Into<String>,
        capacity: CapacityCertificate,
        measurements: Vec<MeasurementCertificate>,
    ) -> Result<Self, ContextContractError> {
        let accounting_revision = accounting_revision.into();
        let projection_revision = projection_revision.into();
        let reservation_revision = reservation_revision.into();
        validate_label(&accounting_revision, "context accounting revision")?;
        validate_label(&projection_revision, "model projection revision")?;
        validate_label(&reservation_revision, "output reservation revision")?;
        if measurements.len() > MAX_MEASUREMENT_CERTIFICATES {
            return Err(ContextContractError::TooManyMeasurementCertificates);
        }
        let mut certificate_ids = BTreeSet::new();
        for certificate in &measurements {
            if !certificate_ids.insert(certificate.certificate_id()) {
                return Err(ContextContractError::DuplicateMeasurementCertificate);
            }
        }
        match capacity.evidence_class() {
            ContextEvidenceClass::ReleaseVerified => {
                if matches!(capacity.rule(), CapacityRule::ProviderReservedInput { .. })
                    || measurements.iter().any(|measurement| {
                        matches!(
                            measurement.kind(),
                            CountCertificateKind::ReferenceClientEnvelopeUpperBound { .. }
                                | CountCertificateKind::CustomEnvelopeUpperBound { .. }
                        )
                    })
                {
                    return Err(ContextContractError::EvidenceClassMismatch);
                }
            }
            ContextEvidenceClass::ReferenceClientEnvelope {
                catalog_receipt, ..
            } => {
                if measurements.is_empty()
                    || !matches!(capacity.rule(), CapacityRule::ProviderReservedInput { .. })
                    || measurements.iter().any(|measurement| {
                        !matches!(
                            measurement.kind(),
                            CountCertificateKind::ReferenceClientEnvelopeUpperBound {
                                catalog_receipt: receipt
                            } if receipt == *catalog_receipt
                        )
                    })
                {
                    return Err(ContextContractError::EvidenceClassMismatch);
                }
            }
            ContextEvidenceClass::CustomEnvelope { declaration, .. } => {
                if measurements.is_empty()
                    || !matches!(capacity.rule(), CapacityRule::ProviderReservedInput { .. })
                    || measurements.iter().any(|measurement| {
                        !matches!(
                            measurement.kind(),
                            CountCertificateKind::CustomEnvelopeUpperBound {
                                declaration: measurement_declaration
                            } if measurement_declaration == *declaration
                        )
                    })
                {
                    return Err(ContextContractError::EvidenceClassMismatch);
                }
            }
        }
        let content = ContextContractContent {
            protocol_family: &protocol_family,
            accounting_revision: &accounting_revision,
            projection_revision: &projection_revision,
            reservation_revision: &reservation_revision,
            capacity: &capacity,
            measurements: &measurements,
        };
        let identity = semantic_digest(DigestDomain::ContextBudget, 4, &content)?;
        let semantic_identity = context_contract_semantic_identity(
            &protocol_family,
            &accounting_revision,
            &projection_revision,
            &reservation_revision,
            &capacity,
            &measurements,
        )?;
        Ok(Self {
            protocol_family,
            accounting_revision,
            projection_revision,
            reservation_revision,
            capacity,
            measurements,
            identity,
            semantic_identity,
        })
    }

    /// Returns the protocol-family identity.
    #[must_use]
    pub const fn protocol_family(&self) -> &ProtocolId {
        &self.protocol_family
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

    /// Returns the explicit-output-reservation revision.
    #[must_use]
    pub fn reservation_revision(&self) -> &str {
        &self.reservation_revision
    }

    /// Returns configured capacity evidence.
    #[must_use]
    pub const fn capacity(&self) -> &CapacityCertificate {
        &self.capacity
    }

    /// Returns allowed complete-projection measurement certificates.
    #[must_use]
    pub fn measurements(&self) -> &[MeasurementCertificate] {
        &self.measurements
    }

    /// Returns the complete contract identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }

    /// Returns the stable behavior-bearing contract identity.
    ///
    /// Observation/expiry timestamps and authenticated catalog receipt values
    /// are intentionally excluded. Revisions, rules, methods, evidence
    /// sources, authority bindings, and evidence class remain bound.
    #[must_use]
    pub const fn semantic_identity(&self) -> Digest {
        self.semantic_identity
    }

    pub(crate) fn measurement(&self, certificate_id: &str) -> Option<&MeasurementCertificate> {
        self.measurements
            .iter()
            .find(|certificate| certificate.certificate_id == certificate_id)
    }
}

fn context_contract_semantic_identity(
    protocol_family: &ProtocolId,
    accounting_revision: &str,
    projection_revision: &str,
    reservation_revision: &str,
    capacity: &CapacityCertificate,
    measurements: &[MeasurementCertificate],
) -> Result<Digest, ContextContractError> {
    let evidence_class = match capacity.evidence_class() {
        ContextEvidenceClass::ReleaseVerified => SemanticContextEvidenceClass::ReleaseVerified,
        ContextEvidenceClass::ReferenceClientEnvelope {
            reference_revision, ..
        } => SemanticContextEvidenceClass::ReferenceClientEnvelope { reference_revision },
        ContextEvidenceClass::CustomEnvelope {
            declaration,
            declaration_revision,
        } => SemanticContextEvidenceClass::CustomEnvelope {
            declaration: *declaration,
            declaration_revision,
        },
    };
    let capacity = SemanticCapacityCertificate {
        evidence_id: capacity.evidence_id(),
        revision: capacity.revision(),
        source: capacity.source(),
        rule: capacity.rule(),
        authority_binding: capacity.authority_binding(),
        evidence_class,
        non_guarantee_disposition: capacity.non_guarantee_disposition(),
    };
    let measurements = measurements
        .iter()
        .map(|measurement| SemanticMeasurementCertificate {
            certificate_id: measurement.certificate_id(),
            revision: measurement.revision(),
            method: measurement.method(),
            method_revision: measurement.method_revision(),
            source: measurement.source(),
            kind: match measurement.kind() {
                CountCertificateKind::ExactTokenizer => {
                    SemanticCountCertificateKind::ExactTokenizer
                }
                CountCertificateKind::ExactProviderCounter => {
                    SemanticCountCertificateKind::ExactProviderCounter
                }
                CountCertificateKind::ExactProviderUsage => {
                    SemanticCountCertificateKind::ExactProviderUsage
                }
                CountCertificateKind::ReleaseVerifiedUpperBound { corpus } => {
                    SemanticCountCertificateKind::ReleaseVerifiedUpperBound { corpus }
                }
                CountCertificateKind::ReferenceClientEnvelopeUpperBound { .. } => {
                    SemanticCountCertificateKind::ReferenceClientEnvelopeUpperBound
                }
                CountCertificateKind::CustomEnvelopeUpperBound { .. } => {
                    SemanticCountCertificateKind::CustomEnvelopeUpperBound
                }
            },
            authority_binding: measurement.authority_binding(),
        })
        .collect();
    semantic_digest(
        DigestDomain::ContextBudget,
        8,
        &SemanticContextContractContent {
            protocol_family,
            accounting_revision,
            projection_revision,
            reservation_revision,
            capacity,
            measurements,
        },
    )
}

impl<'de> Deserialize<'de> for ContextContract {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ContextContractWire::deserialize(deserializer)?;
        let contract = Self::new(
            wire.protocol_family,
            wire.accounting_revision,
            wire.projection_revision,
            wire.reservation_revision,
            wire.capacity,
            wire.measurements,
        )
        .map_err(serde::de::Error::custom)?;
        if contract.identity != wire.identity {
            return Err(serde::de::Error::custom(
                ContextContractError::IntegrityMismatch {
                    field: "context contract",
                },
            ));
        }
        Ok(contract)
    }
}

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

/// One overrun on a specific capacity axis.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct LimitOverflow {
    required: u64,
    available: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LimitOverflowWire {
    required: u64,
    available: u64,
}

impl<'de> Deserialize<'de> for LimitOverflow {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = LimitOverflowWire::deserialize(deserializer)?;
        if wire.required <= wire.available {
            return Err(serde::de::Error::custom(
                "context overflow must exceed available capacity",
            ));
        }
        Ok(Self {
            required: wire.required,
            available: wire.available,
        })
    }
}

impl LimitOverflow {
    /// Returns the safe upper bound that was required.
    #[must_use]
    pub const fn required(self) -> u64 {
        self.required
    }

    /// Returns the safe lower bound that was available.
    #[must_use]
    pub const fn available(self) -> u64 {
        self.available
    }
}

/// Typed context overflow across shared, input, and/or output axes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ContextOverflow {
    shared: Option<LimitOverflow>,
    input: Option<LimitOverflow>,
    output: Option<LimitOverflow>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextOverflowWire {
    shared: Option<LimitOverflow>,
    input: Option<LimitOverflow>,
    output: Option<LimitOverflow>,
}

impl<'de> Deserialize<'de> for ContextOverflow {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ContextOverflowWire::deserialize(deserializer)?;
        if wire.shared.is_none() && wire.input.is_none() && wire.output.is_none() {
            return Err(serde::de::Error::custom(
                "context overflow must name at least one capacity axis",
            ));
        }
        Ok(Self {
            shared: wire.shared,
            input: wire.input,
            output: wire.output,
        })
    }
}

impl ContextOverflow {
    /// Returns shared-window overflow when present.
    #[must_use]
    pub const fn shared(self) -> Option<LimitOverflow> {
        self.shared
    }

    /// Returns input-capacity overflow when present.
    #[must_use]
    pub const fn input(self) -> Option<LimitOverflow> {
        self.input
    }

    /// Returns output-capacity overflow when present.
    #[must_use]
    pub const fn output(self) -> Option<LimitOverflow> {
        self.output
    }
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

/// Byte composition of one exact lowered request body, by input part.
///
/// This is a measurement of the serialized wire body the harness is about to
/// send — the same bytes the admission count is derived from — split into the
/// parts an operator reasons about: the system prompt, the tool definitions,
/// the conversation messages, and replayed reasoning. Every figure is a byte
/// count read from the body except the supplied image-token bound. A product
/// consumer converts part bytes divided by four for occupancy display
/// (spec 02 §5.6). Admission still uses each vertical's own token bound.
///
/// The split is keyed on the three kernel body shapes (Responses
/// `instructions`/`tools`/`input`, Messages `system`/`tools`/`messages`, Chat
/// `messages` with a `system`/`developer` role) and names no provider. Bytes
/// the split does not attribute (model, options, metadata) are the difference
/// between `total_bytes` and the parts.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContextInputParts {
    total_bytes: u64,
    image_transport_bytes: u64,
    image_tokens: u64,
    system_bytes: u64,
    tool_definition_bytes: u64,
    tool_definition_count: u32,
    message_bytes: u64,
    reasoning_bytes: u64,
}

impl ContextInputParts {
    /// Measures one serialized request body. Returns `None` when the body is
    /// not a JSON object, so an unknown wire shape is reported as unmeasured
    /// rather than as a zero split.
    #[must_use]
    pub fn measure_wire(body: &[u8]) -> Option<Self> {
        let mut value: serde_json::Value = serde_json::from_slice(body).ok()?;
        let before = json_bytes(&value);
        let object = value.as_object_mut()?;
        for key in ["input", "messages"] {
            if let Some(field) = object.get_mut(key) {
                remove_image_payloads(field, Some(1)).ok()?;
            }
        }
        let image_transport_bytes = before.saturating_sub(json_bytes(&value));
        let object = value.as_object()?;
        let total_bytes = u64::try_from(body.len()).ok()?;
        let mut system_bytes = 0_u64;
        let mut message_bytes = 0_u64;
        let mut reasoning_bytes = 0_u64;
        // Responses `instructions` and Messages `system` are the whole system
        // prompt in one field.
        for key in ["instructions", "system"] {
            if let Some(field) = object.get(key) {
                system_bytes = system_bytes.saturating_add(json_bytes(field));
            }
        }
        let (tool_definition_bytes, tool_definition_count) = match object.get("tools") {
            Some(serde_json::Value::Array(tools)) => (
                json_bytes(object.get("tools")?),
                u32::try_from(tools.len()).unwrap_or(u32::MAX),
            ),
            Some(other) => (json_bytes(other), 0),
            None => (0, 0),
        };
        // Responses `input` items, or Messages/Chat `messages`.
        for key in ["input", "messages"] {
            let Some(serde_json::Value::Array(items)) = object.get(key) else {
                if let Some(other) = object.get(key) {
                    message_bytes = message_bytes.saturating_add(json_bytes(other));
                }
                continue;
            };
            for item in items {
                let bytes = json_bytes(item);
                let kind = item.get("type").and_then(serde_json::Value::as_str);
                let role = item.get("role").and_then(serde_json::Value::as_str);
                if matches!(kind, Some("reasoning" | "compaction")) {
                    reasoning_bytes = reasoning_bytes.saturating_add(bytes);
                } else if matches!(role, Some("system" | "developer")) {
                    system_bytes = system_bytes.saturating_add(bytes);
                } else {
                    // Thinking blocks and chat reasoning fields ride inside a
                    // message; they are moved from the message share to the
                    // reasoning share so neither is counted twice.
                    let inner = embedded_reasoning_bytes(item);
                    reasoning_bytes = reasoning_bytes.saturating_add(inner);
                    message_bytes = message_bytes.saturating_add(bytes.saturating_sub(inner));
                }
            }
        }
        Some(Self {
            total_bytes,
            image_transport_bytes,
            image_tokens: 0,
            system_bytes,
            tool_definition_bytes,
            tool_definition_count,
            message_bytes,
            reasoning_bytes,
        })
    }

    /// Returns the exact serialized body length.
    #[must_use]
    pub const fn total_bytes(self) -> u64 {
        self.total_bytes
    }
    /// Image transport bytes excluded from every text-token estimate.
    #[must_use]
    pub const fn image_transport_bytes(self) -> u64 {
        self.image_transport_bytes
    }

    /// The dialect-supplied image allowance for the measured input.
    #[must_use]
    pub const fn image_tokens(self) -> u64 {
        self.image_tokens
    }

    pub(crate) fn with_image_tokens(mut self, tokens: u64) -> Self {
        self.image_tokens = tokens;
        self
    }

    /// Returns the bytes of the system prompt (`instructions`, `system`, or
    /// system/developer-role messages).
    #[must_use]
    pub const fn system_bytes(self) -> u64 {
        self.system_bytes
    }
    /// Returns the bytes of the `tools` array.
    #[must_use]
    pub const fn tool_definition_bytes(self) -> u64 {
        self.tool_definition_bytes
    }
    /// Returns the number of entries in the `tools` array.
    #[must_use]
    pub const fn tool_definition_count(self) -> u32 {
        self.tool_definition_count
    }
    /// Returns the bytes of conversation items that are not system prompt or
    /// reasoning material.
    #[must_use]
    pub const fn message_bytes(self) -> u64 {
        self.message_bytes
    }
    /// Returns the bytes of replayed reasoning, encrypted reasoning, native
    /// compaction items, and thinking blocks.
    #[must_use]
    pub const fn reasoning_bytes(self) -> u64 {
        self.reasoning_bytes
    }

    /// Occupying tokens for chrome: each part's UTF-8 bytes divided by four
    /// (spec 02 §5.6). Envelope bytes are the unattributed remainder of the
    /// serialized body.
    #[must_use]
    pub const fn occupancy_tokens(self) -> u64 {
        const fn tokens(bytes: u64) -> u64 {
            bytes / 4
        }
        let attributed = self
            .system_bytes
            .saturating_add(self.tool_definition_bytes)
            .saturating_add(self.message_bytes)
            .saturating_add(self.reasoning_bytes);
        tokens(self.system_bytes)
            .saturating_add(tokens(self.tool_definition_bytes))
            .saturating_add(tokens(self.message_bytes))
            .saturating_add(tokens(self.reasoning_bytes))
            .saturating_add(tokens(
                self.total_bytes
                    .saturating_sub(self.image_transport_bytes)
                    .saturating_sub(attributed),
            ))
            .saturating_add(self.image_tokens)
    }
}

/// One lowered input item. Image transport bytes never enter its bound.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ContextWireItem {
    digest: String,
    bytes: u64,
    image_tokens: u64,
    response_output: bool,
    conversation_index: Option<usize>,
}

/// Item-level measurement of the input actually sent on the wire.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContextWireInput {
    items: Vec<ContextWireItem>,
    conversation_count: usize,
}

impl ContextWireInput {
    /// Measures lowered JSON items with a dialect-supplied per-image maximum.
    /// An image without an explicit dialect bound is unavailable, never base64-sized.
    ///
    /// # Errors
    /// Returns a typed error for an unknown image bound or malformed body.
    pub fn measure_wire(
        body: &[u8],
        image_maximum: Option<u64>,
    ) -> Result<Self, ContextContractError> {
        let object: std::collections::BTreeMap<String, &serde_json::value::RawValue> =
            serde_json::from_slice(body).map_err(|error| ContextContractError::Serialization {
                reason: error.to_string(),
            })?;
        let mut items = Vec::new();
        let mut conversation_count = 0;
        for key in ["instructions", "system", "tools", "input", "messages"] {
            let Some(field) = object.get(key) else {
                continue;
            };
            let values: Vec<&serde_json::value::RawValue> = if field.get().starts_with('[') {
                serde_json::from_str(field.get()).map_err(|error| {
                    ContextContractError::Serialization {
                        reason: error.to_string(),
                    }
                })?
            } else {
                vec![field]
            };
            for raw in values {
                let value: serde_json::Value =
                    serde_json::from_str(raw.get()).map_err(|error| {
                        ContextContractError::Serialization {
                            reason: error.to_string(),
                        }
                    })?;
                let wire = raw.get().as_bytes();
                let normalized =
                    serde_json::to_vec(&WireDigestItem { raw, root: true }).map_err(|error| {
                        ContextContractError::Serialization {
                            reason: error.to_string(),
                        }
                    })?;
                let digest = crate::hex_lower(
                    Digest::sha256(
                        DigestDomain::TokenMeasurement,
                        5,
                        &[key.as_bytes(), &normalized],
                    )?
                    .bytes(),
                );
                let mut bounded = value.clone();
                let image_tokens = if matches!(key, "input" | "messages") {
                    remove_image_payloads(&mut bounded, image_maximum)?
                } else {
                    0
                };
                let bytes = u64::try_from(wire.len())
                    .unwrap_or(u64::MAX)
                    .saturating_sub(json_bytes(&value))
                    .saturating_add(json_bytes(&bounded));
                let index = if matches!(key, "input" | "messages") {
                    let index = conversation_count;
                    conversation_count += 1;
                    Some(index)
                } else {
                    None
                };
                let response_output = index.is_some()
                    && (value.get("role").and_then(serde_json::Value::as_str) == Some("assistant")
                        || matches!(
                            value.get("type").and_then(serde_json::Value::as_str),
                            Some("function_call" | "custom_tool_call" | "reasoning")
                        ));
                items.push(ContextWireItem {
                    digest,
                    bytes,
                    image_tokens,
                    response_output,
                    conversation_index: index,
                });
            }
        }
        // Output schemas and other token-affecting envelope fields are items
        // too. Transport settings are harmless conservative overhead.
        for (key, raw) in &object {
            if matches!(
                key.as_str(),
                "instructions" | "system" | "tools" | "input" | "messages"
            ) {
                continue;
            }
            items.push(ContextWireItem {
                digest: crate::hex_lower(
                    Digest::sha256(
                        DigestDomain::TokenMeasurement,
                        5,
                        &[key.as_bytes(), raw.get().as_bytes()],
                    )?
                    .bytes(),
                ),
                bytes: u64::try_from(raw.get().len()).unwrap_or(u64::MAX),
                image_tokens: 0,
                response_output: false,
                conversation_index: None,
            });
        }
        Ok(Self {
            items,
            conversation_count,
        })
    }

    pub(crate) fn image_tokens(&self) -> u64 {
        self.items
            .iter()
            .fold(0_u64, |total, item| total.saturating_add(item.image_tokens))
    }

    /// Conservative byte/token input bound, before the admission margin.
    #[must_use]
    pub(crate) fn upper_bound(&self) -> u64 {
        self.items.iter().fold(0_u64, |total, item| {
            total
                .saturating_add(item.bytes)
                .saturating_add(item.image_tokens)
                .saturating_add(16)
        })
    }

    /// Non-guaranteed unanchored estimate, excluding image transport bytes.
    #[must_use]
    pub(crate) fn estimate_tokens(&self) -> u64 {
        self.items.iter().fold(0_u64, |total, item| {
            total
                .saturating_add(item.bytes.div_ceil(4))
                .saturating_add(item.image_tokens)
                .saturating_add(16)
        })
    }
}

pub(crate) fn context_admission_margin(ceiling: u64) -> u64 {
    ceiling.div_ceil(100).max(2048.min(ceiling / 10))
}

// Preserve wire key order and opaque tool data while omitting transport annotations.
struct WireDigestItem<'a> {
    raw: &'a serde_json::value::RawValue,
    root: bool,
}

struct WireDigestFields<'a>(Vec<(String, &'a serde_json::value::RawValue)>);

impl<'de> Deserialize<'de> for WireDigestFields<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FieldsVisitor;
        impl<'de> serde::de::Visitor<'de> for FieldsVisitor {
            type Value = WireDigestFields<'de>;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("wire item fields in their original order")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut fields = Vec::new();
                while let Some(field) = map.next_entry()? {
                    fields.push(field);
                }
                Ok(WireDigestFields(fields))
            }
        }
        deserializer.deserialize_map(FieldsVisitor)
    }
}

impl Serialize for WireDigestItem<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{Error as _, SerializeMap as _, SerializeSeq as _};
        let raw = self.raw.get().trim_start();
        if raw.starts_with('[') {
            let values: Vec<&serde_json::value::RawValue> =
                serde_json::from_str(raw).map_err(S::Error::custom)?;
            let mut seq = serializer.serialize_seq(Some(values.len()))?;
            for raw in values {
                seq.serialize_element(&Self { raw, root: false })?;
            }
            return seq.end();
        }
        if !raw.starts_with('{') {
            return self.raw.serialize(serializer);
        }
        let WireDigestFields(fields) = serde_json::from_str(raw).map_err(S::Error::custom)?;
        let annotations = self.root
            || fields.iter().any(|(key, value)| {
                key == "type"
                    && matches!(
                        value.get(),
                        "\"text\""
                            | "\"image\""
                            | "\"tool_use\""
                            | "\"tool_result\""
                            | "\"thinking\""
                            | "\"redacted_thinking\""
                    )
            });
        let mut map = serializer.serialize_map(None)?;
        for (key, raw) in fields {
            if annotations && key == "cache_control" {
                continue;
            }
            if annotations && key == "content" {
                map.serialize_entry(&key, &Self { raw, root: false })?;
            } else {
                map.serialize_entry(&key, raw)?;
            }
        }
        map.end()
    }
}

fn remove_image_payloads(
    value: &mut serde_json::Value,
    maximum: Option<u64>,
) -> Result<u64, ContextContractError> {
    fn content(
        value: &mut serde_json::Value,
        maximum: Option<u64>,
        block: bool,
    ) -> Result<u64, ContextContractError> {
        if block
            && matches!(
                value.get("type").and_then(serde_json::Value::as_str),
                Some("image" | "image_url" | "input_image")
            )
        {
            let tokens = maximum
                .filter(|tokens| *tokens > 0)
                .ok_or(ContextContractError::ImageBoundUnavailable)?;
            *value = serde_json::json!({"type": "bounded_image"});
            return Ok(tokens);
        }
        match value {
            serde_json::Value::Array(values) => {
                values.iter_mut().try_fold(0_u64, |total, value| {
                    Ok(total.saturating_add(content(value, maximum, block)?))
                })
            }
            serde_json::Value::Object(object) => object
                .get_mut("content")
                .map_or(Ok(0), |value| content(value, maximum, true)),
            _ => Ok(0),
        }
    }
    content(value, maximum, false)
}

/// Completed-response authority for item-delta admission on one binding.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ContextRequestAnchor {
    binding: Digest,
    route: RouteId,
    model: ModelId,
    usage: crate::Usage,
    input: ContextWireInput,
    response_items: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextRequestAnchorWire {
    binding: Digest,
    route: RouteId,
    model: ModelId,
    usage: crate::Usage,
    input: ContextWireInput,
    response_items: Option<Vec<String>>,
}

impl<'de> Deserialize<'de> for ContextRequestAnchor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ContextRequestAnchorWire::deserialize(deserializer)?;
        let mut anchor = Self::new(wire.binding, wire.route, wire.model, wire.usage, wire.input)
            .ok_or_else(|| {
                serde::de::Error::custom("context anchor requires positive input usage")
            })?;
        anchor.response_items = wire.response_items;
        Ok(anchor)
    }
}

impl ContextRequestAnchor {
    /// Records a successful response with positive input usage; output may be zero.
    #[must_use]
    pub fn new(
        binding: Digest,
        route: RouteId,
        model: ModelId,
        usage: crate::Usage,
        input: ContextWireInput,
    ) -> Option<Self> {
        (usage.input_tokens() > 0).then_some(Self {
            binding,
            route,
            model,
            usage,
            input,
            response_items: None,
        })
    }

    /// Integrity identity used by durable references to this anchor.
    /// Derived response-item coverage is a cache, not a new authority.
    ///
    /// # Errors
    /// Returns an error if the anchor cannot be serialized or digested.
    pub fn identity(&self) -> Result<Digest, ContextContractError> {
        semantic_digest(
            DigestDomain::TokenMeasurement,
            5,
            &(
                self.binding,
                &self.route,
                &self.model,
                &self.usage,
                &self.input,
            ),
        )
    }

    /// Checks the observed endpoint/model against the current admitted binding.
    #[must_use]
    pub fn matches_descriptor(&self, descriptor: &crate::ResolvedModelDescriptor) -> bool {
        Some(self.binding) == descriptor.semantic_binding_digest().ok()
            && &self.route == descriptor.route()
            && descriptor.serves_same_model(&self.model)
    }

    pub(crate) fn observe_response_items(&mut self, current: &ContextWireInput) {
        if self.response_items.is_some() {
            return;
        }
        let mut output = Vec::new();
        for item in &current.items {
            if item
                .conversation_index
                .is_none_or(|index| index < self.input.conversation_count)
            {
                continue;
            }
            if !item.response_output {
                break;
            }
            if self.usage.output_tokens() > 0 {
                output.push(item.digest.clone());
            }
        }
        self.response_items = Some(output);
    }

    /// Provider input already includes cache reads and writes; add output once.
    #[must_use]
    pub(crate) fn occupied_tokens(&self, current: &ContextWireInput) -> u64 {
        let mut observed = self.clone();
        observed.observe_response_items(current);
        let mut covered: std::collections::BTreeMap<&str, usize> =
            std::collections::BTreeMap::new();
        for item in &self.input.items {
            *covered.entry(item.digest.as_str()).or_default() += 1;
        }
        for digest in observed.response_items.iter().flatten() {
            *covered.entry(digest.as_str()).or_default() += 1;
        }
        current.items.iter().fold(
            self.usage
                .input_tokens()
                .saturating_add(self.usage.output_tokens()),
            |total, item| {
                if let Some(count) = covered.get_mut(item.digest.as_str())
                    && *count > 0
                {
                    *count -= 1;
                    return total;
                }
                total
                    .saturating_add(item.bytes)
                    .saturating_add(item.image_tokens)
                    .saturating_add(16)
            },
        )
    }
}

/// The provider-reported input of one sent request, paired with the local
/// `bytes/4` estimate of that same request.
///
/// The provider's count (cached input included, as its usage reports it) is
/// the authoritative size of the prefix it received; the estimate is what
/// lets later growth be measured on the same local scale, so only the suffix
/// appended since that request is ever estimated.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "ContextAnchorWire")]
pub struct ContextAnchor {
    provider_input_tokens: u64,
    request_estimate_tokens: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextAnchorWire {
    provider_input_tokens: u64,
    request_estimate_tokens: u64,
}

impl TryFrom<ContextAnchorWire> for ContextAnchor {
    type Error = &'static str;

    fn try_from(wire: ContextAnchorWire) -> Result<Self, Self::Error> {
        Self::new(wire.provider_input_tokens, wire.request_estimate_tokens)
            .ok_or("a context anchor needs a positive provider input count")
    }
}

impl ContextAnchor {
    /// Pairs a provider input count with the estimate of the request it
    /// counted. `None` when the provider reported no input: a zero count is
    /// not an observation.
    #[must_use]
    pub const fn new(provider_input_tokens: u64, request_estimate_tokens: u64) -> Option<Self> {
        if provider_input_tokens == 0 {
            return None;
        }
        Some(Self {
            provider_input_tokens,
            request_estimate_tokens,
        })
    }

    /// Returns the provider-reported input tokens of the anchored request.
    #[must_use]
    pub const fn provider_input_tokens(self) -> u64 {
        self.provider_input_tokens
    }

    /// Returns the local estimate of the anchored request.
    #[must_use]
    pub const fn request_estimate_tokens(self) -> u64 {
        self.request_estimate_tokens
    }
}

/// The context measure: the anchored provider input plus the estimate of the
/// suffix appended since that request, or the whole estimate with no anchor.
///
/// `estimate_tokens` is the `bytes/4` estimate of the whole projected request
/// now, on the same scale as the anchor's request estimate. Only the
/// compaction fences and the context gauge read this anchored number;
/// compaction records, telemetry and `ToolResultsReplaced` use the unanchored
/// whole-request estimate (the bound when the request has no parts).
#[must_use]
pub const fn context_occupancy_tokens(estimate_tokens: u64, anchor: Option<ContextAnchor>) -> u64 {
    match anchor {
        Some(anchor) => anchor
            .provider_input_tokens
            .saturating_add(estimate_tokens.saturating_sub(anchor.request_estimate_tokens)),
        None => estimate_tokens,
    }
}

/// Compact serialized length of one JSON value, saturating at `u64::MAX`.
fn json_bytes(value: &serde_json::Value) -> u64 {
    serde_json::to_vec(value).map_or(0, |bytes| u64::try_from(bytes.len()).unwrap_or(u64::MAX))
}

/// Reasoning material embedded in one message item: Messages `thinking` /
/// `redacted_thinking` content blocks, and Chat `reasoning` /
/// `reasoning_details` fields.
fn embedded_reasoning_bytes(item: &serde_json::Value) -> u64 {
    let mut bytes = 0_u64;
    if let Some(serde_json::Value::Array(blocks)) = item.get("content") {
        for block in blocks {
            if matches!(
                block.get("type").and_then(serde_json::Value::as_str),
                Some("thinking" | "redacted_thinking")
            ) {
                bytes = bytes.saturating_add(json_bytes(block));
            }
        }
    }
    for key in ["reasoning", "reasoning_details"] {
        if let Some(field) = item.get(key) {
            bytes = bytes.saturating_add(json_bytes(field));
        }
    }
    bytes
}

/// Inspectable result of complete-projection context evaluation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ContextStatus {
    descriptor_binding: Digest,
    model_input_identity: Digest,
    scope: Option<ContextScope>,
    capacity: Option<ContextCapacity>,
    measurement: Option<ContextMeasurement>,
    reservation: Option<OutputReservation>,
    planner: Option<ContextPlannerDisposition>,
    kind: ContextStatusKind,
    identity: Digest,
    /// Byte composition of the measured body, when the preparing provider
    /// attached one. Observational and presentation-only: it is outside the
    /// status identity and grants no admission authority.
    input_parts: Option<ContextInputParts>,
    /// The provider count the occupancy is anchored on, when the measuring
    /// runtime has one. Presentation-only like `input_parts`.
    context_anchor: Option<ContextAnchor>,
    wire_input: Option<ContextWireInput>,
    /// Freshness-insensitive binding used by item-delta admission, not the
    /// exact descriptor binding in the persisted status identity.
    #[serde(skip)]
    wire_binding: Option<Digest>,
    request_anchor: Option<ContextRequestAnchor>,
    unanchored_estimate: bool,
    local_input_upper_bound: Option<u64>,
    semantic_summary_bound: u64,
}

#[derive(Serialize)]
struct ContextStatusContent<'a> {
    descriptor_binding: Digest,
    model_input_identity: Digest,
    scope: Option<Digest>,
    capacity: Option<Digest>,
    non_guarantee_disposition: Option<ContextNonGuaranteeDisposition>,
    measurement: Option<Digest>,
    reservation: Option<Digest>,
    planner: Option<Digest>,
    kind: &'a ContextStatusKind,
}

impl ContextStatus {
    pub(crate) fn unavailable(
        descriptor_binding: Digest,
        model_input_identity: Digest,
        scope: Option<ContextScope>,
        capacity: Option<ContextCapacity>,
        measurement: Option<ContextMeasurement>,
        reservation: Option<OutputReservation>,
        unavailable: ContextUnavailable,
    ) -> Result<Self, ContextContractError> {
        Self::new(
            descriptor_binding,
            model_input_identity,
            scope,
            capacity,
            measurement,
            reservation,
            None,
            ContextStatusKind::Unavailable(unavailable),
        )
    }

    pub(crate) fn evaluate(
        capacity: ContextCapacity,
        measurement: ContextMeasurement,
        reservation: Option<OutputReservation>,
        request: &ModelRequest,
    ) -> Result<Self, ContextContractError> {
        let scope = capacity.scope().clone();
        let descriptor_binding = scope.descriptor_binding();
        let model_input_identity = scope.model_input_identity();
        let mut planner = None;
        let kind = if request.model_input_identity() != model_input_identity {
            ContextStatusKind::Unavailable(ContextUnavailable::ScopeDrift {
                field: "planner model input",
            })
        } else if measurement.scope().identity() != scope.identity()
            || reservation
                .as_ref()
                .is_some_and(|reservation| reservation.scope != scope.identity())
        {
            ContextStatusKind::Unavailable(ContextUnavailable::ScopeDrift {
                field: "context fact",
            })
        } else {
            match ContextHistoryPlan::new(request.input()) {
                Err(_) => {
                    ContextStatusKind::Unavailable(ContextUnavailable::HistoryPlanningUnavailable)
                }
                Ok(history_plan) => {
                    match evaluate_rule(
                        capacity.rule(),
                        measurement.bound(),
                        reservation.as_ref().map(OutputReservation::tokens),
                    ) {
                        Ok(headroom) => ContextStatusKind::Fits { headroom },
                        Err(EvaluationFailure::Overflow(overflow)) => {
                            let disposition = history_plan.disposition().clone();
                            let kind = match disposition.kind() {
                                ContextPlannerDispositionKind::EligibleWholeGroups
                                | ContextPlannerDispositionKind::EligibleActiveTurnPrefix => {
                                    ContextStatusKind::WouldOverflow(overflow)
                                }
                                ContextPlannerDispositionKind::ProtectedOnly => {
                                    ContextStatusKind::ProtectedOverflow(overflow)
                                }
                            };
                            planner = Some(disposition);
                            kind
                        }
                        Err(EvaluationFailure::ArithmeticOverflow) => {
                            ContextStatusKind::Unavailable(ContextUnavailable::ArithmeticOverflow)
                        }
                    }
                }
            }
        };
        Self::new(
            descriptor_binding,
            model_input_identity,
            Some(scope),
            Some(capacity),
            Some(measurement),
            reservation,
            planner,
            kind,
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "status retains every authority-bearing evaluation input"
    )]
    fn new(
        descriptor_binding: Digest,
        model_input_identity: Digest,
        scope: Option<ContextScope>,
        capacity: Option<ContextCapacity>,
        measurement: Option<ContextMeasurement>,
        reservation: Option<OutputReservation>,
        planner: Option<ContextPlannerDisposition>,
        kind: ContextStatusKind,
    ) -> Result<Self, ContextContractError> {
        let content = ContextStatusContent {
            descriptor_binding,
            model_input_identity,
            scope: scope.as_ref().map(ContextScope::identity),
            capacity: capacity.as_ref().map(ContextCapacity::identity),
            non_guarantee_disposition: capacity
                .as_ref()
                .and_then(ContextCapacity::non_guarantee_disposition),
            measurement: measurement.as_ref().map(ContextMeasurement::identity),
            reservation: reservation.as_ref().map(OutputReservation::identity),
            planner: planner.as_ref().map(ContextPlannerDisposition::identity),
            kind: &kind,
        };
        let identity = semantic_digest(DigestDomain::ContextAdmission, 3, &content)?;
        Ok(Self {
            descriptor_binding,
            model_input_identity,
            scope,
            capacity,
            measurement,
            reservation,
            planner,
            kind,
            identity,
            input_parts: None,
            context_anchor: None,
            wire_input: None,
            wire_binding: None,
            request_anchor: None,
            unanchored_estimate: false,
            local_input_upper_bound: None,
            semantic_summary_bound: u64::from(crate::SEMANTIC_COMPACTION_SUMMARY_TOKENS),
        })
    }

    pub(crate) const fn with_semantic_summary_bound(mut self, bound: u64) -> Self {
        self.semantic_summary_bound = bound;
        self
    }

    /// Remaining conservative input headroom after a matching completed response.
    #[must_use]
    pub fn completed_response_headroom(
        &self,
        anchor: &ContextRequestAnchor,
    ) -> Option<ContextHeadroom> {
        if Some(anchor.binding) != self.wire_binding
            || !self.scope.as_ref().is_some_and(|scope| {
                anchor.route == *scope.route() && scope.serves_same_model(&anchor.model)
            })
        {
            return None;
        }
        let capacity = self.capacity.as_ref()?;
        let bound = anchor
            .usage
            .input_tokens()
            .saturating_add(anchor.usage.output_tokens())
            .saturating_add(context_admission_margin(capacity.rule().input_ceiling()));
        evaluate_rule(
            capacity.rule(),
            CountBound::CustomEnvelopeUpperBound {
                tokens: bound,
                declaration: self.identity,
            },
            self.reservation.as_ref().map(OutputReservation::tokens),
        )
        .ok()
    }

    /// Shared hosted/core compaction trigger, using this request's stable prefix.
    #[must_use]
    pub fn compaction_trigger_tokens(&self) -> Option<u64> {
        let stable = self.input_parts.map_or(0, |parts| {
            parts
                .system_bytes()
                .div_ceil(4)
                .saturating_add(parts.tool_definition_bytes().div_ceil(4))
        });
        Some(
            self.capacity.as_ref()?.rule().compaction_trigger_tokens(
                self.reservation
                    .as_ref()
                    .map_or(0, OutputReservation::tokens),
                stable,
                self.semantic_summary_bound,
            ),
        )
    }

    pub(crate) fn evaluate_wire_input(
        mut self,
        request: &ModelRequest,
        input: &ContextWireInput,
        anchor: Option<&ContextRequestAnchor>,
        binding: Digest,
        require_bound: bool,
    ) -> Result<Self, ContextContractError> {
        self.semantic_summary_bound = u64::from(
            crate::semantic_compaction_output_bound(
                request.limits().max_output_tokens(),
                request.reasoning(),
            )
            .get(),
        );
        let Some(capacity) = self.capacity.as_ref() else {
            return Ok(self);
        };
        if matches!(self.kind, ContextStatusKind::Unavailable(_)) {
            return Ok(self);
        }
        self.wire_binding = Some(binding);
        let anchor = anchor.filter(|anchor| {
            anchor.binding == binding
                && self.scope.as_ref().is_some_and(|scope| {
                    &anchor.route == scope.route() && scope.serves_same_model(&anchor.model)
                })
        });
        let ceiling = capacity.rule().input_ceiling();
        let margin = context_admission_margin(ceiling);
        let bounded = anchor
            .map_or_else(
                || input.upper_bound(),
                |anchor| anchor.occupied_tokens(input).min(input.upper_bound()),
            )
            .saturating_add(margin);
        let reserved = self.reservation.as_ref().map(OutputReservation::tokens);
        let mut evaluation = evaluate_rule(
            capacity.rule(),
            CountBound::CustomEnvelopeUpperBound {
                tokens: bounded,
                declaration: self.identity,
            },
            reserved,
        );
        if anchor.is_none()
            && !require_bound
            && matches!(evaluation, Err(EvaluationFailure::Overflow(_)))
        {
            // This is explicitly not a guarantee: a plausible first request is
            // decided by the current provider, with typed overflow recovery.
            evaluation = evaluate_rule(
                capacity.rule(),
                CountBound::CustomEnvelopeUpperBound {
                    tokens: input.estimate_tokens(),
                    declaration: self.identity,
                },
                reserved,
            );
            self.unanchored_estimate = evaluation.is_ok();
        }
        self.local_input_upper_bound = (!self.unanchored_estimate).then_some(bounded);
        self.kind = match evaluation {
            Ok(headroom) => ContextStatusKind::Fits { headroom },
            Err(EvaluationFailure::ArithmeticOverflow) => {
                ContextStatusKind::Unavailable(ContextUnavailable::ArithmeticOverflow)
            }
            Err(EvaluationFailure::Overflow(overflow)) => {
                let plan = ContextHistoryPlan::new(request.input()).map_err(|_| {
                    ContextContractError::Serialization {
                        reason: "history planning unavailable".to_owned(),
                    }
                })?;
                let disposition = plan.disposition().clone();
                let protected = matches!(
                    disposition.kind(),
                    ContextPlannerDispositionKind::ProtectedOnly
                );
                self.planner = Some(disposition);
                if protected {
                    ContextStatusKind::ProtectedOverflow(overflow)
                } else {
                    ContextStatusKind::WouldOverflow(overflow)
                }
            }
        };
        self.wire_input = Some(input.clone());
        self.request_anchor = anchor.cloned();
        self.identity = semantic_digest(
            DigestDomain::ContextAdmission,
            5,
            &(
                self.identity,
                &self.wire_input,
                &self.request_anchor,
                self.unanchored_estimate,
                &self.kind,
            ),
        )?;
        Ok(self)
    }

    /// The selected conservative input bound, including the local margin.
    /// Absent when a non-guaranteed estimate delegated to the provider.
    #[must_use]
    pub const fn local_input_upper_bound(&self) -> Option<u64> {
        self.local_input_upper_bound
    }

    /// Returns the item measurement used by local admission.
    #[must_use]
    pub const fn wire_input(&self) -> Option<&ContextWireInput> {
        self.wire_input.as_ref()
    }

    /// Whether an unanchored estimate delegated the final decision to the provider.
    #[must_use]
    pub const fn uses_unanchored_estimate(&self) -> bool {
        self.unanchored_estimate
    }

    /// Attaches the byte composition of the measured body. The identity is
    /// unchanged: the parts describe the same bytes the measurement counted.
    #[must_use]
    pub const fn with_input_parts(mut self, input_parts: Option<ContextInputParts>) -> Self {
        self.input_parts = input_parts;
        self
    }

    /// Returns the byte composition of the measured body, when attached.
    #[must_use]
    pub const fn input_parts(&self) -> Option<ContextInputParts> {
        self.input_parts
    }

    /// Attaches the provider count occupancy is anchored on. The identity is
    /// unchanged and no admission disposition reads it.
    #[must_use]
    pub const fn with_context_anchor(mut self, anchor: Option<ContextAnchor>) -> Self {
        self.context_anchor = anchor;
        self
    }

    /// Returns the attached occupancy anchor.
    #[must_use]
    pub const fn context_anchor(&self) -> Option<ContextAnchor> {
        self.context_anchor
    }

    /// The `bytes/4` estimate of the whole measured request, from the byte
    /// parts. `None` without parts: the measurement bound is on another
    /// scale (wire bytes on some routes) and is never anchored.
    #[must_use]
    pub fn occupancy_estimate_tokens(&self) -> Option<u64> {
        self.wire_input
            .as_ref()
            .map(ContextWireInput::estimate_tokens)
            .or_else(|| self.input_parts.map(ContextInputParts::occupancy_tokens))
    }

    /// The unanchored size of this request: the parts estimate, or the
    /// measurement's upper bound without parts. Compaction records use this
    /// for both their before and after figures so the two share one basis.
    #[must_use]
    pub fn unanchored_occupancy_tokens(&self) -> Option<u64> {
        self.occupancy_estimate_tokens().or_else(|| {
            self.measurement
                .as_ref()
                .map(|measurement| measurement.bound().upper_bound())
        })
    }

    /// The context measure of this request: [`context_occupancy_tokens`] over
    /// the parts estimate and the attached anchor, or the unanchored bound
    /// when no parts were attached.
    #[must_use]
    pub fn occupancy_tokens(&self) -> Option<u64> {
        if let (Some(input), Some(anchor)) = (&self.wire_input, &self.request_anchor) {
            return Some(anchor.occupied_tokens(input));
        }
        if let Some(input) = &self.wire_input {
            return Some(input.estimate_tokens());
        }
        match self.occupancy_estimate_tokens() {
            Some(estimate) => Some(context_occupancy_tokens(estimate, self.context_anchor)),
            None => self.unanchored_occupancy_tokens(),
        }
    }

    /// Returns the exact resolved-descriptor binding.
    #[must_use]
    pub const fn descriptor_binding(&self) -> Digest {
        self.descriptor_binding
    }

    /// Returns the complete model-input projection identity.
    #[must_use]
    pub const fn model_input_identity(&self) -> Digest {
        self.model_input_identity
    }

    /// Returns the exact scope when a descriptor contract was available.
    #[must_use]
    pub const fn scope(&self) -> Option<&ContextScope> {
        self.scope.as_ref()
    }

    /// Returns admitted capacity when available.
    #[must_use]
    pub const fn capacity(&self) -> Option<&ContextCapacity> {
        self.capacity.as_ref()
    }

    /// Returns admitted measurement when available.
    #[must_use]
    pub const fn measurement(&self) -> Option<&ContextMeasurement> {
        self.measurement.as_ref()
    }

    /// Returns explicit output reservation when available.
    #[must_use]
    pub const fn reservation(&self) -> Option<&OutputReservation> {
        self.reservation.as_ref()
    }

    /// Returns sealed closed-policy evidence when arithmetic overflowed.
    #[must_use]
    pub const fn planner_disposition(&self) -> Option<&ContextPlannerDisposition> {
        self.planner.as_ref()
    }

    /// Returns the checked status.
    #[must_use]
    pub const fn kind(&self) -> &ContextStatusKind {
        &self.kind
    }

    /// Returns the status identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }

    /// Returns the evidence class when capacity evidence was admitted.
    #[must_use]
    pub fn evidence_class(&self) -> Option<&ContextEvidenceClass> {
        self.capacity.as_ref().map(ContextCapacity::evidence_class)
    }

    /// Returns the mandatory warning for reference-client-envelope capacity.
    #[must_use]
    pub fn non_guarantee_disposition(&self) -> Option<ContextNonGuaranteeDisposition> {
        if self.unanchored_estimate {
            return Some(ContextNonGuaranteeDisposition::UnanchoredProviderDecision);
        }
        self.capacity
            .as_ref()
            .and_then(ContextCapacity::non_guarantee_disposition)
    }

    /// Freezes an admitted request only when this exact status fits.
    ///
    /// # Errors
    ///
    /// Returns typed unavailability/overflow or exact request-projection drift.
    pub fn admit(&self, request: &ModelRequest) -> Result<ContextAdmission, ContextAdmissionError> {
        self.admit_with_requirement(request, ContextAdmissionRequirement::AnyBounded)
    }

    /// Freezes an admitted request only when this exact status fits and meets
    /// the caller's closed minimum-evidence-class requirement.
    ///
    /// # Errors
    ///
    /// Returns before dispatch when release-verified capacity was required but this
    /// status carries a reference-client-envelope operating envelope.
    pub fn admit_with_requirement(
        &self,
        request: &ModelRequest,
        requirement: ContextAdmissionRequirement,
    ) -> Result<ContextAdmission, ContextAdmissionError> {
        if request.model_input_identity() != self.model_input_identity {
            return Err(ContextAdmissionError::Unavailable(
                ContextUnavailable::ScopeDrift {
                    field: "model input",
                },
            ));
        }
        match &self.kind {
            ContextStatusKind::Fits { .. }
                if requirement == ContextAdmissionRequirement::ReleaseVerifiedRequired
                    && (self.evidence_class() != Some(&ContextEvidenceClass::ReleaseVerified)
                        || self.unanchored_estimate) =>
            {
                Err(ContextAdmissionError::Unavailable(
                    ContextUnavailable::EvidenceRequirementNotMet {
                        required: requirement,
                        actual: self.evidence_class().cloned(),
                    },
                ))
            }
            ContextStatusKind::Fits { .. } => {
                ContextAdmission::new(request.clone(), self.clone(), requirement)
            }
            ContextStatusKind::WouldOverflow(overflow) => {
                Err(ContextAdmissionError::WouldOverflow(*overflow))
            }
            ContextStatusKind::ProtectedOverflow(overflow) => {
                Err(ContextAdmissionError::ProtectedOverflow(*overflow))
            }
            ContextStatusKind::Unavailable(unavailable) => {
                Err(ContextAdmissionError::Unavailable(unavailable.clone()))
            }
        }
    }
}

/// A descriptor-checked whole-projection admission witness.
#[derive(Clone, Debug, PartialEq)]
pub struct ContextAdmission {
    request: ModelRequest,
    status: ContextStatus,
    requirement: ContextAdmissionRequirement,
    identity: Digest,
}

impl ContextAdmission {
    fn new(
        request: ModelRequest,
        status: ContextStatus,
        requirement: ContextAdmissionRequirement,
    ) -> Result<Self, ContextAdmissionError> {
        let requirement_bytes = serde_json::to_vec(&requirement).map_err(|error| {
            ContextContractError::Serialization {
                reason: error.to_string(),
            }
        })?;
        let identity = Digest::sha256(
            DigestDomain::ContextAdmission,
            4,
            &[
                &request.identity().identity_bytes(),
                &status.identity().identity_bytes(),
                &requirement_bytes,
            ],
        )?;
        Ok(Self {
            request,
            status,
            requirement,
            identity,
        })
    }

    /// Returns the exact immutable admitted request.
    #[must_use]
    pub const fn request(&self) -> &ModelRequest {
        &self.request
    }

    /// Returns the complete checked status witness.
    #[must_use]
    pub const fn status(&self) -> &ContextStatus {
        &self.status
    }

    /// Returns the caller-selected minimum evidence class bound into this receipt.
    #[must_use]
    pub const fn requirement(&self) -> ContextAdmissionRequirement {
        self.requirement
    }

    /// Returns the admitted evidence class.
    #[must_use]
    pub fn evidence_class(&self) -> Option<&ContextEvidenceClass> {
        self.status.evidence_class()
    }

    /// Returns the mandatory warning for reference-client-envelope capacity.
    #[must_use]
    pub fn non_guarantee_disposition(&self) -> Option<ContextNonGuaranteeDisposition> {
        self.status.non_guarantee_disposition()
    }

    /// Returns the admission identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
}

/// A context contract construction failure.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum ContextContractError {
    /// The selected dialect has no documented image token bound.
    #[error("image context measurement requires a documented dialect bound")]
    ImageBoundUnavailable,
    /// A label was empty, oversized, or non-portable.
    #[error("{field} must be a nonempty portable identifier")]
    InvalidLabel {
        /// The invalid field.
        field: &'static str,
    },
    /// A safe capacity limit was zero.
    #[error("{field} must be nonzero")]
    ZeroLimit {
        /// The invalid capacity axis.
        field: &'static str,
    },
    /// A provider-reserved input ceiling exceeded its reported context window.
    #[error("provider-reserved input must not exceed the reported context window")]
    InvalidCapacityRelationship,
    /// Evidence expiry did not follow observation.
    #[error("evidence expiry must follow observation")]
    InvalidFreshness,
    /// Authenticated discovery lacked exact expiring authority scope.
    #[error("authenticated discovery requires an authority binding and finite expiry")]
    UnscopedDiscovery,
    /// A custom declaration lacked an exact configuration/provenance binding.
    #[error("a custom context declaration requires an authority binding")]
    UnscopedCustomDeclaration,
    /// Non-discovery evidence incorrectly carried account/credential scope.
    #[error("only authenticated discovery may carry an authority binding")]
    UnexpectedAuthorityBinding,
    /// Release-owned evidence construction lacked Standard authority.
    #[error("standard context evidence requires release-owned authority")]
    StandardAuthorityRequired,
    /// Descriptor ownership disagreed with its context evidence owner.
    #[error("model integration class disagrees with context evidence class")]
    IntegrationClassMismatch,
    /// A conservative count did not use a release-owned certificate.
    #[error("a conservative token upper bound requires release-owned certification")]
    UpperBoundNotReleaseVerified,
    /// Reference-client-envelope evidence was incomplete or was presented as certified.
    #[error(
        "reference-client-envelope evidence requires an input-only rule, exact receipt, and finite authenticated freshness"
    )]
    InvalidReferenceClientEnvelopeEvidence,
    /// Custom caller evidence was incomplete or crossed into a standard lane.
    #[error(
        "custom-envelope evidence requires an input-only rule, exact declaration, and non-guaranteed caller provenance"
    )]
    InvalidCustomEnvelopeEvidence,
    /// Capacity and measurement evidence classes or receipts differed.
    #[error("context capacity and measurement evidence classes do not match")]
    EvidenceClassMismatch,
    /// Too many measurement certificates were configured.
    #[error("a context contract accepts at most 16 measurement certificates")]
    TooManyMeasurementCertificates,
    /// Measurement certificate IDs must be unique within a contract.
    #[error("duplicate measurement certificate ID")]
    DuplicateMeasurementCertificate,
    /// A descriptor binding could not be computed.
    #[error("resolved descriptor binding failed: {reason}")]
    DescriptorBinding {
        /// Safe diagnostic reason.
        reason: String,
    },
    /// JSON serialization for an identity failed.
    #[error("context identity serialization failed: {reason}")]
    Serialization {
        /// Serialization failure.
        reason: String,
    },
    /// Digest construction failed.
    #[error(transparent)]
    Digest(#[from] DigestError),
    /// Serialized certificate facts disagreed with their identity.
    #[error("serialized {field} identity does not match its semantic facts")]
    IntegrityMismatch {
        /// The invalid semantic fact.
        field: &'static str,
    },
}

/// A typed denial or construction failure from context admission.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum ContextAdmissionError {
    /// Safe context status was unavailable.
    #[error("context admission unavailable: {0:?}")]
    Unavailable(ContextUnavailable),
    /// The projection exceeded capacity and may be compactable.
    #[error("model input and output reservation exceed context capacity")]
    WouldOverflow(ContextOverflow),
    /// The projection exceeded capacity and only protected material remains.
    #[error("protected model input and output reservation exceed context capacity")]
    ProtectedOverflow(ContextOverflow),
    /// Contract or status identity construction failed.
    #[error(transparent)]
    Contract(#[from] ContextContractError),
    /// Admission identity construction failed.
    #[error(transparent)]
    Digest(#[from] DigestError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EvaluationFailure {
    Overflow(ContextOverflow),
    ArithmeticOverflow,
}

#[allow(
    clippy::too_many_lines,
    reason = "one closed match keeps all capacity algebras auditable together"
)]
fn evaluate_rule(
    rule: CapacityRule,
    input: CountBound,
    reserved_output: Option<u64>,
) -> Result<ContextHeadroom, EvaluationFailure> {
    let input_upper = input.upper_bound();
    match rule {
        CapacityRule::ProviderReservedInput {
            maximum_projected_input,
            ..
        } => {
            if input_upper > maximum_projected_input {
                return Err(EvaluationFailure::Overflow(ContextOverflow {
                    shared: None,
                    input: Some(LimitOverflow {
                        required: input_upper,
                        available: maximum_projected_input,
                    }),
                    output: None,
                }));
            }
            Ok(ContextHeadroom::ProviderReservedInput {
                input: qualify(maximum_projected_input - input_upper, false),
            })
        }
        CapacityRule::SharedWindow { total } => {
            let reserved_output = reserved_output.ok_or(EvaluationFailure::ArithmeticOverflow)?;
            let required = input_upper
                .checked_add(reserved_output)
                .ok_or(EvaluationFailure::ArithmeticOverflow)?;
            let available = total.lower_bound();
            if required > available {
                return Err(EvaluationFailure::Overflow(ContextOverflow {
                    shared: Some(LimitOverflow {
                        required,
                        available,
                    }),
                    input: None,
                    output: None,
                }));
            }
            Ok(ContextHeadroom::Shared(qualify(
                available - required,
                total.is_exact() && input.is_exact(),
            )))
        }
        CapacityRule::Separate {
            input: input_limit,
            output,
        } => {
            let reserved_output = reserved_output.ok_or(EvaluationFailure::ArithmeticOverflow)?;
            let input_available = input_limit.lower_bound();
            let output_available = output.lower_bound();
            let overflow = ContextOverflow {
                shared: None,
                input: (input_upper > input_available).then_some(LimitOverflow {
                    required: input_upper,
                    available: input_available,
                }),
                output: (reserved_output > output_available).then_some(LimitOverflow {
                    required: reserved_output,
                    available: output_available,
                }),
            };
            if overflow.input.is_some() || overflow.output.is_some() {
                return Err(EvaluationFailure::Overflow(overflow));
            }
            Ok(ContextHeadroom::Separate {
                input: qualify(
                    input_available - input_upper,
                    input_limit.is_exact() && input.is_exact(),
                ),
                output: qualify(output_available - reserved_output, output.is_exact()),
            })
        }
        CapacityRule::SharedWindowWithOutputCap { total, output } => {
            let reserved_output = reserved_output.ok_or(EvaluationFailure::ArithmeticOverflow)?;
            let required = input_upper
                .checked_add(reserved_output)
                .ok_or(EvaluationFailure::ArithmeticOverflow)?;
            let total_available = total.lower_bound();
            let output_available = output.lower_bound();
            let overflow = ContextOverflow {
                shared: (required > total_available).then_some(LimitOverflow {
                    required,
                    available: total_available,
                }),
                input: None,
                output: (reserved_output > output_available).then_some(LimitOverflow {
                    required: reserved_output,
                    available: output_available,
                }),
            };
            if overflow.shared.is_some() || overflow.output.is_some() {
                return Err(EvaluationFailure::Overflow(overflow));
            }
            Ok(ContextHeadroom::SharedWithOutputCap {
                shared: qualify(
                    total_available - required,
                    total.is_exact() && input.is_exact(),
                ),
                output: qualify(output_available - reserved_output, output.is_exact()),
            })
        }
    }
}

const fn qualify(value: u64, exact: bool) -> QualifiedTokens {
    if exact {
        QualifiedTokens::Exact(value)
    } else {
        QualifiedTokens::GuaranteedMinimum(value)
    }
}

fn validate_label(value: &str, field: &'static str) -> Result<(), ContextContractError> {
    if value.is_empty()
        || value.len() > MAX_CONTEXT_LABEL_BYTES
        || !value.bytes().all(|byte| {
            matches!(
                byte,
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'
            )
        })
    {
        Err(ContextContractError::InvalidLabel { field })
    } else {
        Ok(())
    }
}

fn validate_evidence_authority(
    source: ContextEvidenceSource,
    authority_binding: Option<Digest>,
    freshness: EvidenceFreshness,
) -> Result<(), ContextContractError> {
    if source == ContextEvidenceSource::AuthenticatedDiscovery {
        if authority_binding.is_none() || freshness.expires_at().is_none() {
            Err(ContextContractError::UnscopedDiscovery)
        } else {
            Ok(())
        }
    } else if source == ContextEvidenceSource::CallerDeclaration {
        if authority_binding.is_none() {
            Err(ContextContractError::UnscopedCustomDeclaration)
        } else {
            Ok(())
        }
    } else if authority_binding.is_some() {
        Err(ContextContractError::UnexpectedAuthorityBinding)
    } else {
        Ok(())
    }
}

fn semantic_digest(
    domain: DigestDomain,
    version: u32,
    value: &impl Serialize,
) -> Result<Digest, ContextContractError> {
    let bytes = serde_json::to_vec(value).map_err(|error| ContextContractError::Serialization {
        reason: error.to_string(),
    })?;
    Digest::sha256(domain, version, &[&bytes]).map_err(Into::into)
}

#[cfg(test)]
mod occupancy_anchor_tests {
    use super::{ContextAnchor, context_occupancy_tokens};

    #[test]
    fn anchored_occupancy_is_provider_input_plus_the_suffix_estimate() {
        // The provider counted 298,468 tokens for a request estimated at
        // 850,715; 4,000 bytes appended since then add 1,000 tokens.
        let anchor = ContextAnchor::new(298_468, 850_715);
        assert_eq!(
            context_occupancy_tokens(850_715 + 1_000, anchor),
            298_468 + 1_000
        );
        assert_eq!(context_occupancy_tokens(850_715, anchor), 298_468);
    }

    /// Continuation frames decode their anchor through serde, so the
    /// positive-count invariant holds there too.
    #[test]
    fn a_decoded_context_anchor_rejects_a_zero_provider_count() -> serde_json::Result<()> {
        let decoded: ContextAnchor = serde_json::from_str(
            r#"{"provider_input_tokens":298468,"request_estimate_tokens":850715}"#,
        )?;
        assert_eq!(Some(decoded), ContextAnchor::new(298_468, 850_715));
        assert!(
            serde_json::from_str::<ContextAnchor>(
                r#"{"provider_input_tokens":0,"request_estimate_tokens":850715}"#
            )
            .is_err(),
            "a zero provider count is not an anchor, on any decode path"
        );
        Ok(())
    }

    #[test]
    fn without_a_provider_report_occupancy_is_the_whole_estimate() {
        assert_eq!(context_occupancy_tokens(850_715, None), 850_715);
        assert_eq!(ContextAnchor::new(0, 850_715), None);
    }

    #[test]
    fn an_estimate_below_the_anchored_request_adds_no_suffix() {
        let anchor = ContextAnchor::new(10_000, 30_000);
        assert_eq!(context_occupancy_tokens(20_000, anchor), 10_000);
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::cast_possible_truncation,
    reason = "unit tests use expect for deterministic fixture clarity and measure short literal bodies"
)]
mod input_parts_tests {
    use super::ContextInputParts;

    #[test]
    fn responses_body_splits_instructions_tools_input_and_reasoning_items() {
        let body = br#"{"model":"m","instructions":"be brief","tools":[{"type":"function","name":"a"},{"type":"function","name":"b"}],"input":[{"role":"user","content":[{"type":"input_text","text":"hi"}]},{"type":"reasoning","encrypted_content":"xxxxxxxx"},{"type":"compaction","encrypted_content":"yyyy"}],"include":["reasoning.encrypted_content"]}"#;
        let parts = ContextInputParts::measure_wire(body).expect("object body");
        assert_eq!(parts.total_bytes(), body.len() as u64);
        assert_eq!(parts.system_bytes(), r#""be brief""#.len() as u64);
        assert_eq!(parts.tool_definition_count(), 2);
        assert_eq!(
            parts.tool_definition_bytes(),
            r#"[{"type":"function","name":"a"},{"type":"function","name":"b"}]"#.len() as u64
        );
        assert_eq!(
            parts.message_bytes(),
            r#"{"role":"user","content":[{"type":"input_text","text":"hi"}]}"#.len() as u64
        );
        assert_eq!(
            parts.reasoning_bytes(),
            (r#"{"type":"reasoning","encrypted_content":"xxxxxxxx"}"#.len()
                + r#"{"type":"compaction","encrypted_content":"yyyy"}"#.len()) as u64
        );
        let attributed = parts.system_bytes()
            + parts.tool_definition_bytes()
            + parts.message_bytes()
            + parts.reasoning_bytes();
        assert!(
            attributed < parts.total_bytes(),
            "envelope keys stay unattributed"
        );
        assert_eq!(
            parts.occupancy_tokens(),
            parts.system_bytes() / 4
                + parts.tool_definition_bytes() / 4
                + parts.message_bytes() / 4
                + parts.reasoning_bytes() / 4
                + (parts.total_bytes() - attributed) / 4
        );
    }

    #[test]
    fn messages_body_moves_thinking_blocks_out_of_the_message_share() {
        let body = br#"{"model":"m","system":[{"type":"text","text":"sys"}],"tools":[{"name":"t"}],"messages":[{"role":"user","content":"q"},{"role":"assistant","content":[{"type":"thinking","thinking":"hmm","signature":"s"},{"type":"text","text":"a"}]}],"thinking":{"type":"adaptive"}}"#;
        let parts = ContextInputParts::measure_wire(body).expect("object body");
        assert_eq!(
            parts.system_bytes(),
            r#"[{"type":"text","text":"sys"}]"#.len() as u64
        );
        assert_eq!(parts.tool_definition_count(), 1);
        let thinking = r#"{"type":"thinking","thinking":"hmm","signature":"s"}"#.len() as u64;
        assert_eq!(parts.reasoning_bytes(), thinking);
        let assistant = r#"{"role":"assistant","content":[{"type":"thinking","thinking":"hmm","signature":"s"},{"type":"text","text":"a"}]}"#.len() as u64;
        let user = r#"{"role":"user","content":"q"}"#.len() as u64;
        assert_eq!(parts.message_bytes(), user + assistant - thinking);
    }

    #[test]
    fn chat_body_counts_system_role_messages_as_system_and_omits_nothing_twice() {
        let body = br#"{"model":"m","messages":[{"role":"system","content":"sys"},{"role":"user","content":"q"},{"role":"assistant","content":"a","reasoning":"why"}],"tools":[]}"#;
        let parts = ContextInputParts::measure_wire(body).expect("object body");
        assert_eq!(
            parts.system_bytes(),
            r#"{"role":"system","content":"sys"}"#.len() as u64
        );
        assert_eq!(parts.tool_definition_count(), 0);
        assert_eq!(parts.tool_definition_bytes(), 2);
        assert_eq!(parts.reasoning_bytes(), r#""why""#.len() as u64);
        let assistant = r#"{"role":"assistant","content":"a","reasoning":"why"}"#.len() as u64;
        let user = r#"{"role":"user","content":"q"}"#.len() as u64;
        assert_eq!(
            parts.message_bytes(),
            user + assistant - r#""why""#.len() as u64
        );
    }

    #[test]
    fn a_non_object_body_is_unmeasured_rather_than_a_zero_split() {
        assert!(ContextInputParts::measure_wire(b"[1,2]").is_none());
        assert!(ContextInputParts::measure_wire(b"not json").is_none());
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "unit tests use expect for deterministic fixture clarity"
)]
mod tests {
    use std::collections::BTreeMap;
    use std::num::NonZeroU32;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use proptest::prelude::*;
    use uuid::Uuid;

    use super::*;
    use crate::{
        Actor, AttemptId, CanonicalHistory, Capability, CapabilityConstraints, CapabilitySet,
        ContentBlock, ConversationItem, ConversationView, EffectiveProviderConfigurationId,
        EntryId, FactProvenance, FactSource, FeatureSupport, GenerationLimits, HistorySegment,
        HistorySegmentKind, LanguageModel, MAX_PREPARED_CALL_RECORD_BYTES, ModelError,
        ModelErrorKind, ModelPreparationContext, ModelPreparationError, ModelStream,
        PreparedCacheUse, PreparedCallRecord, PreparedModelCall, PreparedModelProjection,
        PreparedOutputLimit, PreparedTokenProjection, Provenance, RequestMetadata,
        ResolvedModelDescriptor, RunId, RuntimeModelAuthority, SelectedProviderConfigurationId,
        Support, TextBlock, TurnId, ValidatedRequestPlan, ValidationMode, WorkingProjection,
    };

    fn timestamp(seconds: u64) -> Timestamp {
        Timestamp::new(seconds, 0).expect("valid timestamp")
    }

    fn authority(value: &'static [u8]) -> Digest {
        Digest::sha256(DigestDomain::ModelBinding, 7, &[value]).expect("authority digest")
    }

    fn provider_configuration_ids() -> (
        SelectedProviderConfigurationId,
        EffectiveProviderConfigurationId,
    ) {
        let selected = Digest::sha256(
            DigestDomain::SelectedProviderConfiguration,
            5,
            &[b"context-test-selected"],
        )
        .expect("selected digest");
        let effective = Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"context-test-effective"],
        )
        .expect("effective digest");
        (
            SelectedProviderConfigurationId::new(selected).expect("selected identity"),
            EffectiveProviderConfigurationId::new(effective).expect("effective identity"),
        )
    }

    fn credential_authority() -> Digest {
        authority(b"credential-authority")
    }

    fn authority_class(account: Digest) -> Digest {
        RuntimeModelAuthority::authority_class_for(account, credential_authority())
            .expect("authority class")
    }

    fn request(max_output: Option<u32>) -> ModelRequest {
        request_with_input(ConversationView::default(), max_output)
    }

    fn request_with_input(input: ConversationView, max_output: Option<u32>) -> ModelRequest {
        let limits = GenerationLimits::new(max_output.and_then(NonZeroU32::new), Vec::new())
            .expect("generation limits");
        let (selected, effective) = provider_configuration_ids();
        ModelRequest::builder(
            selected,
            effective,
            input,
            RequestMetadata::for_turn(
                RunId::try_from(Uuid::from_u128(1)).expect("run ID"),
                TurnId::try_from(Uuid::from_u128(2)).expect("turn ID"),
                AttemptId::try_from(Uuid::from_u128(3)).expect("attempt ID"),
                timestamp(1),
            ),
        )
        .limits(limits)
        .build()
        .expect("request")
    }

    fn text_item(value: u128, actor: Actor, text: &str) -> ConversationItem {
        ConversationItem::new(
            EntryId::try_from(Uuid::from_u128(value)).expect("entry ID"),
            actor,
            vec![ContentBlock::Text(
                TextBlock::new(text, None).expect("text"),
            )],
            if actor == Actor::User {
                Provenance::UserInput
            } else {
                Provenance::ModelOutput
            },
        )
        .expect("item")
    }

    fn measurement_certificate(kind: CountCertificateKind) -> MeasurementCertificate {
        let source = if matches!(kind, CountCertificateKind::ReleaseVerifiedUpperBound { .. }) {
            ContextEvidenceSource::ReleaseCertificate
        } else {
            ContextEvidenceSource::BuiltInCatalog
        };
        let freshness =
            EvidenceFreshness::new(timestamp(1), Some(timestamp(100))).expect("freshness");
        if matches!(kind, CountCertificateKind::CustomEnvelopeUpperBound { .. }) {
            MeasurementCertificate::new(
                "fixture-counter",
                "cert-1",
                "complete-projection-counter",
                "method-1",
                source,
                kind,
                None,
                freshness,
            )
        } else {
            MeasurementCertificate::standard(
                &StandardIntegrationAuthority::release_owned(),
                "fixture-counter",
                "cert-1",
                "complete-projection-counter",
                "method-1",
                source,
                kind,
                None,
                freshness,
            )
        }
        .expect("measurement certificate")
    }

    fn contract(rule: CapacityRule, certificate: MeasurementCertificate) -> ContextContract {
        ContextContract::new(
            ProtocolId::new("responses-family").expect("protocol family"),
            "accounting-1",
            "projection-1",
            "reservation-1",
            CapacityCertificate::new(
                &StandardIntegrationAuthority::release_owned(),
                "fixture-capacity",
                "capacity-1",
                ContextEvidenceSource::BuiltInCatalog,
                rule,
                None,
                EvidenceFreshness::new(timestamp(1), Some(timestamp(100))).expect("freshness"),
            )
            .expect("capacity certificate"),
            vec![certificate],
        )
        .expect("context contract")
    }

    fn descriptor(
        rule: CapacityRule,
        certificate: MeasurementCertificate,
    ) -> ResolvedModelDescriptor {
        let provenance = FactProvenance::new(FactSource::TestFixture, timestamp(1), None);
        let mut features = BTreeMap::new();
        features.insert(
            Capability::Streaming,
            FeatureSupport::new(
                Support::Native,
                CapabilityConstraints::default(),
                provenance,
            ),
        );
        ResolvedModelDescriptor::new(
            &StandardIntegrationAuthority::release_owned(),
            ProtocolId::new("test-dialect").expect("protocol"),
            RouteId::new("test-route").expect("route"),
            ModelId::new("requested-model").expect("model"),
            Some(ModelId::new("effective-model").expect("reported model")),
            None,
            ModelAdapterProfileId::new("test-adapter").expect("adapter"),
            CapabilitySet::new(features, BTreeMap::new()),
        )
        .with_context_contract(contract(rule, certificate))
    }

    fn observation(tokens: u64) -> CountObservation {
        CountObservation::new("fixture-counter", "cert-1", tokens).expect("observation")
    }

    fn prepared(
        descriptor: &ResolvedModelDescriptor,
        request: &ModelRequest,
        account: Digest,
        observation: Option<CountObservation>,
    ) -> (ValidatedRequestPlan, PreparedModelProjection) {
        let plan = descriptor
            .validate_request(request.clone(), ValidationMode::Strict)
            .expect("validated plan");
        let authority = RuntimeModelAuthority::new(
            descriptor,
            account,
            credential_authority(),
            1,
            timestamp(1),
            Some(timestamp(100)),
        )
        .expect("runtime authority");
        let context = ModelPreparationContext::new(authority, timestamp(2));
        let wire = serde_json::to_vec(&request.model_input_identity()).expect("wire");
        let wire_digest = Digest::sha256(DigestDomain::Blob, 1, &[b"context-test-wire-v1", &wire])
            .expect("wire digest");
        let wire_size = u64::try_from(wire.len()).expect("wire size");
        let token_projection = PreparedTokenProjection::new(wire_digest, wire_size, observation)
            .expect("token projection");
        let output_limit = request.limits().max_output_tokens().map(|tokens| {
            PreparedOutputLimit::enforced_wire_maximum(
                tokens,
                "max_output_tokens",
                wire_digest,
                "test-lowering-1",
            )
            .expect("output limit")
        });
        let contract = descriptor.context_contract();
        let protocol_family = contract.map_or_else(
            || descriptor.protocol().clone(),
            |value| value.protocol_family().clone(),
        );
        let projection_revision =
            contract.map_or("projection-1", ContextContract::projection_revision);
        let accounting_revision =
            contract.map_or("accounting-1", ContextContract::accounting_revision);
        let projection = PreparedModelProjection::new(
            descriptor,
            &plan,
            &context,
            protocol_family,
            "test-lowering-1",
            projection_revision,
            accounting_revision,
            wire_digest,
            wire_size,
            token_projection,
            PreparedCacheUse::None,
            output_limit,
        )
        .expect("prepared projection");
        (plan, projection)
    }

    fn item_status(
        input: &ContextWireInput,
        anchor: Option<&ContextRequestAnchor>,
        binding: Digest,
        require_bound: bool,
    ) -> ContextStatus {
        let descriptor = descriptor(
            CapacityRule::SharedWindow {
                total: SafeLimit::Exact(1_000_000),
            },
            measurement_certificate(CountCertificateKind::ExactTokenizer),
        );
        let request = request(Some(128_000));
        prepared_status(
            &descriptor,
            &request,
            authority(b"account"),
            Some(observation(10)),
            timestamp(2),
        )
        .evaluate_wire_input(&request, input, anchor, binding, require_bound)
        .expect("item status")
    }

    fn wire_input(value: &serde_json::Value) -> ContextWireInput {
        ContextWireInput::measure_wire(
            &serde_json::to_vec(value).expect("wire"),
            Some(CONSERVATIVE_IMAGE_MAX_TOKENS),
        )
        .expect("item input")
    }

    #[test]
    fn anchor_requires_positive_input_but_allows_zero_output() {
        let input = wire_input(&serde_json::json!({"messages": []}));
        let make = |usage| {
            ContextRequestAnchor::new(
                authority(b"binding"),
                RouteId::new("test-route").expect("route"),
                ModelId::new("requested-model").expect("model"),
                usage,
                input.clone(),
            )
        };
        assert!(make(crate::Usage::new(0, 100, 0, 0, 0)).is_none());
        assert!(make(crate::Usage::new(1, 0, 0, 0, 0)).is_some());
    }

    #[test]
    fn completed_response_headroom_requires_the_item_admission_binding() {
        let input = wire_input(&serde_json::json!({"messages": []}));
        let status = item_status(&input, None, authority(b"binding"), false);
        let mut anchor = ContextRequestAnchor::new(
            authority(b"binding"),
            RouteId::new("test-route").expect("route"),
            ModelId::new("requested-model").expect("model"),
            crate::Usage::new(3000, 100, 0, 0, 0),
            input,
        )
        .expect("anchor");
        assert!(status.completed_response_headroom(&anchor).is_some());
        anchor.binding = authority(b"another-binding");
        assert!(status.completed_response_headroom(&anchor).is_none());
    }

    #[test]
    fn anchored_bound_uses_the_tighter_wire_bound_and_small_envelope_margin() {
        let input =
            wire_input(&serde_json::json!({"messages": [{"role":"user","content":"hello"}]}));
        let binding = authority(b"binding");
        let anchor = ContextRequestAnchor::new(
            binding,
            RouteId::new("test-route").expect("route"),
            ModelId::new("requested-model").expect("model"),
            crate::Usage::new(900_000, 0, 0, 0, 0),
            input.clone(),
        )
        .expect("anchor");
        let status = item_status(&input, Some(&anchor), binding, true);
        assert_eq!(
            status.local_input_upper_bound(),
            Some(input.upper_bound() + context_admission_margin(1_000_000))
        );
        assert_eq!(context_admission_margin(2048), 204);
    }

    #[test]
    fn only_a_declared_same_served_model_anchors_item_admission() {
        let input = wire_input(&serde_json::json!({
            "messages": [{"role": "user", "content": "x".repeat(20_000)}]
        }));
        let declared = |served: &[&str]| {
            descriptor(
                CapacityRule::SharedWindow {
                    total: SafeLimit::Exact(1_000_000),
                },
                measurement_certificate(CountCertificateKind::ExactTokenizer),
            )
            .with_same_served_models(
                served
                    .iter()
                    .map(|model| ModelId::new(*model).expect("model")),
            )
        };
        let measure = |descriptor: &ResolvedModelDescriptor, served: &str| {
            let binding = descriptor.semantic_binding_digest().expect("binding");
            let anchor = ContextRequestAnchor::new(
                binding,
                RouteId::new("test-route").expect("route"),
                ModelId::new(served).expect("model"),
                crate::Usage::new(1_000, 0, 0, 0, 0),
                input.clone(),
            )
            .expect("anchor");
            let request = request(Some(128_000));
            let status = prepared_status(
                descriptor,
                &request,
                authority(b"account"),
                Some(observation(10)),
                timestamp(2),
            )
            .evaluate_wire_input(&request, &input, Some(&anchor), binding, true)
            .expect("item status");
            (
                anchor.matches_descriptor(descriptor),
                status.completed_response_headroom(&anchor).is_some(),
                status.local_input_upper_bound(),
            )
        };
        let margin = context_admission_margin(1_000_000);
        let anchored = Some(1_000 + margin);
        let whole_wire = Some(input.upper_bound() + margin);
        assert!(anchored < whole_wire);

        let variant = declared(&["served-variant"]);
        assert_eq!(measure(&variant, "served-variant"), (true, true, anchored));
        assert_eq!(measure(&variant, "requested-model"), (true, true, anchored));
        assert_eq!(measure(&variant, "effective-model"), (true, true, anchored));
        assert_eq!(
            measure(&variant, "undeclared-variant"),
            (false, false, whole_wire)
        );
        assert_eq!(
            measure(&declared(&[]), "served-variant"),
            (false, false, whole_wire)
        );
    }

    #[test]
    fn messages_cache_breakpoint_does_not_reprice_the_previous_result() {
        let old = wire_input(
            &serde_json::json!({"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"a","content":"x".repeat(10_000),"cache_control":{"type":"ephemeral"}}]}]}),
        );
        let new = wire_input(
            &serde_json::json!({"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"a","content":"x".repeat(10_000)}]}, {"role":"assistant","content":[{"type":"text","text":"next","cache_control":{"type":"ephemeral"}}]}]}),
        );
        let anchor = ContextRequestAnchor::new(
            authority(b"binding"),
            RouteId::new("test-route").expect("route"),
            ModelId::new("requested-model").expect("model"),
            crate::Usage::new(3000, 100, 0, 0, 0),
            old,
        )
        .expect("anchor");
        assert_eq!(anchor.occupied_tokens(&new), 3100);
        let data = |value| {
            wire_input(
                &serde_json::json!({"messages":[{"role":"user","content":[{"type":"tool_result","content":{"cache_control":value}}]}]}),
            )
        };
        assert_ne!(
            data("first").items[0].digest,
            data("second").items[0].digest
        );
    }

    #[test]
    fn large_item_anchor_encoding_is_compact_and_tool_arguments_are_not_images() {
        let messages = (0..6000)
            .map(|i| serde_json::json!({"role":"user", "content": i.to_string()}))
            .collect::<Vec<_>>();
        let input = wire_input(&serde_json::json!({"messages":messages}));
        let anchor = ContextRequestAnchor::new(
            authority(b"binding"),
            RouteId::new("test-route").expect("route"),
            ModelId::new("requested-model").expect("model"),
            crate::Usage::new(30_000, 0, 0, 0, 0),
            input.clone(),
        )
        .expect("anchor");
        let encoded = serde_json::to_vec(&anchor).expect("encode");
        assert!(encoded.len() < 1024 * 1024, "{} bytes", encoded.len());
        assert_eq!(
            serde_json::from_slice::<ContextRequestAnchor>(&encoded).expect("restore"),
            anchor
        );
        let arguments = serde_json::json!({"messages":[{"role":"assistant","content":[{"type":"tool_use","input":{"type":"image","payload":"x".repeat(10_000)}}]}]});
        let measured =
            ContextWireInput::measure_wire(&serde_json::to_vec(&arguments).expect("json"), None)
                .expect("not an image block");
        assert!(measured.upper_bound() > 10_000);
    }

    #[test]
    fn prepared_record_with_six_thousand_items_retains_only_an_anchor_reference() {
        let descriptor = descriptor(
            CapacityRule::SharedWindow {
                total: SafeLimit::Exact(1_000_000),
            },
            measurement_certificate(CountCertificateKind::ExactTokenizer),
        );
        let request = request(Some(128_000));
        let (plan, _) = prepared(
            &descriptor,
            &request,
            authority(b"account"),
            Some(observation(10)),
        );
        let authority = RuntimeModelAuthority::new(
            &descriptor,
            authority(b"account"),
            credential_authority(),
            1,
            timestamp(1),
            Some(timestamp(100)),
        )
        .expect("authority");
        let messages = (0..6000)
            .map(|i| serde_json::json!({"role":"user","content":i.to_string()}))
            .collect::<Vec<_>>();
        let body = serde_json::to_vec(&serde_json::json!({"messages":messages})).expect("body");
        let input = ContextWireInput::measure_wire(&body, None).expect("input");
        let anchor = ContextRequestAnchor::new(
            descriptor.semantic_binding_digest().expect("binding"),
            descriptor.route().clone(),
            descriptor.requested_model().clone(),
            crate::Usage::new(30_000, 0, 0, 0, 0),
            input,
        )
        .expect("anchor");
        let context =
            ModelPreparationContext::new(authority, timestamp(2)).with_request_anchor(Some(anchor));
        let digest = Digest::sha256(DigestDomain::Blob, 1, &[&body]).expect("wire digest");
        let size = u64::try_from(body.len()).expect("size");
        let tokens = PreparedTokenProjection::new(digest, size, Some(observation(10)))
            .expect("tokens")
            .with_wire_input(&body, None, &context)
            .expect("items");
        let projection = PreparedModelProjection::new(
            &descriptor,
            &plan,
            &context,
            descriptor
                .context_contract()
                .expect("contract")
                .protocol_family()
                .clone(),
            "test-lowering-1",
            "projection-1",
            "accounting-1",
            digest,
            size,
            tokens,
            PreparedCacheUse::None,
            Some(
                PreparedOutputLimit::enforced_wire_maximum(
                    NonZeroU32::new(128_000).expect("output"),
                    "max_output_tokens",
                    digest,
                    "test-lowering-1",
                )
                .expect("limit"),
            ),
        )
        .expect("projection");
        let record = PreparedCallRecord::for_prepared(&descriptor, &plan, projection, &context)
            .expect("prepared record");
        let encoded = serde_json::to_vec(&record).expect("encode");
        assert!(encoded.len() < 1024 * 1024, "{} bytes", encoded.len());
        let restored = serde_json::from_slice::<PreparedCallRecord>(&encoded).expect("restore");
        assert_eq!(restored, record);
        assert_eq!(serde_json::to_vec(&restored).expect("reencode"), encoded);
        assert!(
            !String::from_utf8(encoded)
                .expect("utf8")
                .contains("\"request_anchor\":")
        );
        let oversized = ModelError::from(ModelPreparationError::from(
            crate::PreparedCallError::RecordTooLarge,
        ));
        assert_eq!(oversized.kind(), ModelErrorKind::ContextLimit);
    }

    #[test]
    fn small_ceiling_trigger_caps_the_floor_and_catalog_limit_at_round_headroom() {
        let rule = CapacityRule::ProviderReservedInput {
            reported_context_window: Some(20_000),
            maximum_projected_input: 20_000,
            auto_compact_input: Some(19_000),
        };
        assert_eq!(rule.compaction_trigger_tokens(0, 6000, 8192), 15_904);
        assert_eq!(rule.compaction_trigger_tokens(0, 1000, 1000), 15_904);
        let rule = CapacityRule::SharedWindow {
            total: SafeLimit::Exact(40_000),
        };
        assert_eq!(rule.compaction_trigger_tokens(0, 20_000, 8192), 32_288);
        assert_eq!(rule.compaction_trigger_tokens(4096, 20_000, 8192), 31_808);
    }

    #[test]
    fn item_delta_anchor_counts_dynamic_system_changes_but_not_old_input_or_response_twice() {
        let binding = authority(b"binding");
        let huge = "x".repeat(850_715 * 4);
        let old = wire_input(
            &serde_json::json!({"instructions":"clock=1", "input":[{"role":"user", "content":huge}]}),
        );
        let usage = crate::Usage::new(298_468, 1_000, 0, 100_000, 20_000);
        let mut anchor = ContextRequestAnchor::new(
            binding,
            RouteId::new("test-route").expect("route"),
            ModelId::new("requested-model").expect("model"),
            usage,
            old,
        )
        .expect("anchor");
        let encoded = serde_json::to_value(&anchor).expect("anchor wire");
        assert_eq!(
            serde_json::from_value::<ContextRequestAnchor>(encoded.clone())
                .expect("anchor restore"),
            anchor
        );
        let mut zero_usage = encoded;
        zero_usage["usage"] =
            serde_json::to_value(crate::Usage::new(0, 0, 0, 0, 0)).expect("zero usage");
        assert!(serde_json::from_value::<ContextRequestAnchor>(zero_usage).is_err());
        let current = wire_input(
            &serde_json::json!({"instructions":"clock=2", "input":[{"role":"user", "content":huge}, {"role":"assistant", "content":"a".repeat(4000)}, {"type":"function_call_output", "output":"tool-result"}]}),
        );
        let authority_identity = anchor.identity().expect("authority identity");
        anchor.observe_response_items(&current);
        assert_eq!(
            anchor.identity().expect("cached identity"),
            authority_identity
        );
        let new_system = json_bytes(&serde_json::json!("clock=2"));
        let result =
            json_bytes(&serde_json::json!({"type":"function_call_output", "output":"tool-result"}));
        assert_eq!(
            anchor.occupied_tokens(&current),
            298_468 + 1_000 + new_system + result + 32
        );
        let status = item_status(&current, Some(&anchor), binding, false);
        assert!(matches!(status.kind(), ContextStatusKind::Fits { .. }));
        assert_eq!(
            status.occupancy_tokens(),
            Some(anchor.occupied_tokens(&current))
        );
        assert!(!status.uses_unanchored_estimate());
        // A later zero-usage response cannot spend the prior output credit.
        let later = wire_input(
            &serde_json::json!({"instructions":"clock=2", "input":[{"role":"user", "content":huge}, {"role":"assistant", "content":"changed-output"}, {"type":"function_call_output", "output":"tool-result"}, {"role":"assistant", "content":"later-response"}]}),
        );
        assert!(anchor.occupied_tokens(&later) > 299_468 + new_system + result + 32);
    }

    #[test]
    fn item_delta_removal_gives_no_credit_and_duplicate_items_are_new() {
        let binding = authority(b"binding");
        let old = wire_input(&serde_json::json!({"system":["one","two"],"messages":[]}));
        let anchor = ContextRequestAnchor::new(
            binding,
            RouteId::new("test-route").expect("route"),
            ModelId::new("requested-model").expect("model"),
            crate::Usage::new(100, 0, 0, 0, 0),
            old,
        )
        .expect("anchor");
        assert_eq!(
            anchor.occupied_tokens(&wire_input(
                &serde_json::json!({"system":["one"],"messages":[]})
            )),
            100
        );
        assert_eq!(
            anchor.occupied_tokens(&wire_input(
                &serde_json::json!({"system":["one","one"],"messages":[]})
            )),
            100 + 5 + 16
        );
    }

    #[test]
    fn unanchored_resume_import_and_model_switch_delegate_a_plausible_big_request() {
        let input = wire_input(
            &serde_json::json!({"input":[{"role":"user","content":"x".repeat(850_715 * 4)}]}),
        );
        let old_binding = authority(b"old-binding");
        let binding = authority(b"new-binding");
        let anchor = ContextRequestAnchor::new(
            old_binding,
            RouteId::new("old-route").expect("route"),
            ModelId::new("old-model").expect("model"),
            crate::Usage::new(298_468, 1000, 0, 0, 0),
            input.clone(),
        )
        .expect("anchor");
        let mut foreign_model = anchor.clone();
        foreign_model.binding = binding;
        foreign_model.route = RouteId::new("test-route").expect("route");
        let mut foreign_route = foreign_model.clone();
        foreign_route.model = ModelId::new("requested-model").expect("model");
        foreign_route.route = RouteId::new("foreign-route").expect("route");
        for anchor in [
            None,
            Some(&anchor),
            Some(&foreign_model),
            Some(&foreign_route),
        ] {
            let status = item_status(&input, anchor, binding, false);
            assert!(matches!(status.kind(), ContextStatusKind::Fits { .. }));
            assert!(status.uses_unanchored_estimate());
            assert!(matches!(
                status.admit_with_requirement(
                    &request(Some(128_000)),
                    ContextAdmissionRequirement::ReleaseVerifiedRequired
                ),
                Err(ContextAdmissionError::Unavailable(_))
            ));
        }
        let too_large = wire_input(
            &serde_json::json!({"input":[{"role":"user","content":"x".repeat(900_000 * 4)}]}),
        );
        assert!(matches!(
            item_status(&too_large, None, binding, false).kind(),
            ContextStatusKind::ProtectedOverflow(_)
        ));
    }

    #[test]
    fn fold_input_requires_a_bound_even_when_a_huge_tool_result_estimate_fits() {
        let binding = authority(b"binding");
        let huge = wire_input(
            &serde_json::json!({"input":[{"type":"function_call_output","output":"x".repeat(850_715 * 4)}]}),
        );
        assert!(matches!(
            item_status(&huge, None, binding, false).kind(),
            ContextStatusKind::Fits { .. }
        ));
        assert!(matches!(
            item_status(&huge, None, binding, true).kind(),
            ContextStatusKind::ProtectedOverflow(_)
        ));
        let elided = wire_input(
            &serde_json::json!({"input":[{"type":"function_call_output","output":"tool result elided"}]}),
        );
        assert!(matches!(
            item_status(&elided, None, binding, true).kind(),
            ContextStatusKind::Fits { .. }
        ));
    }

    #[test]
    fn requested_output_is_reserved_once_only_on_shared_windows() {
        let binding = authority(b"binding");
        let input = wire_input(
            &serde_json::json!({"input":[{"role":"user","content":"q".repeat(900_000)}]}),
        );
        let anchor = ContextRequestAnchor::new(
            binding,
            RouteId::new("test-route").expect("route"),
            ModelId::new("requested-model").expect("model"),
            crate::Usage::new(850_000, 10_000, 0, 0, 0),
            input.clone(),
        )
        .expect("anchor");
        assert!(matches!(
            item_status(&input, Some(&anchor), binding, false).kind(),
            ContextStatusKind::Fits {
                headroom: ContextHeadroom::Shared(QualifiedTokens::GuaranteedMinimum(2000))
            }
        ));
        assert!(
            evaluate_rule(
                CapacityRule::ProviderReservedInput {
                    reported_context_window: Some(1_000_000),
                    maximum_projected_input: 1_000_000,
                    auto_compact_input: None
                },
                CountBound::Exact(900_000),
                Some(128_000)
            )
            .is_ok()
        );
    }

    #[test]
    fn image_input_uses_the_dialect_bound_not_base64_and_digests_exact_wire_items() {
        let small = wire_input(
            &serde_json::json!({"messages":[{"role":"user","content":[{"type":"image","source":{"type":"base64","data":"abcd"}}]}]}),
        );
        let huge = wire_input(
            &serde_json::json!({"messages":[{"role":"user","content":[{"type":"image","source":{"type":"base64","data":"a".repeat(4_000_000)}}]}]}),
        );
        assert_eq!(small.upper_bound(), huge.upper_bound());
        assert_eq!(small.estimate_tokens(), huge.estimate_tokens());
        let small_body = serde_json::to_vec(&serde_json::json!({"messages":[{"role":"user","content":[{"type":"image","source":{"type":"base64","data":"abcd"}}]}]})).expect("small body");
        let huge_body = serde_json::to_vec(&serde_json::json!({"messages":[{"role":"user","content":[{"type":"image","source":{"type":"base64","data":"a".repeat(4_000_000)}}]}]})).expect("huge body");
        let small_parts = ContextInputParts::measure_wire(&small_body)
            .expect("small parts")
            .with_image_tokens(small.image_tokens());
        let huge_parts = ContextInputParts::measure_wire(&huge_body)
            .expect("huge parts")
            .with_image_tokens(huge.image_tokens());
        assert_eq!(
            small_parts.occupancy_tokens(),
            huge_parts.occupancy_tokens()
        );
        assert_eq!(small_parts.message_bytes(), huge_parts.message_bytes());
        assert!(huge_parts.image_transport_bytes() > 3_999_000);
        assert_ne!(small.items[0].digest, huge.items[0].digest);
        assert!(small.upper_bound() >= CONSERVATIVE_IMAGE_MAX_TOKENS);
        let first =
            ContextWireInput::measure_wire(br#"{"input":[{"role":"user","content":"q"}]}"#, None)
                .expect("wire");
        let reordered =
            ContextWireInput::measure_wire(br#"{"input":[{"content":"q","role":"user"}]}"#, None)
                .expect("wire");
        assert_ne!(first.items[0].digest, reordered.items[0].digest);
    }

    fn prepared_status(
        descriptor: &ResolvedModelDescriptor,
        request: &ModelRequest,
        account: Digest,
        observation: Option<CountObservation>,
        now: Timestamp,
    ) -> ContextStatus {
        let (plan, projection) = prepared(descriptor, request, account, observation);
        descriptor
            .context_status(&plan, &projection, now)
            .expect("context status")
    }

    struct InspectingModel {
        descriptor: ResolvedModelDescriptor,
        authority: RuntimeModelAuthority,
        observation: Option<CountObservation>,
        input_parts: Option<ContextInputParts>,
        prepares: Arc<AtomicUsize>,
        starts: Arc<AtomicUsize>,
    }

    impl InspectingModel {
        fn new(descriptor: ResolvedModelDescriptor, observation: Option<CountObservation>) -> Self {
            let authority = RuntimeModelAuthority::new(
                &descriptor,
                authority(b"inspection-account"),
                credential_authority(),
                1,
                timestamp(1),
                Some(timestamp(100)),
            )
            .expect("inspection authority");
            Self {
                descriptor,
                authority,
                observation,
                input_parts: None,
                prepares: Arc::new(AtomicUsize::new(0)),
                starts: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn prepare(
            self: &Arc<Self>,
            request: ModelRequest,
        ) -> Result<PreparedModelCall, ModelPreparationError> {
            let plan = self
                .descriptor
                .validate_request(request, ValidationMode::Strict)
                .expect("inspection plan");
            let context = ModelPreparationContext::new(self.authority.clone(), timestamp(2));
            let model: Arc<dyn LanguageModel> = self.clone();
            model.prepare_call(plan, context)
        }
    }

    impl LanguageModel for InspectingModel {
        fn prepare_call(
            self: Arc<Self>,
            plan: ValidatedRequestPlan,
            context: ModelPreparationContext,
        ) -> Result<PreparedModelCall, ModelPreparationError> {
            self.prepares.fetch_add(1, Ordering::AcqRel);
            self.descriptor.validate_plan_binding(&plan)?;
            context
                .authority()
                .validate(&self.descriptor, context.prepared_at())?;
            let wire = serde_json::to_vec(&plan.request().model_input_identity())
                .expect("inspection wire");
            let wire_digest =
                Digest::sha256(DigestDomain::Blob, 1, &[b"inspection-wire-v1", &wire])
                    .expect("inspection wire digest");
            let token_projection = PreparedTokenProjection::new(
                wire_digest,
                u64::try_from(wire.len()).expect("inspection wire size"),
                self.observation.clone(),
            )?
            .with_input_parts(self.input_parts);
            let output_limit = plan.request().limits().max_output_tokens().map(|tokens| {
                PreparedOutputLimit::enforced_wire_maximum(
                    tokens,
                    "max_output_tokens",
                    wire_digest,
                    "inspection-lowering-1",
                )
                .expect("inspection output limit")
            });
            let contract = self.descriptor.context_contract();
            let projection = PreparedModelProjection::new(
                &self.descriptor,
                &plan,
                &context,
                contract.map_or_else(
                    || self.descriptor.protocol().clone(),
                    |value| value.protocol_family().clone(),
                ),
                "inspection-lowering-1",
                contract.map_or(
                    "inspection-projection-1",
                    ContextContract::projection_revision,
                ),
                contract.map_or(
                    "inspection-accounting-1",
                    ContextContract::accounting_revision,
                ),
                wire_digest,
                u64::try_from(wire.len()).expect("inspection wire size"),
                token_projection,
                PreparedCacheUse::None,
                output_limit,
            )?;
            let record =
                PreparedCallRecord::for_prepared(&self.descriptor, &plan, projection, &context)?;
            let starts = Arc::clone(&self.starts);
            Ok(PreparedModelCall::new(record, move |_| {
                starts.fetch_add(1, Ordering::AcqRel);
                let stream: ModelStream = Box::pin(futures_util::stream::empty());
                Box::pin(async move { Ok(stream) })
            }))
        }
    }

    fn eligible_overflow_request() -> ModelRequest {
        let input = ConversationView::from_shared(
            CanonicalHistory::new(vec![
                HistorySegment::new(
                    HistorySegmentKind::RecentIntent,
                    vec![text_item(20, Actor::User, "older")],
                )
                .expect("older intent"),
                HistorySegment::new(
                    HistorySegmentKind::Conversation,
                    vec![text_item(21, Actor::Assistant, "older answer")],
                )
                .expect("older answer"),
                HistorySegment::new(
                    HistorySegmentKind::RecentIntent,
                    vec![text_item(22, Actor::User, "latest")],
                )
                .expect("latest intent"),
            ])
            .expect("eligible history"),
            WorkingProjection::default(),
        );
        request_with_input(input, Some(20))
    }

    #[test]
    fn ordinary_prepare_exposes_only_the_exact_admitted_status_identity_before_start() {
        let descriptor = descriptor(
            CapacityRule::SharedWindow {
                total: SafeLimit::Exact(100),
            },
            measurement_certificate(CountCertificateKind::ExactTokenizer),
        );
        let model = Arc::new(InspectingModel::new(descriptor, Some(observation(60))));
        let prepared = model.prepare(request(Some(20))).expect("fits");
        assert_eq!(model.prepares.load(Ordering::Acquire), 1);
        assert_eq!(model.starts.load(Ordering::Acquire), 0);
        assert_eq!(
            prepared.record().context_status_identity().domain(),
            DigestDomain::ContextAdmission
        );
        assert_eq!(prepared.record().context_status_identity().version(), 3);
        assert_eq!(prepared.record().context_admission_identity().version(), 4);
        assert_eq!(prepared.record().identity().version(), 9);
        let encoded = serde_json::to_vec(prepared.record()).expect("prepared record encode");
        assert!(encoded.len() <= MAX_PREPARED_CALL_RECORD_BYTES);
        assert!(
            !encoded
                .windows(b"Fits".len())
                .any(|window| window == b"Fits")
        );
        let decoded: PreparedCallRecord =
            serde_json::from_slice(&encoded).expect("prepared record decode");
        assert_eq!(decoded, *prepared.record());
        drop(prepared);
        assert_eq!(model.starts.load(Ordering::Acquire), 0);
    }

    #[test]
    fn prepared_projection_binds_required_provider_configuration_ids() {
        let descriptor = descriptor(
            CapacityRule::SharedWindow {
                total: SafeLimit::Exact(100),
            },
            measurement_certificate(CountCertificateKind::ExactTokenizer),
        );
        let request = request(Some(20));
        let (plan, projection) = prepared(
            &descriptor,
            &request,
            authority(b"projection-account"),
            Some(observation(60)),
        );
        assert_eq!(
            projection.selected_provider_configuration_id(),
            request.selected_provider_configuration_id()
        );
        assert_eq!(
            projection.effective_provider_configuration_id(),
            request.effective_provider_configuration_id()
        );
        projection
            .validate_for_plan(&descriptor, &plan)
            .expect("matching plan");

        let encoded = serde_json::to_value(&projection).expect("projection encode");
        let decoded: PreparedModelProjection =
            serde_json::from_value(encoded.clone()).expect("projection decode");
        assert_eq!(decoded, projection);

        let mut missing = encoded.clone();
        missing
            .as_object_mut()
            .expect("projection object")
            .remove("effective_provider_configuration");
        assert!(serde_json::from_value::<PreparedModelProjection>(missing).is_err());

        let mut swapped = encoded;
        let object = swapped.as_object_mut().expect("projection object");
        let selected = object["selected_provider_configuration"].clone();
        let effective = object["effective_provider_configuration"].clone();
        object.insert("selected_provider_configuration".into(), effective);
        object.insert("effective_provider_configuration".into(), selected);
        assert!(serde_json::from_value::<PreparedModelProjection>(swapped).is_err());
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one preparation matrix proves complete status/source identity and zero-start behavior for every closed denial kind"
    )]
    fn ordinary_prepare_retains_complete_exact_status_for_every_denial_before_start() {
        let mut unavailable_features = BTreeMap::new();
        unavailable_features.insert(
            Capability::Streaming,
            FeatureSupport::new(
                Support::Native,
                CapabilityConstraints::default(),
                FactProvenance::new(FactSource::TestFixture, timestamp(1), None),
            ),
        );
        let unavailable_model = Arc::new(InspectingModel::new(
            ResolvedModelDescriptor::new(
                &StandardIntegrationAuthority::release_owned(),
                ProtocolId::new("private-dialect").expect("protocol"),
                RouteId::new("private-route").expect("route"),
                ModelId::new("private-model").expect("model"),
                None,
                None,
                ModelAdapterProfileId::new("private-adapter").expect("adapter"),
                CapabilitySet::new(unavailable_features, BTreeMap::new()),
            ),
            None,
        ));
        let unavailable_request = request(Some(20));
        let unavailable = unavailable_model
            .prepare(unavailable_request.clone())
            .expect_err("missing contract must deny");
        let unavailable_status = unavailable
            .context_status()
            .expect("complete unavailable status");
        assert_eq!(
            unavailable_status.kind(),
            &ContextStatusKind::Unavailable(ContextUnavailable::ContractUnavailable)
        );
        assert_eq!(
            unavailable.context_admission_error(),
            Some(&ContextAdmissionError::Unavailable(
                ContextUnavailable::ContractUnavailable
            ))
        );
        assert_eq!(
            unavailable_status.model_input_identity(),
            unavailable_request.model_input_identity()
        );
        assert!(unavailable_status.scope().is_none());
        assert_eq!(unavailable_model.prepares.load(Ordering::Acquire), 1);
        assert_eq!(unavailable_model.starts.load(Ordering::Acquire), 0);
        let debug = format!("{unavailable:?}");
        assert!(debug.contains("<complete context status retained>"));
        assert!(!debug.contains("private-route"));
        assert!(!debug.contains("private-model"));
        assert_eq!(
            ModelError::from(unavailable.clone()).kind(),
            ModelErrorKind::InvalidRequest
        );

        let overflow_descriptor = descriptor(
            CapacityRule::Separate {
                input: SafeLimit::Exact(50),
                output: SafeLimit::Exact(10),
            },
            measurement_certificate(CountCertificateKind::ExactTokenizer),
        );
        for (request, expected_protected) in [
            (eligible_overflow_request(), false),
            (request(Some(20)), true),
        ] {
            let model = Arc::new(InspectingModel::new(
                overflow_descriptor.clone(),
                Some(observation(60)),
            ));
            let error = model
                .prepare(request.clone())
                .expect_err("overflow must deny");
            let status = error.context_status().expect("complete overflow status");
            assert_eq!(
                status.model_input_identity(),
                request.model_input_identity()
            );
            assert_eq!(
                status.descriptor_binding(),
                overflow_descriptor
                    .binding_digest()
                    .expect("descriptor binding")
            );
            let scope = status.scope().expect("overflow scope");
            assert_eq!(scope.protocol(), overflow_descriptor.protocol());
            assert_eq!(scope.route(), overflow_descriptor.route());
            assert_eq!(
                scope.requested_model(),
                overflow_descriptor.requested_model()
            );
            assert_eq!(scope.lowering_revision(), "inspection-lowering-1");
            assert_eq!(scope.model_input_identity(), request.model_input_identity());
            assert_eq!(
                scope.prepared_token_projection(),
                status
                    .measurement()
                    .expect("measurement")
                    .scope()
                    .prepared_token_projection()
            );
            assert_eq!(
                status
                    .measurement()
                    .expect("measurement")
                    .bound()
                    .upper_bound(),
                60
            );
            assert_eq!(status.reservation().expect("reservation").tokens(), 20);
            let planner = status.planner_disposition().expect("planner disposition");
            match (
                expected_protected,
                status.kind(),
                error.context_admission_error(),
            ) {
                (
                    false,
                    ContextStatusKind::WouldOverflow(status_overflow),
                    Some(ContextAdmissionError::WouldOverflow(error_overflow)),
                ) => {
                    assert_eq!(status_overflow, error_overflow);
                    assert_eq!(
                        planner.kind(),
                        ContextPlannerDispositionKind::EligibleWholeGroups
                    );
                }
                (
                    true,
                    ContextStatusKind::ProtectedOverflow(status_overflow),
                    Some(ContextAdmissionError::ProtectedOverflow(error_overflow)),
                ) => {
                    assert_eq!(status_overflow, error_overflow);
                    assert_eq!(planner.kind(), ContextPlannerDispositionKind::ProtectedOnly);
                }
                other => panic!("mismatched retained status and denial: {other:?}"),
            }
            assert_eq!(model.prepares.load(Ordering::Acquire), 1);
            assert_eq!(model.starts.load(Ordering::Acquire), 0);
            assert_eq!(ModelError::from(error).kind(), ModelErrorKind::ContextLimit);
        }
    }

    #[test]
    fn exact_scope_and_attempt_metadata_exclusion_are_preserved() {
        let descriptor = descriptor(
            CapacityRule::SharedWindow {
                total: SafeLimit::Exact(100),
            },
            measurement_certificate(CountCertificateKind::ExactTokenizer),
        );
        let request = request(Some(20));
        let status = prepared_status(
            &descriptor,
            &request,
            authority(b"account-a"),
            Some(observation(70)),
            timestamp(2),
        );
        let scope = status.scope().expect("scope");
        assert_eq!(scope.protocol_family().as_str(), "responses-family");
        assert_eq!(scope.protocol().as_str(), "test-dialect");
        assert_eq!(scope.route().as_str(), "test-route");
        assert_eq!(scope.requested_model().as_str(), "requested-model");
        assert_eq!(scope.effective_model().as_str(), "effective-model");
        assert_eq!(scope.adapter().as_str(), "test-adapter");
        assert_eq!(scope.model_input_identity(), request.model_input_identity());
        assert!(matches!(
            status.kind(),
            ContextStatusKind::Fits {
                headroom: ContextHeadroom::Shared(QualifiedTokens::Exact(10))
            }
        ));
        assert!(status.planner_disposition().is_none());

        let metadata_only = request
            .for_round(RequestMetadata::for_turn(
                request.metadata().run_id(),
                request.metadata().turn_id().expect("turn-rooted fixture"),
                AttemptId::try_from(Uuid::from_u128(4)).expect("attempt ID"),
                timestamp(3),
            ))
            .expect("metadata-only request");
        assert_ne!(request.identity(), metadata_only.identity());
        assert_eq!(
            request.model_input_identity(),
            metadata_only.model_input_identity()
        );
        let metadata_status = prepared_status(
            &descriptor,
            &metadata_only,
            authority(b"account-a"),
            Some(observation(70)),
            timestamp(2),
        );
        assert_eq!(status.identity(), metadata_status.identity());
    }

    /// The fences and the gauge read part bytes/4, plus the provider input
    /// and suffix when anchored; records read the unanchored estimate (the
    /// bound without parts); the anchor never enters the status identity.
    #[test]
    fn status_occupancy_is_the_one_anchored_measure() {
        let descriptor = descriptor(
            CapacityRule::SharedWindow {
                total: SafeLimit::Exact(100),
            },
            measurement_certificate(CountCertificateKind::ExactTokenizer),
        );
        let status = prepared_status(
            &descriptor,
            &request(Some(20)),
            authority(b"account-a"),
            Some(observation(70)),
            timestamp(2),
        );
        assert_eq!(status.occupancy_tokens(), Some(70), "bound fallback");
        // Without parts the bound is on another scale: an anchor never
        // applies to it, and the fences see no estimate at all.
        let unparted = status
            .clone()
            .with_context_anchor(ContextAnchor::new(5, 10));
        assert_eq!(unparted.occupancy_estimate_tokens(), None);
        assert_eq!(unparted.occupancy_tokens(), Some(70));
        assert_eq!(unparted.unanchored_occupancy_tokens(), Some(70));
        let body = vec![b'x'; 4_000];
        let body = format!(
            r#"{{"input":"{}"}}"#,
            String::from_utf8(body).expect("ascii")
        );
        let parts = ContextInputParts::measure_wire(body.as_bytes()).expect("object body");
        let measured = status.clone().with_input_parts(Some(parts));
        let estimate = parts.occupancy_tokens();
        assert_eq!(measured.occupancy_estimate_tokens(), Some(estimate));
        assert_eq!(measured.occupancy_tokens(), Some(estimate));

        let anchor = ContextAnchor::new(300, estimate - 250);
        let anchored = measured.clone().with_context_anchor(anchor);
        assert_eq!(anchored.occupancy_tokens(), Some(300 + 250));
        assert_eq!(
            anchored.occupancy_tokens(),
            Some(context_occupancy_tokens(estimate, anchor))
        );
        assert_eq!(anchored.occupancy_estimate_tokens(), Some(estimate));
        assert_eq!(anchored.unanchored_occupancy_tokens(), Some(estimate));
        assert_eq!(anchored.identity(), status.identity());
    }

    /// An admission denial carries the byte parts its request was measured
    /// with, so a recovery reading of an overflowing request stays on the
    /// anchor's scale instead of falling back to the measurement bound.
    #[test]
    fn an_admission_denial_carries_the_measured_input_parts() {
        let overflow_descriptor = descriptor(
            CapacityRule::Separate {
                input: SafeLimit::Exact(50),
                output: SafeLimit::Exact(10),
            },
            measurement_certificate(CountCertificateKind::ExactTokenizer),
        );
        let body = format!(r#"{{"input":"{}"}}"#, "x".repeat(400));
        let parts = ContextInputParts::measure_wire(body.as_bytes()).expect("object body");
        let mut model = InspectingModel::new(overflow_descriptor, Some(observation(60)));
        model.input_parts = Some(parts);
        let error = Arc::new(model)
            .prepare(request(Some(20)))
            .expect_err("overflow must deny");
        let status = error.context_status().expect("complete overflow status");
        assert_eq!(status.input_parts(), Some(parts));
        assert_eq!(
            status.occupancy_estimate_tokens(),
            Some(parts.occupancy_tokens())
        );
        let anchor = ContextAnchor::new(7, parts.occupancy_tokens() - 3);
        assert_eq!(
            status
                .clone()
                .with_context_anchor(anchor)
                .occupancy_tokens(),
            Some(10)
        );
    }

    #[test]
    fn release_verified_upper_bound_and_lower_capacity_never_claim_exact_headroom() {
        let corpus =
            Digest::sha256(DigestDomain::TokenMeasurement, 99, &[b"proof-corpus"]).expect("corpus");
        let descriptor = descriptor(
            CapacityRule::SharedWindowWithOutputCap {
                total: SafeLimit::GuaranteedLowerBound(120),
                output: SafeLimit::Exact(40),
            },
            measurement_certificate(CountCertificateKind::ReleaseVerifiedUpperBound { corpus }),
        );
        let request = request(Some(30));
        let status = prepared_status(
            &descriptor,
            &request,
            authority(b"account-a"),
            Some(observation(80)),
            timestamp(2),
        );
        let measurement = status.measurement().expect("measurement");
        assert!(matches!(
            measurement.bound(),
            CountBound::ReleaseVerifiedUpperBound {
                tokens: 80,
                corpus: found,
                ..
            } if found == corpus
        ));
        assert!(matches!(
            status.kind(),
            ContextStatusKind::Fits {
                headroom: ContextHeadroom::SharedWithOutputCap {
                    shared: QualifiedTokens::GuaranteedMinimum(10),
                    output: QualifiedTokens::Exact(10),
                }
            }
        ));
    }

    #[test]
    fn missing_unknown_drifted_and_expired_evidence_fail_closed() {
        let request = request(Some(20));
        let provenance = FactProvenance::new(FactSource::TestFixture, timestamp(1), None);
        let mut features = BTreeMap::new();
        features.insert(
            Capability::Streaming,
            FeatureSupport::new(
                Support::Native,
                CapabilityConstraints::default(),
                provenance,
            ),
        );
        let plain = ResolvedModelDescriptor::new(
            &StandardIntegrationAuthority::release_owned(),
            ProtocolId::new("test-dialect").expect("protocol"),
            RouteId::new("test-route").expect("route"),
            ModelId::new("requested-model").expect("model"),
            None,
            None,
            ModelAdapterProfileId::new("test-adapter").expect("adapter"),
            CapabilitySet::new(features, BTreeMap::new()),
        );
        let missing = prepared_status(
            &plain,
            &request,
            authority(b"account-a"),
            None,
            timestamp(2),
        );
        assert_eq!(
            missing.kind(),
            &ContextStatusKind::Unavailable(ContextUnavailable::ContractUnavailable)
        );

        let descriptor = descriptor(
            CapacityRule::SharedWindow {
                total: SafeLimit::Exact(100),
            },
            measurement_certificate(CountCertificateKind::ExactTokenizer),
        );
        for (candidate, expected) in [
            (
                CountObservation::new("unknown-counter", "cert-1", 50).expect("candidate"),
                ContextUnavailable::UnknownMeasurementCertificate,
            ),
            (
                CountObservation::new("fixture-counter", "wrong-revision", 50).expect("candidate"),
                ContextUnavailable::MeasurementCertificateRevisionDrift,
            ),
        ] {
            let status = prepared_status(
                &descriptor,
                &request,
                authority(b"account-a"),
                Some(candidate),
                timestamp(2),
            );
            assert_eq!(status.kind(), &ContextStatusKind::Unavailable(expected));
        }

        let expired = prepared_status(
            &descriptor,
            &request,
            authority(b"account-a"),
            Some(observation(50)),
            timestamp(100),
        );
        assert_eq!(
            expired.kind(),
            &ContextStatusKind::Unavailable(ContextUnavailable::EvidenceExpired {
                evidence: "capacity"
            })
        );
    }

    #[test]
    fn provider_reserved_input_requires_a_positive_ceiling_within_the_reported_window() {
        let freshness =
            EvidenceFreshness::new(timestamp(1), Some(timestamp(10))).expect("freshness");
        let certificate = |rule| {
            CapacityCertificate::new(
                &StandardIntegrationAuthority::release_owned(),
                "capacity",
                "1",
                ContextEvidenceSource::BuiltInCatalog,
                rule,
                None,
                freshness,
            )
        };
        assert!(matches!(
            certificate(CapacityRule::ProviderReservedInput {
                reported_context_window: Some(1),
                maximum_projected_input: 0,
                auto_compact_input: None,
            }),
            Err(ContextContractError::ZeroLimit { .. })
        ));
        assert_eq!(
            certificate(CapacityRule::ProviderReservedInput {
                reported_context_window: Some(1),
                maximum_projected_input: 2,
                auto_compact_input: None,
            }),
            Err(ContextContractError::InvalidCapacityRelationship)
        );
    }

    #[test]
    fn invalid_or_product_shaped_evidence_never_reaches_descriptor_authority() {
        let freshness =
            EvidenceFreshness::new(timestamp(1), Some(timestamp(10))).expect("freshness");
        assert!(matches!(
            CapacityCertificate::new(
                &StandardIntegrationAuthority::release_owned(),
                "capacity",
                "1",
                ContextEvidenceSource::BuiltInCatalog,
                CapacityRule::SharedWindow {
                    total: SafeLimit::Exact(0)
                },
                None,
                freshness,
            ),
            Err(ContextContractError::ZeroLimit { .. })
        ));
        assert_eq!(
            CapacityCertificate::new(
                &StandardIntegrationAuthority::release_owned(),
                "capacity",
                "1",
                ContextEvidenceSource::AuthenticatedDiscovery,
                CapacityRule::SharedWindow {
                    total: SafeLimit::Exact(1)
                },
                None,
                freshness,
            ),
            Err(ContextContractError::UnscopedDiscovery)
        );
        assert_eq!(
            CapacityCertificate::new(
                &StandardIntegrationAuthority::release_owned(),
                "capacity",
                "1",
                ContextEvidenceSource::BuiltInCatalog,
                CapacityRule::SharedWindow {
                    total: SafeLimit::Exact(1)
                },
                Some(authority(b"foreign-authority")),
                freshness,
            ),
            Err(ContextContractError::UnexpectedAuthorityBinding)
        );
        assert_eq!(
            CapacityCertificate::new(
                &StandardIntegrationAuthority::release_owned(),
                "capacity",
                "1",
                ContextEvidenceSource::BuiltInCatalog,
                CapacityRule::ProviderReservedInput {
                    reported_context_window: Some(1),
                    maximum_projected_input: 1,
                    auto_compact_input: None,
                },
                None,
                freshness,
            ),
            Err(ContextContractError::EvidenceClassMismatch)
        );
        let corpus =
            Digest::sha256(DigestDomain::TokenMeasurement, 99, &[b"corpus"]).expect("corpus");
        assert_eq!(
            MeasurementCertificate::standard(
                &StandardIntegrationAuthority::release_owned(),
                "counter",
                "1",
                "bound",
                "1",
                ContextEvidenceSource::BuiltInCatalog,
                CountCertificateKind::ReleaseVerifiedUpperBound { corpus },
                None,
                freshness,
            ),
            Err(ContextContractError::UpperBoundNotReleaseVerified)
        );
        assert!(
            serde_json::from_str::<ContextEvidenceSource>(r#""UserOverride""#).is_err(),
            "product/user override is not an evidence authority variant"
        );
    }

    #[test]
    fn discovery_is_authority_scoped_and_explicit_reservation_is_mandatory() {
        let account_a = authority(b"account-a");
        let account_b = authority(b"account-b");
        let discovery_capacity = CapacityCertificate::new(
            &StandardIntegrationAuthority::release_owned(),
            "discovered-capacity",
            "1",
            ContextEvidenceSource::AuthenticatedDiscovery,
            CapacityRule::Separate {
                input: SafeLimit::Exact(80),
                output: SafeLimit::Exact(20),
            },
            Some(authority_class(account_a)),
            EvidenceFreshness::new(timestamp(1), Some(timestamp(10))).expect("freshness"),
        )
        .expect("capacity");
        let contract = ContextContract::new(
            ProtocolId::new("responses-family").expect("family"),
            "accounting-1",
            "projection-1",
            "reservation-1",
            discovery_capacity,
            vec![measurement_certificate(
                CountCertificateKind::ExactProviderCounter,
            )],
        )
        .expect("contract");
        let descriptor = descriptor(
            CapacityRule::SharedWindow {
                total: SafeLimit::Exact(1),
            },
            measurement_certificate(CountCertificateKind::ExactProviderCounter),
        )
        .with_context_contract(contract);
        let reserved_request = request(Some(10));
        let foreign = prepared_status(
            &descriptor,
            &reserved_request,
            account_b,
            Some(observation(40)),
            timestamp(2),
        );
        assert_eq!(
            foreign.kind(),
            &ContextStatusKind::Unavailable(ContextUnavailable::ScopeDrift {
                field: "capacity authority binding"
            })
        );

        let without_reservation = prepared_status(
            &descriptor,
            &request(None),
            account_a,
            Some(observation(40)),
            timestamp(2),
        );
        assert_eq!(
            without_reservation.kind(),
            &ContextStatusKind::Unavailable(
                ContextUnavailable::ExplicitOutputReservationUnavailable
            )
        );
    }

    #[test]
    fn overflow_uses_only_the_internally_derived_closed_history_plan() {
        let descriptor = descriptor(
            CapacityRule::Separate {
                input: SafeLimit::Exact(50),
                output: SafeLimit::Exact(10),
            },
            measurement_certificate(CountCertificateKind::ExactTokenizer),
        );
        let protected_request = request(Some(20));
        let protected = prepared_status(
            &descriptor,
            &protected_request,
            authority(b"account-a"),
            Some(observation(60)),
            timestamp(2),
        );
        let ContextStatusKind::ProtectedOverflow(protected_overflow) = protected.kind() else {
            panic!("expected protected overflow")
        };
        assert_eq!(protected_overflow.input().expect("input").required(), 60);
        assert_eq!(protected_overflow.output().expect("output").required(), 20);
        assert_eq!(
            protected.admit(&protected_request),
            Err(ContextAdmissionError::ProtectedOverflow(
                *protected_overflow
            ))
        );
        let protected_plan = protected
            .planner_disposition()
            .expect("protected planner disposition");
        assert_eq!(
            protected_plan.kind(),
            ContextPlannerDispositionKind::ProtectedOnly
        );
        assert_eq!(protected_plan.selected_group_count(), 0);

        let input = ConversationView::from_shared(
            CanonicalHistory::new(vec![
                HistorySegment::new(
                    HistorySegmentKind::RecentIntent,
                    vec![text_item(10, Actor::User, "older")],
                )
                .expect("older intent"),
                HistorySegment::new(
                    HistorySegmentKind::Conversation,
                    vec![text_item(11, Actor::Assistant, "older answer")],
                )
                .expect("older answer"),
                HistorySegment::new(
                    HistorySegmentKind::RecentIntent,
                    vec![text_item(12, Actor::User, "latest")],
                )
                .expect("latest intent"),
            ])
            .expect("history"),
            WorkingProjection::default(),
        );
        let eligible_request = request_with_input(input, Some(20));
        let eligible = prepared_status(
            &descriptor,
            &eligible_request,
            authority(b"account-a"),
            Some(observation(60)),
            timestamp(2),
        );
        let ContextStatusKind::WouldOverflow(overflow) = eligible.kind() else {
            panic!("expected whole-group overflow")
        };
        assert_eq!(overflow.input().expect("input").required(), 60);
        assert_eq!(overflow.output().expect("output").required(), 20);
        let planner = eligible
            .planner_disposition()
            .expect("eligible planner disposition");
        assert_eq!(
            planner.kind(),
            ContextPlannerDispositionKind::EligibleWholeGroups
        );
        assert_eq!(planner.selected_group_count(), 1);
        assert_eq!(planner.selected_segment_count(), 2);
        assert_eq!(
            eligible.admit(&eligible_request),
            Err(ContextAdmissionError::WouldOverflow(*overflow))
        );
    }

    #[test]
    fn descriptor_revalidation_rejects_binding_drift() {
        let certificate = measurement_certificate(CountCertificateKind::ExactTokenizer);
        let descriptor = descriptor(
            CapacityRule::SharedWindow {
                total: SafeLimit::Exact(100),
            },
            certificate.clone(),
        );
        let request = request(Some(20));
        let (plan, projection) = prepared(
            &descriptor,
            &request,
            authority(b"account-a"),
            Some(observation(60)),
        );
        let admission = descriptor
            .admit_context(&plan, &projection, timestamp(2))
            .expect("admission");
        assert_eq!(admission.status().identity().version(), 3);
        assert_eq!(admission.identity().version(), 4);
        descriptor
            .revalidate_context_admission(&plan, &projection, &admission, timestamp(2))
            .expect("revalidation");
        assert_eq!(
            descriptor
                .revalidate_context_admission(&plan, &projection, &admission, timestamp(100),),
            Err(ContextAdmissionError::Unavailable(
                ContextUnavailable::EvidenceExpired {
                    evidence: "capacity"
                }
            ))
        );

        let foreign = ResolvedModelDescriptor::new(
            &StandardIntegrationAuthority::release_owned(),
            descriptor.protocol().clone(),
            RouteId::new("foreign-route").expect("route"),
            descriptor.requested_model().clone(),
            descriptor.reported_model().cloned(),
            descriptor.model_family().cloned(),
            descriptor.profile().clone(),
            descriptor.capabilities().clone(),
        )
        .with_context_contract(contract(
            CapacityRule::SharedWindow {
                total: SafeLimit::Exact(100),
            },
            certificate,
        ));
        assert_eq!(
            foreign.revalidate_context_admission(&plan, &projection, &admission, timestamp(2),),
            Err(ContextAdmissionError::Unavailable(
                ContextUnavailable::ScopeDrift {
                    field: "descriptor binding"
                }
            ))
        );
    }

    #[test]
    fn contract_deserialization_recomputes_identity() {
        let contract = contract(
            CapacityRule::SharedWindow {
                total: SafeLimit::Exact(100),
            },
            measurement_certificate(CountCertificateKind::ExactTokenizer),
        );
        let encoded = serde_json::to_string(&contract).expect("serialize");
        let decoded: ContextContract = serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(decoded, contract);

        let mut forged = serde_json::to_value(contract).expect("value");
        forged["accounting_revision"] = serde_json::json!("forged-revision");
        assert!(serde_json::from_value::<ContextContract>(forged).is_err());
    }

    #[test]
    fn descriptor_request_validation_still_uses_exact_binding() {
        let descriptor = descriptor(
            CapacityRule::SharedWindow {
                total: SafeLimit::Exact(100),
            },
            measurement_certificate(CountCertificateKind::ExactTokenizer),
        );
        let request = request(Some(20));
        let plan = descriptor
            .validate_request(request.clone(), ValidationMode::Strict)
            .expect("plan");
        descriptor
            .validate_plan_admission(&request, &plan)
            .expect("plan admission");
    }

    proptest! {
        #[test]
        fn shared_window_exact_algebra(
            input in 0_u64..1_000_000,
            reserve in 1_u64..1_000_000,
            spare in 0_u64..1_000_000,
        ) {
            let required = input + reserve;
            let total = required + spare;
            let result = evaluate_rule(
                CapacityRule::SharedWindow { total: SafeLimit::Exact(total.max(1)) },
                CountBound::Exact(input),
                Some(reserve),
            );
            prop_assert_eq!(
                result,
                Ok(ContextHeadroom::Shared(QualifiedTokens::Exact(spare)))
            );
        }

        #[test]
        fn separate_rule_checks_both_axes(
            input in 0_u64..10_000,
            reserve in 1_u64..10_000,
            input_spare in 0_u64..10_000,
            output_spare in 0_u64..10_000,
        ) {
            let result = evaluate_rule(
                CapacityRule::Separate {
                    input: SafeLimit::Exact(input + input_spare),
                    output: SafeLimit::GuaranteedLowerBound(reserve + output_spare),
                },
                CountBound::Exact(input),
                Some(reserve),
            );
            prop_assert_eq!(
                result,
                Ok(ContextHeadroom::Separate {
                    input: QualifiedTokens::Exact(input_spare),
                    output: QualifiedTokens::GuaranteedMinimum(output_spare),
                })
            );
        }

        #[test]
        fn shared_with_output_cap_reports_each_failed_axis(
            input in 1_u64..10_000,
            reserve in 1_u64..10_000,
            shared_shortfall in 1_u64..=100,
            output_shortfall in 1_u64..=100,
        ) {
            let required = input + reserve;
            let total = required.saturating_sub(shared_shortfall).max(1);
            let output = reserve.saturating_sub(output_shortfall).max(1);
            let result = evaluate_rule(
                CapacityRule::SharedWindowWithOutputCap {
                    total: SafeLimit::Exact(total),
                    output: SafeLimit::Exact(output),
                },
                CountBound::Exact(input),
                Some(reserve),
            );
            let Err(EvaluationFailure::Overflow(overflow)) = result else {
                prop_assert!(false, "expected overflow");
                return Ok(());
            };
            prop_assert_eq!(overflow.shared().is_some(), required > total);
            prop_assert_eq!(overflow.output().is_some(), reserve > output);
        }
    }

    #[test]
    fn arithmetic_overflow_is_unavailable_not_wrapped() {
        assert_eq!(
            evaluate_rule(
                CapacityRule::SharedWindow {
                    total: SafeLimit::Exact(u64::MAX)
                },
                CountBound::Exact(u64::MAX),
                Some(1),
            ),
            Err(EvaluationFailure::ArithmeticOverflow)
        );
    }

    #[test]
    fn reference_client_envelope_input_is_closed_and_never_claims_output_or_exact_headroom() {
        let receipt = authority(b"reference-receipt");
        assert_eq!(
            evaluate_rule(
                CapacityRule::ProviderReservedInput {
                    reported_context_window: Some(125),
                    maximum_projected_input: 100,
                    auto_compact_input: None,
                },
                CountBound::ReferenceClientEnvelopeUpperBound {
                    tokens: 60,
                    catalog_receipt: receipt,
                },
                None,
            ),
            Ok(ContextHeadroom::ProviderReservedInput {
                input: QualifiedTokens::GuaranteedMinimum(40),
            })
        );
        assert!(matches!(
            evaluate_rule(
                CapacityRule::ProviderReservedInput {
                    reported_context_window: Some(125),
                    maximum_projected_input: 100,
                    auto_compact_input: None,
                },
                CountBound::ReferenceClientEnvelopeUpperBound {
                    tokens: 101,
                    catalog_receipt: receipt,
                },
                None,
            ),
            Err(EvaluationFailure::Overflow(ContextOverflow {
                input: Some(_),
                output: None,
                ..
            }))
        ));
    }

    #[test]
    fn reference_client_envelope_receipt_drift_cannot_construct_a_contract() {
        let freshness = EvidenceFreshness::new(timestamp(10), Some(timestamp(20)))
            .expect("fresh reference evidence");
        let auth_binding = authority(b"authority");
        let capacity_receipt = authority(b"capacity-receipt");
        let capacity = CapacityCertificate::reference_client_envelope(
            &StandardIntegrationAuthority::release_owned(),
            "reference-capacity",
            "v1",
            CapacityRule::ProviderReservedInput {
                reported_context_window: Some(125),
                maximum_projected_input: 100,
                auto_compact_input: None,
            },
            auth_binding,
            freshness,
            capacity_receipt,
            "reference-v1",
        )
        .expect("reference capacity");
        let measurement = MeasurementCertificate::standard(
            &StandardIntegrationAuthority::release_owned(),
            "reference-measurement",
            "v1",
            "prepared-wire-bytes",
            "v1",
            ContextEvidenceSource::AuthenticatedDiscovery,
            CountCertificateKind::ReferenceClientEnvelopeUpperBound {
                catalog_receipt: authority(b"foreign-receipt"),
            },
            Some(auth_binding),
            freshness,
        )
        .expect("reference measurement");
        assert_eq!(
            ContextContract::new(
                ProtocolId::new("responses").expect("protocol"),
                "accounting-v1",
                "projection-v1",
                "no-output-v1",
                capacity,
                vec![measurement],
            ),
            Err(ContextContractError::EvidenceClassMismatch)
        );
    }

    #[test]
    fn context_semantic_identity_survives_refresh_and_binds_behavior() {
        fn reference_contract(
            freshness: EvidenceFreshness,
            receipt: Digest,
            reference_revision: &str,
            reported_context_window: u64,
        ) -> ContextContract {
            let auth_binding = authority(b"semantic-authority");
            let capacity = CapacityCertificate::reference_client_envelope(
                &StandardIntegrationAuthority::release_owned(),
                "semantic-capacity",
                "capacity-v1",
                CapacityRule::ProviderReservedInput {
                    reported_context_window: Some(reported_context_window),
                    maximum_projected_input: 100,
                    auto_compact_input: None,
                },
                auth_binding,
                freshness,
                receipt,
                reference_revision,
            )
            .expect("reference capacity");
            let measurement = MeasurementCertificate::standard(
                &StandardIntegrationAuthority::release_owned(),
                "semantic-measurement",
                "certificate-v1",
                "complete-input-bound",
                "method-v1",
                ContextEvidenceSource::AuthenticatedDiscovery,
                CountCertificateKind::ReferenceClientEnvelopeUpperBound {
                    catalog_receipt: receipt,
                },
                Some(auth_binding),
                freshness,
            )
            .expect("reference measurement");
            ContextContract::new(
                ProtocolId::new("responses-family").expect("protocol family"),
                "accounting-v1",
                "projection-v1",
                "reservation-v1",
                capacity,
                vec![measurement],
            )
            .expect("context contract")
        }

        let original = reference_contract(
            EvidenceFreshness::new(timestamp(1), Some(timestamp(10))).expect("freshness"),
            authority(b"catalog-receipt-1"),
            "reference-v1",
            125,
        );
        let refreshed = reference_contract(
            EvidenceFreshness::new(timestamp(20), Some(timestamp(30))).expect("freshness"),
            authority(b"catalog-receipt-2"),
            "reference-v1",
            125,
        );
        assert_ne!(original.identity(), refreshed.identity());
        assert_eq!(original.semantic_identity(), refreshed.semantic_identity());

        let changed = reference_contract(
            EvidenceFreshness::new(timestamp(20), Some(timestamp(30))).expect("freshness"),
            authority(b"catalog-receipt-2"),
            "reference-v2",
            125,
        );
        assert_ne!(original.semantic_identity(), changed.semantic_identity());

        let changed_reported_window = reference_contract(
            EvidenceFreshness::new(timestamp(20), Some(timestamp(30))).expect("freshness"),
            authority(b"catalog-receipt-2"),
            "reference-v1",
            126,
        );
        assert_ne!(
            original.semantic_identity(),
            changed_reported_window.semantic_identity()
        );
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one end-to-end certificate test keeps the complete fail-closed matrix visible"
    )]
    fn reference_client_envelope_contract_requires_measurement_and_exposes_non_guarantee() {
        let freshness = EvidenceFreshness::new(timestamp(1), Some(timestamp(100)))
            .expect("fresh reference evidence");
        let auth_binding = authority_class(authority(b"reference-authority"));
        let receipt = authority(b"reference-receipt");
        let capacity = CapacityCertificate::reference_client_envelope(
            &StandardIntegrationAuthority::release_owned(),
            "reference-capacity",
            "v1",
            CapacityRule::ProviderReservedInput {
                reported_context_window: Some(125),
                maximum_projected_input: 100,
                auto_compact_input: None,
            },
            auth_binding,
            freshness,
            receipt,
            "reference-v1",
        )
        .expect("reference capacity");
        assert_eq!(
            capacity.non_guarantee_disposition(),
            Some(ContextNonGuaranteeDisposition::ReferenceClientOperatingEnvelope)
        );
        assert_eq!(
            ContextContract::new(
                ProtocolId::new("responses-family").expect("protocol"),
                "accounting-v1",
                "projection-v1",
                "no-output-v1",
                capacity.clone(),
                Vec::new(),
            ),
            Err(ContextContractError::EvidenceClassMismatch)
        );

        let measurement = MeasurementCertificate::standard(
            &StandardIntegrationAuthority::release_owned(),
            "reference-measurement",
            "v1",
            "complete-prepared-input-bound",
            "v1",
            ContextEvidenceSource::AuthenticatedDiscovery,
            CountCertificateKind::ReferenceClientEnvelopeUpperBound {
                catalog_receipt: receipt,
            },
            Some(auth_binding),
            freshness,
        )
        .expect("reference measurement");
        let reference_contract = ContextContract::new(
            ProtocolId::new("responses-family").expect("protocol"),
            "accounting-v1",
            "projection-v1",
            "no-output-v1",
            capacity,
            vec![measurement],
        )
        .expect("reference contract");
        let reference_descriptor = descriptor(
            CapacityRule::SharedWindow {
                total: SafeLimit::Exact(1),
            },
            measurement_certificate(CountCertificateKind::ExactTokenizer),
        )
        .with_context_contract(reference_contract);
        let request = request(None);
        let status = prepared_status(
            &reference_descriptor,
            &request,
            authority(b"reference-authority"),
            Some(CountObservation::new("reference-measurement", "v1", 60).expect("observation")),
            timestamp(2),
        );
        assert!(matches!(status.kind(), ContextStatusKind::Fits { .. }));
        assert!(matches!(
            status.evidence_class(),
            Some(ContextEvidenceClass::ReferenceClientEnvelope { .. })
        ));
        assert_eq!(
            status.non_guarantee_disposition(),
            Some(ContextNonGuaranteeDisposition::ReferenceClientOperatingEnvelope)
        );
        let admission = status
            .admit_with_requirement(&request, ContextAdmissionRequirement::AnyBounded)
            .expect("bounded admission");
        assert_eq!(
            admission.requirement(),
            ContextAdmissionRequirement::AnyBounded
        );
        assert_eq!(
            admission.non_guarantee_disposition(),
            Some(ContextNonGuaranteeDisposition::ReferenceClientOperatingEnvelope)
        );
        assert_eq!(
            status.admit_with_requirement(
                &request,
                ContextAdmissionRequirement::ReleaseVerifiedRequired,
            ),
            Err(ContextAdmissionError::Unavailable(
                ContextUnavailable::EvidenceRequirementNotMet {
                    required: ContextAdmissionRequirement::ReleaseVerifiedRequired,
                    actual: status.evidence_class().cloned(),
                }
            ))
        );
        let status_wire = serde_json::to_string(&status).expect("status wire");
        assert!(status_wire.contains("ReferenceClientOperatingEnvelope"));
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one end-to-end certificate test keeps the complete fail-closed matrix visible"
    )]
    fn caller_envelope_on_maintained_and_custom_models_is_bounded_and_never_release_verified() {
        let freshness = EvidenceFreshness::new(timestamp(1), None).expect("fresh declaration");
        let auth_binding = authority_class(authority(b"custom-authority"));
        let declaration = authority(b"custom-config-and-provenance");
        let capacity = CapacityCertificate::custom_envelope(
            "custom-capacity",
            "v1",
            CapacityRule::ProviderReservedInput {
                reported_context_window: None,
                maximum_projected_input: 100,
                auto_compact_input: None,
            },
            auth_binding,
            freshness,
            declaration,
            "custom-declaration-v1",
        )
        .expect("custom capacity");
        assert!(
            capacity
                .evidence_class()
                .allows_integration_class(IntegrationClass::Custom)
        );
        assert!(
            capacity
                .evidence_class()
                .allows_integration_class(IntegrationClass::Standard)
        );
        assert_eq!(
            capacity.non_guarantee_disposition(),
            Some(ContextNonGuaranteeDisposition::CustomCallerOperatingEnvelope)
        );
        let measurement = MeasurementCertificate::new(
            "custom-measurement",
            "v1",
            "caller-complete-input-bound",
            "v1",
            ContextEvidenceSource::CallerDeclaration,
            CountCertificateKind::CustomEnvelopeUpperBound { declaration },
            Some(auth_binding),
            freshness,
        )
        .expect("custom measurement");
        let release_capacity = CapacityCertificate::new(
            &StandardIntegrationAuthority::release_owned(),
            "release-capacity",
            "v1",
            ContextEvidenceSource::BuiltInCatalog,
            CapacityRule::SharedWindow {
                total: SafeLimit::Exact(100),
            },
            None,
            freshness,
        )
        .expect("release capacity");
        assert_eq!(
            ContextContract::new(
                ProtocolId::new("responses-family").expect("protocol"),
                "accounting-v1",
                "projection-v1",
                "reservation-v1",
                release_capacity,
                vec![measurement.clone()],
            ),
            Err(ContextContractError::EvidenceClassMismatch)
        );
        let contract = ContextContract::new(
            ProtocolId::new("responses-family").expect("protocol"),
            "accounting-v1",
            "projection-v1",
            "no-output-v1",
            capacity,
            vec![measurement],
        )
        .expect("custom contract");
        let standard = descriptor(
            CapacityRule::SharedWindow {
                total: SafeLimit::Exact(1),
            },
            measurement_certificate(CountCertificateKind::ExactTokenizer),
        );
        let custom = ResolvedModelDescriptor::new_custom(
            standard.protocol().clone(),
            standard.route().clone(),
            standard.requested_model().clone(),
            standard.reported_model().cloned(),
            standard.model_family().cloned(),
            standard.profile().clone(),
            standard.capabilities().clone(),
        )
        .with_context_contract(contract.clone());
        let maintained = standard.with_context_contract(contract);
        let request = request(None);
        for descriptor in [custom, maintained] {
            let wire = serde_json::to_vec(&descriptor).expect("descriptor wire");
            let descriptor: ResolvedModelDescriptor =
                serde_json::from_slice(&wire).expect("compatible descriptor");
            let status = prepared_status(
                &descriptor,
                &request,
                authority(b"custom-authority"),
                Some(CountObservation::new("custom-measurement", "v1", 60).expect("observation")),
                timestamp(2),
            );
            assert!(matches!(status.kind(), ContextStatusKind::Fits { .. }));
            assert!(matches!(
                status.evidence_class(),
                Some(ContextEvidenceClass::CustomEnvelope { .. })
            ));
            let admission = status
                .admit_with_requirement(&request, ContextAdmissionRequirement::AnyBounded)
                .expect("bounded admission");
            assert_eq!(
                admission.non_guarantee_disposition(),
                Some(ContextNonGuaranteeDisposition::CustomCallerOperatingEnvelope)
            );
            assert!(matches!(
                status.admit_with_requirement(
                    &request,
                    ContextAdmissionRequirement::ReleaseVerifiedRequired
                ),
                Err(ContextAdmissionError::Unavailable(
                    ContextUnavailable::EvidenceRequirementNotMet { .. }
                ))
            ));
            let overflow = prepared_status(
                &descriptor,
                &request,
                authority(b"custom-authority"),
                Some(
                    CountObservation::new("custom-measurement", "v1", 101)
                        .expect("overflow observation"),
                ),
                timestamp(2),
            );
            assert!(
                overflow
                    .admit_with_requirement(&request, ContextAdmissionRequirement::AnyBounded)
                    .is_err()
            );
        }
    }

    #[test]
    fn admission_requirement_is_bound_into_the_receipt_identity() {
        let descriptor = descriptor(
            CapacityRule::SharedWindow {
                total: SafeLimit::Exact(100),
            },
            measurement_certificate(CountCertificateKind::ExactTokenizer),
        );
        let request = request(Some(20));
        let status = prepared_status(
            &descriptor,
            &request,
            authority(b"account-a"),
            Some(observation(60)),
            timestamp(2),
        );
        let any = status
            .admit_with_requirement(&request, ContextAdmissionRequirement::AnyBounded)
            .expect("bounded admission");
        let certified = status
            .admit_with_requirement(
                &request,
                ContextAdmissionRequirement::ReleaseVerifiedRequired,
            )
            .expect("certified admission");
        assert_ne!(any.identity(), certified.identity());
        assert_eq!(
            certified.requirement(),
            ContextAdmissionRequirement::ReleaseVerifiedRequired
        );
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one adversarial matrix proves every recomputed evidence-class forgery fails"
    )]
    fn forged_recomputed_wire_cannot_bypass_evidence_class_invariants() {
        let freshness =
            EvidenceFreshness::new(timestamp(1), Some(timestamp(100))).expect("freshness");
        let authority_binding = authority(b"authority");
        let receipt = authority(b"receipt");
        let valid_capacity = CapacityCertificate::reference_client_envelope(
            &StandardIntegrationAuthority::release_owned(),
            "reference-capacity",
            "v1",
            CapacityRule::ProviderReservedInput {
                reported_context_window: Some(125),
                maximum_projected_input: 100,
                auto_compact_input: None,
            },
            authority_binding,
            freshness,
            receipt,
            "reference-v1",
        )
        .expect("capacity");

        let mut forged_capacity = serde_json::to_value(&valid_capacity).expect("capacity wire");
        forged_capacity["non_guarantee_disposition"] = serde_json::Value::Null;
        let forged_capacity_content = CapacityCertificateContent {
            evidence_id: "reference-capacity",
            revision: "v1",
            source: ContextEvidenceSource::AuthenticatedDiscovery,
            rule: CapacityRule::ProviderReservedInput {
                reported_context_window: Some(125),
                maximum_projected_input: 100,
                auto_compact_input: None,
            },
            authority_binding: Some(authority_binding),
            freshness,
            evidence_class: valid_capacity.evidence_class(),
            non_guarantee_disposition: None,
        };
        forged_capacity["identity"] = serde_json::to_value(
            semantic_digest(DigestDomain::ContextBudget, 3, &forged_capacity_content)
                .expect("forged identity"),
        )
        .expect("identity wire");
        assert!(serde_json::from_value::<CapacityCertificate>(forged_capacity).is_err());

        let valid_measurement = MeasurementCertificate::standard(
            &StandardIntegrationAuthority::release_owned(),
            "reference-measurement",
            "v1",
            "complete-input-bound",
            "v1",
            ContextEvidenceSource::AuthenticatedDiscovery,
            CountCertificateKind::ReferenceClientEnvelopeUpperBound {
                catalog_receipt: receipt,
            },
            Some(authority_binding),
            freshness,
        )
        .expect("measurement");
        let mut forged_measurement =
            serde_json::to_value(&valid_measurement).expect("measurement wire");
        forged_measurement["source"] =
            serde_json::to_value(ContextEvidenceSource::BuiltInCatalog).expect("source wire");
        forged_measurement["authority_binding"] = serde_json::Value::Null;
        let forged_measurement_content = MeasurementCertificateContent {
            certificate_id: "reference-measurement",
            revision: "v1",
            method: "complete-input-bound",
            method_revision: "v1",
            source: ContextEvidenceSource::BuiltInCatalog,
            kind: valid_measurement.kind(),
            authority_binding: None,
            freshness,
        };
        forged_measurement["identity"] = serde_json::to_value(
            semantic_digest(
                DigestDomain::TokenMeasurement,
                3,
                &forged_measurement_content,
            )
            .expect("forged identity"),
        )
        .expect("identity wire");
        assert!(serde_json::from_value::<MeasurementCertificate>(forged_measurement).is_err());

        let valid_contract = ContextContract::new(
            ProtocolId::new("responses-family").expect("protocol"),
            "accounting-v1",
            "projection-v1",
            "no-output-v1",
            valid_capacity,
            vec![valid_measurement],
        )
        .expect("contract");
        let mut forged_contract = serde_json::to_value(&valid_contract).expect("contract wire");
        forged_contract["measurements"] = serde_json::json!([]);
        let empty_measurements: Vec<MeasurementCertificate> = Vec::new();
        let forged_contract_content = ContextContractContent {
            protocol_family: valid_contract.protocol_family(),
            accounting_revision: valid_contract.accounting_revision(),
            projection_revision: valid_contract.projection_revision(),
            reservation_revision: valid_contract.reservation_revision(),
            capacity: valid_contract.capacity(),
            measurements: &empty_measurements,
        };
        forged_contract["identity"] = serde_json::to_value(
            semantic_digest(DigestDomain::ContextBudget, 4, &forged_contract_content)
                .expect("forged identity"),
        )
        .expect("identity wire");
        assert!(serde_json::from_value::<ContextContract>(forged_contract).is_err());
    }

    #[test]
    fn standalone_freshness_and_evidence_class_deserialization_fail_closed() {
        let invalid_freshness = serde_json::json!({
            "observed_at": timestamp(10),
            "expires_at": timestamp(10),
        });
        assert!(serde_json::from_value::<EvidenceFreshness>(invalid_freshness).is_err());
        assert!(serde_json::from_str::<ContextEvidenceClass>(
            r#"{"ReferenceClientEnvelope":{"catalog_receipt":{"algorithm":"Sha256","domain":"ContextBudget","version":1,"bytes":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]},"reference_revision":""}}"#,
        )
        .is_err());
        assert!(serde_json::from_str::<ContextEvidenceClass>(r#""Certified""#).is_err());
        assert!(serde_json::from_str::<ContextEvidenceClass>(
            r#"{"ReferenceBacked":{"catalog_receipt":{"algorithm":"Sha256","domain":"ContextBudget","version":1,"bytes":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]},"reference_revision":"v1"}}"#,
        )
        .is_err());

        let capacity = CapacityCertificate::new(
            &StandardIntegrationAuthority::release_owned(),
            "release-capacity",
            "v1",
            ContextEvidenceSource::BuiltInCatalog,
            CapacityRule::SharedWindow {
                total: SafeLimit::Exact(100),
            },
            None,
            EvidenceFreshness::new(timestamp(1), None).expect("freshness"),
        )
        .expect("capacity");
        let mut mixed_wire = serde_json::to_value(capacity).expect("capacity wire");
        mixed_wire["assurance"] = serde_json::json!("Certified");
        assert!(serde_json::from_value::<CapacityCertificate>(mixed_wire).is_err());
    }
}
