//! Credential-free evidence for one native model resolution.

use super::{
    Digest, ProviderConfigurationError, ProviderSemanticEvidence, ResolvedModelDescriptor,
    RuntimeAdmissionWitness, Serialize, Timestamp, XAI_GROK_COMPATIBILITY_PROFILE,
    XaiCatalogDiagnosticFacts, XaiDialect, fmt, model_catalog_source_revision,
};

/// Credential-free evidence for one exact native xAI model resolution.
///
/// Every field is copied from the same authenticated catalog receipt,
/// credential fence, dynamic route, and descriptor that produced the returned
/// [`LanguageModel`](super::LanguageModel). Raw account identifiers, bearer
/// credentials, and local store paths are never retained by this value.
#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct XaiModelResolutionEvidence {
    pub(super) evidence_revision: u32,
    pub(super) account_fingerprint: Digest,
    pub(super) credential_authority_id: String,
    pub(super) credential_authority_binding: Digest,
    pub(super) selected_credential_generation: u64,
    pub(super) credential_selection_epoch: String,
    pub(super) route_fingerprint: String,
    pub(super) descriptor: ResolvedModelDescriptor,
    pub(super) descriptor_digest: Digest,
    pub(super) dialect: XaiDialect,
    pub(super) adapter_revision: String,
    pub(super) catalog_observed_at: Timestamp,
    pub(super) catalog_expires_at: Timestamp,
    pub(super) catalog_diagnostics: XaiCatalogDiagnosticFacts,
    pub(super) runtime_admission_witness: RuntimeAdmissionWitness,
    pub(super) profile_revision: String,
}

impl XaiModelResolutionEvidence {
    /// Returns the serialized exact-resolution evidence revision.
    #[must_use]
    pub const fn evidence_revision(&self) -> u32 {
        self.evidence_revision
    }

    /// Returns the exact opaque billed-account fingerprint bound into the route.
    #[must_use]
    pub const fn account_fingerprint(&self) -> Digest {
        self.account_fingerprint
    }

    /// Returns the stable non-secret ID of the selected credential authority.
    #[must_use]
    pub fn credential_authority_id(&self) -> &str {
        &self.credential_authority_id
    }

    /// Returns the opaque binding of the selected credential authority.
    #[must_use]
    pub const fn credential_authority_binding(&self) -> Digest {
        self.credential_authority_binding
    }

    /// Returns the exact stored credential generation selected for resolution.
    #[must_use]
    pub const fn selected_credential_generation(&self) -> u64 {
        self.selected_credential_generation
    }

    /// Returns the exact random selection epoch stored with the selected login.
    #[must_use]
    pub fn credential_selection_epoch(&self) -> &str {
        &self.credential_selection_epoch
    }

    /// Returns the exact dynamic route fingerprint used by the returned model.
    #[must_use]
    pub fn route_fingerprint(&self) -> &str {
        &self.route_fingerprint
    }

    /// Returns the exact neutral descriptor binding digest of the returned model.
    #[must_use]
    pub const fn descriptor_digest(&self) -> Digest {
        self.descriptor_digest
    }

    /// Returns the exact immutable descriptor resolved from the authenticated catalog.
    #[must_use]
    pub const fn descriptor(&self) -> &ResolvedModelDescriptor {
        &self.descriptor
    }

    /// Returns the descriptor after rechecking its retained binding digest.
    ///
    /// # Errors
    ///
    /// Returns an identity error when the descriptor cannot be digested or no
    /// longer matches the digest captured atomically during resolution.
    pub fn validated_descriptor(
        &self,
    ) -> Result<&ResolvedModelDescriptor, ProviderConfigurationError> {
        if self.evidence_revision != 2
            || self
                .descriptor
                .binding_digest()
                .map_err(|_| ProviderConfigurationError::Identity)?
                != self.descriptor_digest
        {
            return Err(ProviderConfigurationError::Identity);
        }
        Ok(&self.descriptor)
    }

    /// Returns the exact xAI protocol dialect selected for the returned model.
    #[must_use]
    pub const fn dialect(&self) -> XaiDialect {
        self.dialect
    }

