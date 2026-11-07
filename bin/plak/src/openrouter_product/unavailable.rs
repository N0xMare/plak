use super::OpenRouterCatalogProbe;
use crate::home::paths::PlakPaths;
use plakous_hosted::{CliFailure, CliResult, openrouter::OpenRouterCredentials};
use std::sync::Arc;
pub struct ProductOpenRouterCredentials;
impl ProductOpenRouterCredentials {
    pub fn new(_: PlakPaths) -> Self {
        Self
    }
    pub fn shared(_: &PlakPaths) -> Arc<dyn OpenRouterCredentials> {
        Arc::new(Self)
    }
}
impl OpenRouterCredentials for ProductOpenRouterCredentials {
    fn preflight(&self, _account: &plakous_hosted::account::AccountRef) -> CliResult<()> {
        Err(unavailable())
    }
    fn key_label(&self, _account: &plakous_hosted::account::AccountRef) -> CliResult<String> {
        Err(unavailable())
    }
}
fn unavailable() -> CliFailure {
    plakous_hosted::capabilities::CapabilityUnavailable {
        id: "openrouter",
        kind: plakous_hosted::capabilities::CapabilityKind::Route,
        feature: "openrouter",
    }
    .into()
}
pub fn list_models(_: &PlakPaths) -> std::future::Ready<CliResult<Vec<String>>> {
    std::future::ready(Err(unavailable()))
}
pub fn probe_openrouter_catalog_allowlist(
    _: &PlakPaths,
    _: Option<&str>,
    _: Option<&str>,
) -> OpenRouterCatalogProbe {
    OpenRouterCatalogProbe::Failed(unavailable().to_string())
}
