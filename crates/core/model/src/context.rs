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
mod occupancy_anchor_tests;

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::cast_possible_truncation,
    reason = "unit tests use expect for deterministic fixture clarity and measure short literal bodies"
)]
mod input_parts_tests;

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "unit tests use expect for deterministic fixture clarity"
)]
mod tests;
