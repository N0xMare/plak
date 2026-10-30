//! Uninhabited native resolutions in a build without native execution.
use super::{
    Arc, ModelId, NativeModelResolutionError, NativeToolMode, NativeToolModeRequirementError,
};
use plakous_model::{
    CompiledProviderConfiguration, ConfiguredLanguageModel, Digest, DigestError, LanguageModel,
    ResolvedModelDescriptor, RuntimeModelAuthority, Timestamp,
};
/// No native resolution can be created in this build.
pub struct ResolvedNativeModel(std::convert::Infallible);
impl ResolvedNativeModel {
    /// Returns the exact catalog-selected model.
    #[must_use]
    pub fn selected_model(&self) -> &ModelId {
        match self.0 {}
    }
    /// Returns the exact resolved model handle.
    #[must_use]
    pub fn model(&self) -> Arc<dyn LanguageModel> {
        match self.0 {}
    }
    /// Returns the exact immutable descriptor retained by resolution evidence.
    #[must_use]
    pub fn descriptor(&self) -> &ResolvedModelDescriptor {
        match self.0 {}
    }
}
/// No native provider can be created in this build.
pub struct ResolvedProvider(std::convert::Infallible);
impl ResolvedProvider {
    /// The route's hosted-retrieval port bound to the same model, account
    /// and credential, when the route has one.
    #[must_use]
    pub fn retrieval_port(&self) -> Option<Arc<dyn plakous_tools::RetrievalPort>> {
        match self.0 {}
    }
    /// The route's image generation port bound to the same model, account
    /// and credential, when the route has one.
    #[must_use]
    pub fn image_gen_port(&self) -> Option<Arc<dyn plakous_tool_web::ImageGenPort>> {
        match self.0 {}
    }
    /// Returns the exact resolved model handle.
    #[must_use]
    pub fn model(&self) -> Arc<dyn LanguageModel> {
        match self.0 {}
    }
    /// Returns the complete provider configuration authority.
    #[must_use]
    pub fn configuration(&self) -> &CompiledProviderConfiguration {
        match self.0 {}
    }
    /// Returns the exact catalog-selected model.
    #[must_use]
    pub fn selected_model(&self) -> &ModelId {
        match self.0 {}
    }
    /// Returns the fresh execution authority obtained during this resolution.
    #[must_use]
    pub fn initial_authority(&self) -> &RuntimeModelAuthority {
        match self.0 {}
    }
    /// Returns a one-way binding to the exact selected login epoch. A fresh
    /// credential generation preserves this value; relogin does not.
    ///
    /// # Errors
    ///
    /// Returns the digest error if the binding input cannot be hashed.
    pub fn credential_selection_binding(&self) -> Result<Digest, DigestError> {
        match self.0 {}
    }
    /// Returns the sealed model/configuration aggregate for agent construction.
    #[must_use]
    pub fn configured_model(&self) -> ConfiguredLanguageModel {
        match self.0 {}
    }
    /// Returns the exact native tool mode admitted by fresh resolution evidence.
    ///
    /// Non-Codex providers and missing or unknown catalog values return `None`.
    /// No model-name, shell, route, or neighboring-catalog inference is used.
    #[must_use]
    pub fn native_tool_mode(&self) -> Option<NativeToolMode> {
        match self.0 {}
    }
    /// Requires one exact native tool mode before product dispatch is constructed.
    ///
    /// # Errors
    ///
    /// Returns [`NativeToolModeRequirementError`] when evidence admitted a
    /// different mode or did not admit a recognized mode.
    pub fn require_native_tool_mode(
        &self,
        required: NativeToolMode,
    ) -> Result<(), NativeToolModeRequirementError> {
        super::models::require_exact_native_tool_mode(self.native_tool_mode(), required)
    }
    /// Returns the end of the admitted catalog evidence window of a
    /// turn-scoped Codex or SuperGrok resolution, past which only
    /// [`Self::reresolve`] can continue. `None` for routes without a window.
    #[must_use]
    pub fn admitted_until(&self) -> Option<Timestamp> {
        match self.0 {}
    }
    /// Resolves the same exact selected configuration afresh.
    ///
    /// The resolution runs the same exact path as the original one (claimed
    /// refresh when due, authenticated catalog at the exact route) and spends
    /// its auth and catalog permits from the same turn-scoped gate. The fresh
    /// resolution is returned only when the selected and effective
    /// configuration identities, account, login selection epoch, credential
    /// authority, route fingerprint (Codex: the whole admitted catalog row),
    /// native tool mode and content-filter guidance are unchanged and the
    /// credential generation has not decreased. Nothing falls back.
    ///
    /// # Errors
    ///
    /// Returns [`super::NativeSubscriptionError::ReresolutionBindingChanged`] when
    /// any of those changed, [`super::NativeSubscriptionError::ProviderConfigurationRejected`]
    /// for a resolution without a window, or the fresh resolution's own failure.
    #[allow(
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        reason = "disabled resolutions retain the compiled async API contract"
    )]
    pub async fn reresolve(&self) -> Result<Self, NativeModelResolutionError> {
        match self.0 {}
    }
}
