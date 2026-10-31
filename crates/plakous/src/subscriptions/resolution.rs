//! Resolved native models and providers, and resolution errors.

use super::{
    Arc, CompiledProviderConfiguration, ConfiguredLanguageModel, Digest, DigestDomain, DigestError,
    LanguageModel, ModelId, NativeToolMode, NativeToolModeRequirementError,
    ResolvedModelDescriptor, RuntimeAdmissionWitness, RuntimeModelAuthority, Timestamp,
    require_exact_native_tool_mode, reresolve,
};

/// One gated native model resolution with fresh account-scoped evidence.
///
/// Credential material stays inside the returned [`LanguageModel`].
pub struct ResolvedNativeModel {
    pub(super) service_tiers: Vec<String>,
    pub(super) selected_model: ModelId,
    pub(super) descriptor: ResolvedModelDescriptor,
    pub(super) model: Arc<dyn LanguageModel>,
    pub(super) evidence: SubscriptionResolutionEvidence,
    /// The route's hosted-retrieval port on the same model and grant.
    pub(super) retrieval: Option<Arc<dyn plakous_tools::RetrievalPort>>,
    /// The route's image generation port on the same model and grant.
    pub(super) image_gen: Option<Arc<dyn plakous_tool_web::ImageGenPort>>,
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

    /// Returns the route adapter revision this build resolved: a build
    /// constant that feeds the model binding digest, never credential bytes.
    #[must_use]
    pub fn adapter_revision(&self) -> &str {
        &self.model.evidence.adapter_revision
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
pub(super) struct SubscriptionResolutionEvidence {
    pub(super) account_fingerprint: String,
    pub(super) credential_authority_binding: String,
    pub(super) credential_generation: u64,
    pub(super) credential_selection_epoch: String,
    pub(super) route_fingerprint: String,
    pub(super) descriptor_digest: String,
    pub(super) dialect: String,
    pub(super) adapter_revision: String,
    pub(super) catalog_observed_at: Timestamp,
    pub(super) catalog_expires_at: Timestamp,
    pub(super) runtime_admission_witness: Option<RuntimeAdmissionWitness>,
    pub(super) profile_revision: String,
    pub(super) exact: Option<Arc<dyn ExactNativeEvidence>>,
    pub(super) exact_provider: Option<super::NativeProvider>,
}

pub(super) trait ExactNativeEvidence: std::any::Any + std::fmt::Debug + Send + Sync {
    fn as_any(&self) -> &dyn std::any::Any;
    fn equivalent(&self, other: &dyn ExactNativeEvidence) -> bool;
}

impl<T: std::any::Any + std::fmt::Debug + Eq + Send + Sync> ExactNativeEvidence for T {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn equivalent(&self, other: &dyn ExactNativeEvidence) -> bool {
        other.as_any().downcast_ref::<Self>() == Some(self)
    }
}

impl PartialEq for dyn ExactNativeEvidence {
    fn eq(&self, other: &Self) -> bool {
        self.equivalent(other)
    }
}

impl Eq for dyn ExactNativeEvidence {}
