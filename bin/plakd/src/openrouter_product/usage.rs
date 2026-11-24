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

pub(crate) async fn read_only(
    paths: &PlakPaths,
    custody: &crate::gateway::HostCustody,
    account: &AccountRef,
) -> VerticalObservation {
    use plakous::gateway::custody::KeySource;
    match custody.gateway_key_facts(account.as_str()).await {
        Ok(Some(key)) if key.source == KeySource::Missing => {
            return VerticalObservation::NotConnected;
        }
        Ok(_) => {}
        Err(error) => {
            return VerticalObservation::Unavailable(error.to_string());
        }
    }
    let credentials = super::ProductOpenRouterCredentials::new(paths.clone(), custody.clone());
    project(openrouter::read_openrouter_key_usage(&credentials, account).await)
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
