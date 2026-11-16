use super::{
    OpenRouterKeyUsageProbe, ProductOpenRouterRuntime, ProductOpenRouterSpec, SessionRuntimeBinding,
};
use crate::{CliFailure, CliResult};
/// Uninhabited model evidence: an omitted gateway cannot attest a model.
#[derive(Clone, Debug)]
pub enum OpenRouterChatModelEvidence {}
impl OpenRouterChatModelEvidence {
    /// The admitted model's context window; no value exists in this build.
    #[must_use]
    pub fn context_length(&self) -> u64 {
        match *self {}
    }
}
/// Uninhabited allowance reading: this build never performs the gateway GET.
#[derive(Clone, Debug)]
pub enum OpenRouterKeyUsage {}
/// Credential port retained as vocabulary even when gateway execution is omitted.
pub trait OpenRouterCredentials: Send + Sync + 'static {
    /// Check the exact account's credential readiness without dispatching inference.
    ///
    /// # Errors
    /// Returns the product's exact-account credential or capability refusal.
    fn preflight(&self, _account: &crate::account::AccountRef) -> CliResult<()>;
    /// Return a redaction-safe label, never the key material.
    ///
    /// # Errors
    /// Returns an unavailable-account or capability refusal without fallback.
    fn key_label(&self, _account: &crate::account::AccountRef) -> CliResult<String>;
}
fn unavailable() -> CliFailure {
    crate::capabilities::CapabilityUnavailable {
        id: "openrouter",
        kind: crate::capabilities::CapabilityKind::Route,
        feature: "openrouter",
    }
    .into()
}
/// Resolve immediately to the omitted-gateway refusal without contacting a provider.
pub fn construct_openrouter_runtime(
    _: ProductOpenRouterSpec,
) -> std::future::Ready<CliResult<ProductOpenRouterRuntime>> {
    std::future::ready(Err(unavailable()))
}
pub(crate) fn construct_openrouter_runtime_with_fence(
    _: ProductOpenRouterSpec,
    _: crate::agent::SessionToolFence,
) -> std::future::Ready<CliResult<ProductOpenRouterRuntime>> {
    std::future::ready(Err(unavailable()))
}
/// Refuse compaction construction without a paid dispatch or alternate route.
pub fn construct_openrouter_compaction_runtime(
    _: ProductOpenRouterSpec,
) -> std::future::Ready<CliResult<SessionRuntimeBinding>> {
    std::future::ready(Err(unavailable()))
}
/// Return an explicit unavailable-route result without reading credentials or HTTP.
#[must_use]
pub fn probe_openrouter_key_usage(
    _: &dyn OpenRouterCredentials,
    _: &crate::account::AccountRef,
) -> OpenRouterKeyUsageProbe {
    OpenRouterKeyUsageProbe::Failed(unavailable().to_string())
}

/// Async-shape counterpart of the unavailable allowance probe; never performs I/O.
#[allow(
    clippy::unused_async,
    reason = "the unavailable route retains the available route's async public signature"
)]
pub async fn read_openrouter_key_usage(
    credentials: &dyn OpenRouterCredentials,
    account: &crate::account::AccountRef,
) -> OpenRouterKeyUsageProbe {
    probe_openrouter_key_usage(credentials, account)
}
pub(crate) fn carry_input_window<P: plakous_host_protocol::ProtocolExtension>(
    _: &crate::runtime::HostConfig<P>,
) -> CliResult<plakous::sessions::SessionInputWindow> {
    Err(unavailable())
}
