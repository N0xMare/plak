//! Session usage observes the account through its route custody.

use crate::home::paths::PlakPaths;
use plakous_hosted::{
    account::AccountRef,
    openrouter::{self, OpenRouterKeyUsageProbe},
    subscription_usage::VerticalObservation,
};

pub(crate) fn legacy(
    paths: &PlakPaths,
    custody: &crate::gateway::HostCustody,
    account: &AccountRef,
) -> VerticalObservation {
    let credentials = super::ProductOpenRouterCredentials::shared(paths, custody.clone());
    project(openrouter::probe_openrouter_key_usage(
        credentials.as_ref(),
        account,
    ))
}

fn project(probe: OpenRouterKeyUsageProbe) -> VerticalObservation {
    match probe {
        OpenRouterKeyUsageProbe::NotConfigured => VerticalObservation::NotConnected,
        OpenRouterKeyUsageProbe::Ok(usage, label) => {
            VerticalObservation::OpenRouterKey(usage, label)
        }
        OpenRouterKeyUsageProbe::Failed(reason) => VerticalObservation::Unavailable(reason),
        OpenRouterKeyUsageProbe::Rejected { reason, source } => {
            VerticalObservation::RejectedKey { reason, source }
        }
    }
}
