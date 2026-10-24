//! Resolved native models and providers, and resolution errors.

use super::{
    Arc, CompiledProviderConfiguration, ConfiguredLanguageModel, Digest, DigestDomain, DigestError,
    Error, LanguageModel, ModelId, NativeSubscriptionError, NativeToolMode,
    NativeToolModeRequirementError, ResolvedModelDescriptor, RuntimeAdmissionWitness,
    RuntimeModelAuthority, Timestamp, fmt, require_exact_native_tool_mode, reresolve,
};

/// One gated native model resolution with fresh account-scoped evidence.
///
/// Credential material stays inside the returned [`LanguageModel`].
pub struct ResolvedNativeModel {
    pub(crate) selected_model: ModelId,
    pub(crate) descriptor: ResolvedModelDescriptor,
    pub(crate) model: Arc<dyn LanguageModel>,
    pub(crate) evidence: SubscriptionResolutionEvidence,
    /// The route's hosted-retrieval port on the same model and grant.
    pub(crate) retrieval: Option<Arc<dyn plakous_tools::RetrievalPort>>,
    /// The route's image generation port on the same model and grant.
    pub(crate) image_gen: Option<Arc<dyn plakous_tool_web::ImageGenPort>>,
}

/// One exact native provider resolution carrying both the executable model and
/// the complete selected-to-effective configuration authority.
pub struct ResolvedProvider {
    pub(super) model: ResolvedNativeModel,
    pub(super) configured: ConfiguredLanguageModel,
    pub(super) native_tool_mode: Option<NativeToolMode>,
    pub(super) reresolution: Option<Arc<reresolve::Reresolution>>,
}

impl ResolvedProvider {
    /// The route's hosted-retrieval port bound to the same model, account
    /// and credential, when the route has one.
    #[must_use]
    pub fn retrieval_port(&self) -> Option<Arc<dyn plakous_tools::RetrievalPort>> {
        self.model.retrieval.clone()
    }

    /// The route's image generation port bound to the same model, account
    /// and credential, when the route has one.
    #[must_use]
    pub fn image_gen_port(&self) -> Option<Arc<dyn plakous_tool_web::ImageGenPort>> {
        self.model.image_gen.clone()
    }

    /// Returns the exact resolved model handle.
    #[must_use]
    pub fn model(&self) -> Arc<dyn LanguageModel> {
        self.configured.model()
    }

    /// Returns the complete provider configuration authority.
    #[must_use]
    pub const fn configuration(&self) -> &CompiledProviderConfiguration {
        self.configured.configuration()
    }

    /// Returns the exact catalog-selected model.
    #[must_use]
    pub const fn selected_model(&self) -> &ModelId {
        self.model.selected_model()
    }

    /// Returns the fresh execution authority obtained during this resolution.
    #[must_use]
    pub const fn initial_authority(&self) -> &RuntimeModelAuthority {
        self.configured.initial_authority()
    }

    /// Returns a one-way binding to the exact selected login epoch. A fresh
    /// credential generation preserves this value; relogin does not.
    ///
    /// # Errors
    ///
    /// Returns the digest error if the binding input cannot be hashed.
    pub fn credential_selection_binding(&self) -> Result<Digest, DigestError> {
        Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[
                b"plakous/native-credential-selection-v1",
                self.model.evidence.credential_selection_epoch.as_bytes(),
            ],
        )
    }

    /// Returns the sealed model/configuration aggregate for agent construction.
    #[must_use]
    pub fn configured_model(&self) -> ConfiguredLanguageModel {
        self.configured.clone()
    }

    /// Returns the exact native tool mode admitted by fresh resolution evidence.
    ///
    /// Non-Codex providers and missing or unknown catalog values return `None`.
    /// No model-name, shell, route, or neighboring-catalog inference is used.
    #[must_use]
    pub const fn native_tool_mode(&self) -> Option<NativeToolMode> {
        self.native_tool_mode
    }

    /// Requires one exact native tool mode before product dispatch is constructed.
    ///
    /// # Errors
    ///
    /// Returns [`NativeToolModeRequirementError`] when evidence admitted a
    /// different mode or did not admit a recognized mode.
    pub const fn require_native_tool_mode(
        &self,
        required: NativeToolMode,
    ) -> Result<(), NativeToolModeRequirementError> {
        require_exact_native_tool_mode(self.native_tool_mode, required)
    }
}

impl ResolvedNativeModel {
    /// Returns the exact catalog model that was selected.
    #[must_use]
    pub const fn selected_model(&self) -> &ModelId {
        &self.selected_model
    }

    /// Returns the resolved model handle admitted for inference.
    #[must_use]
    pub fn model(&self) -> Arc<dyn LanguageModel> {
        Arc::clone(&self.model)
    }

    /// Returns the exact immutable descriptor retained by resolution evidence.
    #[must_use]
    pub const fn descriptor(&self) -> &ResolvedModelDescriptor {
        &self.descriptor
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SubscriptionResolutionEvidence {
    pub(crate) account_fingerprint: String,
    pub(crate) credential_authority_binding: String,
    pub(crate) credential_generation: u64,
    pub(crate) credential_selection_epoch: String,
    pub(crate) route_fingerprint: String,
    pub(crate) descriptor_digest: String,
    pub(crate) dialect: String,
    pub(crate) adapter_revision: String,
    pub(crate) catalog_observed_at: Timestamp,
    pub(crate) catalog_expires_at: Timestamp,
    pub(crate) runtime_admission_witness: Option<RuntimeAdmissionWitness>,
    pub(crate) profile_revision: String,
    pub(crate) codex_exact: Option<plakous_openai::CodexModelResolutionEvidence>,
    pub(crate) xai_exact: Option<plakous_xai::XaiModelResolutionEvidence>,
}

/// Credential-free failure from gated native model resolution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeModelResolutionError {
    /// Set when failure happened after a concrete model id was chosen.
    pub selected_model: Option<ModelId>,
    /// Underlying credential-free subscription failure (transport, model, etc.).
    pub kind: NativeSubscriptionError,
}

impl fmt::Display for NativeModelResolutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.kind.fmt(f)
    }
}

impl Error for NativeModelResolutionError {}

impl NativeModelResolutionError {
    pub(super) fn before_selection(error: NativeSubscriptionError) -> Self {
        Self {
            selected_model: None,
            kind: error,
        }
    }

    pub(super) fn after_selection(selected_model: ModelId, error: NativeSubscriptionError) -> Self {
        Self {
            selected_model: Some(selected_model),
            kind: error,
        }
    }

    /// Returns the underlying subscription failure kind.
    #[must_use]
    pub const fn kind(&self) -> NativeSubscriptionError {
        self.kind
    }
}
