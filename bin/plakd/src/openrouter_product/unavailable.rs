use crate::home::paths::PlakPaths;
use plakous_hosted::{CliFailure, CliResult, openrouter::OpenRouterCredentials};
use std::sync::Arc;
pub struct ProductOpenRouterCredentials;
impl ProductOpenRouterCredentials {
    pub(crate) const fn new(_: PlakPaths, _: crate::gateway::HostCustody) -> Self {
        Self
    }
    pub(crate) fn shared(
        _: &PlakPaths,
        _: crate::gateway::HostCustody,
    ) -> Arc<dyn OpenRouterCredentials> {
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
pub(crate) fn gateway_usage(
    _: serde_json::Value,
    _: u64,
    _: &str,
) -> CliResult<plakous_hosted::subscription_usage::VerticalObservation> {
    Err(unavailable())
}
pub fn list_models(
    _: &PlakPaths,
    _: &plakous_hosted::account::AccountRef,
) -> std::future::Ready<CliResult<Vec<String>>> {
    std::future::ready(Err(unavailable()))
}
