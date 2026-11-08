use super::{
    OpenRouterKeyUsageProbe, ProductOpenRouterRuntime, ProductOpenRouterSpec, SessionRuntimeBinding,
};
use crate::{CliFailure, CliResult};
#[derive(Clone, Debug)]
pub enum OpenRouterChatModelEvidence {}
impl OpenRouterChatModelEvidence {
    pub fn context_length(&self) -> u64 {
        match *self {}
    }
}
#[derive(Clone, Debug)]
pub enum OpenRouterKeyUsage {}
pub trait OpenRouterCredentials: Send + Sync + 'static {
    fn preflight(&self, _account: &crate::account::AccountRef) -> CliResult<()>;
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
pub fn construct_openrouter_compaction_runtime(
    _: ProductOpenRouterSpec,
) -> std::future::Ready<CliResult<SessionRuntimeBinding>> {
    std::future::ready(Err(unavailable()))
}
pub fn probe_openrouter_key_usage(
    _: &dyn OpenRouterCredentials,
    _: &crate::account::AccountRef,
) -> OpenRouterKeyUsageProbe {
    OpenRouterKeyUsageProbe::Failed(unavailable().to_string())
}

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