    /// Returns the exact xAI protocol-adapter revision.
    #[must_use]
    pub fn adapter_revision(&self) -> &str {
        &self.adapter_revision
    }

    /// Returns when the authenticated account-scoped catalog was observed.
    #[must_use]
    pub const fn catalog_observed_at(&self) -> Timestamp {
        self.catalog_observed_at
    }

    /// Returns the exclusive expiry of the authenticated catalog receipt.
    #[must_use]
    pub const fn catalog_expires_at(&self) -> Timestamp {
        self.catalog_expires_at
    }

    /// Returns authenticated but non-authoritative catalog diagnostics.
    #[must_use]
    pub const fn catalog_diagnostics(&self) -> XaiCatalogDiagnosticFacts {
        self.catalog_diagnostics
    }

    /// Returns the exact bounded selected-row/catalog-receipt witness produced
    /// by the same live route that produced the model.
    #[must_use]
    pub const fn runtime_admission_witness(&self) -> &RuntimeAdmissionWitness {
        &self.runtime_admission_witness
    }

    /// Returns the exact descriptor compatibility-profile identity.
    #[must_use]
    pub const fn profile_id(&self) -> &'static str {
        XAI_GROK_COMPATIBILITY_PROFILE
    }

    /// Returns the exact immutable native compatibility-profile revision.
    #[must_use]
    pub fn profile_revision(&self) -> &str {
        &self.profile_revision
    }

    /// Returns the provider-owned semantic evidence projection used by
    /// effective configuration identity.
    ///
    /// Token generation, selection epoch, dynamic receipt identity, and
    /// freshness stay in per-attempt runtime authority rather than this stable
    /// projection.
    pub fn semantic_provider_evidence(
        &self,
    ) -> Result<ProviderSemanticEvidence, ProviderConfigurationError> {
        #[derive(Serialize)]
        struct SemanticSuperGrokEvidence<'a> {
            schema: &'a str,
            evidence_revision: u32,
            adapter_revision: &'a str,
            profile_revision: &'a str,
            feature_revision: &'a str,
            dialect_revision: u32,
            source_revision: &'a str,
            reported_context_length: u32,
        }
        let bytes = serde_json::to_vec(&SemanticSuperGrokEvidence {
            schema: "plakous-xai/supergrok-semantic-evidence-v2",
            evidence_revision: self.evidence_revision,
            adapter_revision: &self.adapter_revision,
            profile_revision: &self.profile_revision,
            feature_revision: crate::responses_dialect::SUPERGROK_FEATURE_REVISION,
            dialect_revision: crate::responses_dialect::SUPERGROK_RESPONSES_DIALECT_REVISION,
            source_revision: model_catalog_source_revision(
                self.descriptor.requested_model().as_str(),
            ),
            reported_context_length: self.catalog_diagnostics.reported_context_length().get(),
        })
        .map_err(|_| ProviderConfigurationError::Identity)?;
        ProviderSemanticEvidence::new(2, bytes)
    }
}

impl fmt::Debug for XaiModelResolutionEvidence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("XaiModelResolutionEvidence")
            .field("evidence_revision", &self.evidence_revision)
            .field("account_fingerprint", &self.account_fingerprint)
            .field("credential_authority_id", &self.credential_authority_id)
            .field(
                "credential_authority_binding",
                &self.credential_authority_binding,
            )
            .field(
                "selected_credential_generation",
                &self.selected_credential_generation,
            )
            .field(
                "credential_selection_epoch",
                &self.credential_selection_epoch,
            )
            .field("route_fingerprint", &self.route_fingerprint)
            .field("descriptor", &self.descriptor)
            .field("descriptor_digest", &self.descriptor_digest)
            .field("dialect", &self.dialect)
            .field("adapter_revision", &self.adapter_revision)
            .field("catalog_observed_at", &self.catalog_observed_at)
            .field("catalog_expires_at", &self.catalog_expires_at)
            .field("catalog_diagnostics", &self.catalog_diagnostics)
            .field("runtime_admission_witness", &"<redacted>")
            .field("profile_revision", &self.profile_revision)
            .finish()
    }
}
