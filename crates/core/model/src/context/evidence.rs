//! Capacity and measurement evidence and the context contract built from it.

use std::collections::BTreeSet;

use serde::{Deserialize, Deserializer, Serialize};

use crate::{Digest, DigestDomain, ProtocolId, Timestamp};

use super::{
    ContextContractError, ContextUnavailable, MAX_MEASUREMENT_CERTIFICATES, semantic_digest,
    validate_evidence_authority, validate_label,
};

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

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "unit tests use expect for deterministic fixture clarity"
)]
mod tests;
