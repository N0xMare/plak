//! Sealed native verticals; absent entries refuse before state or transport work.
use super::{
    Arc, CompatibilityNotice, CompiledProviderConfiguration, NativeAuthStatus, NativeLoginMode,
    NativeModelResolutionError, NativeModelSelector, NativeModelSummary, NativeProvider,
    NativeSubscriptionError, NativeSubscriptions, NativeToolMode, PendingNativeLogin,
    ResolvedNativeModel, RuntimeModelAuthority, SelectedProviderConfiguration, SessionLineage,
    SubscriptionRequestGate, SubscriptionUsageRead,
};
pub(super) type NativeFuture<'a, T> = std::pin::Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub(super) trait NativeVertical: Sync {
    fn enrolled_models(&self) -> Vec<&'static str>;
    fn enrolled_tool_mode(&self, model: &str) -> Option<NativeToolMode>;
    fn enrolls(&self, model: &str) -> bool;
    fn route_ids(
        &self,
    ) -> (
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
    );
    fn notice(&self) -> CompatibilityNotice;
    fn hosted_retrieval(&self) -> plakous_model::HostedRetrievalTable;
    fn maximum_output_tokens(&self, model: &str) -> Option<u32>;
    fn offline_status<'a>(
        &self,
        subscriptions: &'a NativeSubscriptions,
    ) -> NativeFuture<'a, Result<NativeAuthStatus, NativeSubscriptionError>>;
    fn upstream_account_fingerprint<'a>(
        &self,
        subscriptions: &'a NativeSubscriptions,
    ) -> NativeFuture<'a, Result<Option<plakous_model::Digest>, NativeSubscriptionError>>;
    fn access_expires_at<'a>(
        &self,
        subscriptions: &'a NativeSubscriptions,
    ) -> NativeFuture<'a, Result<u64, NativeSubscriptionError>>;
    fn refresh<'a>(
        &self,
        subscriptions: &'a NativeSubscriptions,
        force: bool,
    ) -> NativeFuture<'a, Result<NativeAuthStatus, NativeSubscriptionError>>;
    fn revoke<'a>(
        &self,
        subscriptions: &'a NativeSubscriptions,
    ) -> NativeFuture<'a, Result<NativeAuthStatus, NativeSubscriptionError>>;
    fn logout<'a>(
        &self,
        subscriptions: &'a NativeSubscriptions,
    ) -> NativeFuture<'a, Result<NativeAuthStatus, NativeSubscriptionError>>;
    fn catalog<'a>(
        &self,
        subscriptions: &'a NativeSubscriptions,
        gate: Arc<dyn SubscriptionRequestGate>,
    ) -> NativeFuture<'a, Result<Vec<NativeModelSummary>, NativeSubscriptionError>>;
    fn resolve<'a>(
        &self,
        subscriptions: &'a NativeSubscriptions,
        model_selector: &'a NativeModelSelector,
        gate: Arc<dyn SubscriptionRequestGate>,
        attach: bool,
        lineage: SessionLineage,
    ) -> NativeFuture<
        'a,
        Result<(ResolvedNativeModel, Option<RuntimeModelAuthority>), NativeModelResolutionError>,
    >;
    fn begin_login<'a>(
        &self,
        subscriptions: &'a NativeSubscriptions,
        mode: NativeLoginMode,
        replaced_unusable: bool,
    ) -> NativeFuture<'a, Result<PendingNativeLogin, NativeSubscriptionError>>;
    fn configure(
        &self,
        selected: &SelectedProviderConfiguration,
        resolved: &ResolvedNativeModel,
        authority: &RuntimeModelAuthority,
    ) -> Result<CompiledProviderConfiguration, NativeModelResolutionError>;
    fn tool_mode(&self, resolved: &ResolvedNativeModel) -> Option<NativeToolMode>;
    fn usage_reader<'a>(
        &self,
        subscriptions: &'a NativeSubscriptions,
    ) -> NativeFuture<
        'a,
        Result<NativeFuture<'static, SubscriptionUsageRead>, NativeSubscriptionError>,
    >;
}
pub(super) struct NativeVerticalEntry {
    pub(super) provider: NativeProvider,
    pub(super) vertical: &'static dyn NativeVertical,
}
const VERTICALS: &[&NativeVerticalEntry] = &[
    #[cfg(feature = "codex")]
    &super::codex::ENTRY,
    #[cfg(feature = "grok")]
    &super::grok::ENTRY,
    #[cfg(feature = "claude")]
    &super::claude::ENTRY,
];
pub(super) fn vertical(provider: NativeProvider) -> Option<&'static dyn NativeVertical> {
    VERTICALS
        .iter()
        .find(|entry| entry.provider == provider)
        .map(|entry| entry.vertical)
}
pub(super) fn require_vertical(
    provider: NativeProvider,
) -> Result<&'static dyn NativeVertical, NativeSubscriptionError> {
    vertical(provider).ok_or(NativeSubscriptionError::NotCompiled(provider))
}
