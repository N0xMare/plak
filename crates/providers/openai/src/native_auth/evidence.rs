//! Credential-free evidence for one native Codex model resolution.

use super::{
    CodexCatalogCapacityFacts, CodexCatalogToolMode, CodexReasoningEffortSet, Digest,
    ProviderConfigurationError, ProviderSemanticEvidence, ReasoningEffort, ResolvedModelDescriptor,
    Serialize, Timestamp,
};

/// Credential-free exact evidence for one native Codex model resolution.
///
/// Identifying account and storage-path inputs are represented only by
/// domain-separated digests. The value contains no credential material and
/// serializes no raw account label, origin, or resource path.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CodexModelResolutionEvidence {
    pub(super) evidence_revision: u32,
    pub(super) account_fingerprint: Digest,
    pub(super) credential_authority_binding: Digest,
    pub(super) selected_credential_generation: u64,
    pub(super) credential_selection_epoch: Digest,
    pub(super) route_fingerprint: Digest,
    pub(super) descriptor: ResolvedModelDescriptor,
    pub(super) descriptor_digest: Digest,
    pub(super) catalog_receipt: Digest,
    pub(super) capacity_facts: CodexCatalogCapacityFacts,
    #[serde(rename = "supported_reasoning_levels")]
    pub(super) reasoning_efforts: CodexReasoningEffortSet,
    pub(super) tool_mode: Option<CodexCatalogToolMode>,
    pub(super) responses_lite: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) content_filter_guidance: Option<String>,
    pub(super) catalog_enum_fingerprint: Digest,
    pub(super) adapter_revision: &'static str,
    pub(super) catalog_observed_at: Timestamp,
    pub(super) catalog_expires_at: Timestamp,
    pub(super) profile_revision: &'static str,
}

impl CodexModelResolutionEvidence {
    /// Returns the bounded, nonblank model-message override, when advertised.
    #[must_use]
    pub fn content_filter_guidance(&self) -> Option<&str> {
        self.content_filter_guidance.as_deref()
    }

    /// Returns the serialized exact-resolution evidence revision.
    #[must_use]
    pub const fn evidence_revision(&self) -> u32 {
        self.evidence_revision
    }

    /// Returns the route-scoped fingerprint of the exact selected account.
    #[must_use]
    pub const fn account_fingerprint(&self) -> Digest {
        self.account_fingerprint
    }

    /// Returns the exact explicit credential-store authority binding.
    #[must_use]
    pub const fn credential_authority_binding(&self) -> Digest {
        self.credential_authority_binding
    }

    /// Returns the selected credential generation authenticated by discovery.
    #[must_use]
    pub const fn selected_credential_generation(&self) -> u64 {
        self.selected_credential_generation
    }

    /// Returns a deterministic epoch for this authority, account, and generation.
    #[must_use]
    pub const fn credential_selection_epoch(&self) -> Digest {
        self.credential_selection_epoch
    }

    /// Returns the exact route/account/model/profile/capacity binding.
    #[must_use]
    pub const fn route_fingerprint(&self) -> Digest {
        self.route_fingerprint
    }

    /// Returns the immutable neutral resolved-descriptor digest.
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
        if self.evidence_revision != 5
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

    /// Returns the exact authenticated catalog/reference receipt enrolled by
    /// the descriptor's reference-client-envelope input envelope.
    #[must_use]
    pub const fn catalog_receipt(&self) -> Digest {
        self.catalog_receipt
    }

    /// Returns the exact authenticated catalog capacity facts in this receipt.
    ///
    /// The receipt binds these facts to the selected account, credential
    /// authority, model, route, observation interval, and adapter revision.
    #[must_use]
    pub const fn capacity_facts(&self) -> CodexCatalogCapacityFacts {
        self.capacity_facts
    }

    /// Reports whether the resolved catalog row advertised the named effort.
    ///
    /// The result is reconstructed from credential-free resolution evidence;
    /// token budgets and unknown future effort labels fail closed.
    #[must_use]
    pub const fn supports_reasoning_effort(&self, effort: ReasoningEffort) -> bool {
        self.reasoning_efforts.supports(effort)
    }

    /// Returns the exact recognized authenticated catalog `tool_mode`.
    ///
    /// Omission resolves to Direct under the pinned 0.166.0 contract. `None`
    /// is deliberately non-qualifying and covers unknown future values.
    #[must_use]
    pub const fn tool_mode(&self) -> Option<CodexCatalogToolMode> {
        self.tool_mode
    }

    /// Returns the authenticated Codex Responses Lite request disposition.
    #[must_use]
    pub const fn uses_responses_lite(&self) -> bool {
        self.responses_lite
    }

    /// Reports whether this exact resolved row advertised Code Mode.
    #[must_use]
    pub const fn supports_code_mode(&self) -> bool {
        matches!(
            self.tool_mode,
            Some(CodexCatalogToolMode::CodeMode | CodexCatalogToolMode::CodeModeOnly)
        )
    }

    /// Returns the provider-owned semantic evidence projection used by
    /// effective configuration identity.
    ///
    /// Credential generation, selection epoch, receipt nonce, observation,
    /// and expiry remain fresh execution authority and are intentionally not
    /// present in this projection.
    pub fn semantic_provider_evidence(
        &self,
    ) -> Result<ProviderSemanticEvidence, ProviderConfigurationError> {
        #[derive(Serialize)]
        struct SemanticCodexEvidence<'a> {
            schema: &'a str,
            evidence_revision: u32,
            adapter_revision: &'a str,
            profile_revision: &'a str,
            reference_client_version: &'a str,
            catalog_source_revision: &'a str,
            capacity_facts: CodexCatalogCapacityFacts,
            reasoning_efforts: u8,
            tool_mode: Option<CodexCatalogToolMode>,
            responses_lite: bool,
            catalog_enum_fingerprint: Digest,
        }
        plakous_auth::native_auth::semantic_provider_evidence(
            6,
            &SemanticCodexEvidence {
                schema: "plakous-openai/codex-semantic-evidence-v6",
                evidence_revision: self.evidence_revision,
                adapter_revision: self.adapter_revision,
                profile_revision: self.profile_revision,
                reference_client_version: crate::OPENAI_CODEX_REFERENCE_CLIENT_VERSION,
                catalog_source_revision: crate::config::OPENAI_CODEX_CATALOG_SOURCE_REVISION,
                capacity_facts: self.capacity_facts,
                reasoning_efforts: self.reasoning_efforts.identity_byte(),
                tool_mode: self.tool_mode,
                responses_lite: self.responses_lite,
                catalog_enum_fingerprint: self.catalog_enum_fingerprint,
            },
        )
    }

    /// Returns the protocol adapter revision that will prepare requests.
    #[must_use]
    pub const fn adapter_revision(&self) -> &'static str {
        self.adapter_revision
    }

    /// Returns when the exact authenticated catalog was observed.
    #[must_use]
    pub const fn catalog_observed_at(&self) -> Timestamp {
        self.catalog_observed_at
    }

    /// Returns the hard expiry of the exact authenticated catalog evidence.
    #[must_use]
    pub const fn catalog_expires_at(&self) -> Timestamp {
        self.catalog_expires_at
    }

    /// Returns the exact revisioned native compatibility-profile identity.
    #[must_use]
    pub const fn profile_revision(&self) -> &'static str {
        self.profile_revision
    }

    /// Returns the exact descriptor profile identity.
    #[must_use]
    pub const fn profile_id(&self) -> &'static str {
        self.profile_revision
    }
}
