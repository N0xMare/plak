use crate::home::paths::PlakPaths;
use plakous_hosted::{account::AccountRef, openrouter, subscription_usage::VerticalObservation};

pub(crate) fn legacy(
    paths: &PlakPaths,
    custody: &crate::gateway::HostCustody,
    account: &AccountRef,
) -> VerticalObservation {
    let credentials = super::ProductOpenRouterCredentials::shared(paths, custody.clone());
    match openrouter::probe_openrouter_key_usage(credentials.as_ref(), account) {
        openrouter::OpenRouterKeyUsageProbe::Failed(reason) => {
            VerticalObservation::Unavailable(reason)
        }
        _ => VerticalObservation::NotConnected,
    }
}
