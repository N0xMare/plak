use super::{ApiCatalogSource, ApiModelCatalog, ApiModelLists, PlatformProvider};
use crate::{CliFailure, CliResult};
pub trait PlatformCredentials: Send + Sync + 'static {}
impl<T: Send + Sync + 'static> PlatformCredentials for T {}
pub use PlatformCredentials as OpenAiCredentials;
pub use PlatformCredentials as AnthropicCredentials;
pub use PlatformCredentials as XaiCredentials;
pub(crate) type AdmittedModel = ();
fn unavailable(provider: PlatformProvider) -> CliFailure {
    crate::capabilities::CapabilityUnavailable {
        id: provider.account(),
        kind: crate::capabilities::CapabilityKind::Route,
        feature: provider.account(),
    }
    .into()
}
pub(super) fn bundled(provider: PlatformProvider, source: ApiCatalogSource) -> ApiModelCatalog {
    ApiModelCatalog {
        provider,
        source,
        models: Vec::new(),
    }
}
pub fn api_model_catalog(
    provider: PlatformProvider,
    _: &crate::account::AccountRef,
    _: &dyn PlatformCredentials,
    _: Option<&std::path::Path>,
    _: ApiModelLists,
    _: u64,
) -> std::future::Ready<CliResult<ApiModelCatalog>> {
    std::future::ready(Err(unavailable(provider)))
}
pub(crate) fn openai_profile(_: &str) -> Option<&'static str> {
    None
}
pub(crate) fn build_runtime(
    provider: PlatformProvider,
    parent: &crate::agent::ApiTopologyParent,
    _: bool,
    _: crate::agent::SessionToolFence,
    _: &crate::tools::ProductToolExtensions,
) -> std::future::Ready<CliResult<plakous::sessions::SessionRuntimeBinding>> {
    let admission = parent.state_roots.platform_admission();
    let _ = (
        admission.lists,
        admission.window,
        admission.created_evidence.as_deref(),
    );
    std::future::ready(Err(unavailable(provider)))
}
pub(crate) fn carry_input_window<P: plakous_host_protocol::ProtocolExtension>(
    provider: PlatformProvider,
    _: &crate::runtime::HostConfig<P>,
) -> CliResult<plakous::sessions::SessionInputWindow> {
    Err(unavailable(provider))
}
