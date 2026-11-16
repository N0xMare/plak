use crate::home::paths::PlakPaths;
use plakous_hosted::{account::AccountRef, openrouter, subscription_usage::VerticalObservation};

pub(crate) fn legacy(paths: &PlakPaths, account: &AccountRef) -> VerticalObservation {
    let credentials = super::ProductOpenRouterCredentials::shared(paths);
    match openrouter::probe_openrouter_key_usage(credentials.as_ref(), account) {
        openrouter::OpenRouterKeyUsageProbe::Failed(reason) => {
            VerticalObservation::Unavailable(reason)
        }
        _ => VerticalObservation::NotConnected,
    }
}
#[cfg(feature = "web")]
pub(crate) mod console {
    use super::{AccountRef, PlakPaths, VerticalObservation, legacy};
    pub(crate) fn read(
        paths: &PlakPaths,
        account: &AccountRef,
    ) -> std::future::Ready<VerticalObservation> {
        std::future::ready(legacy(paths, account))
    }
}
