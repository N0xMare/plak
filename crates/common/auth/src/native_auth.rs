//! Provider-neutral primitives shared by native subscription adapters.

use plakous_model::{
    ProviderConfigurationError, ProviderSemanticEvidence, SubscriptionUsageCredentialState,
    SubscriptionUsageUnavailable,
};
use serde::Serialize;

use crate::CredentialStatus;

/// Names the exact local refusal for a record that is not Ready; `None` means
/// the record is Ready and the read may proceed to lease it.
#[must_use]
pub const fn usage_credential_refusal(
    status: CredentialStatus,
) -> Option<SubscriptionUsageUnavailable> {
    match status {
        CredentialStatus::Ready => None,
        CredentialStatus::RefreshRequired => Some(SubscriptionUsageUnavailable::CredentialState(
            SubscriptionUsageCredentialState::RefreshRequired,
        )),
        CredentialStatus::RefreshOutcomeUnknown => {
            Some(SubscriptionUsageUnavailable::CredentialState(
                SubscriptionUsageCredentialState::Quarantined,
            ))
        }
        CredentialStatus::Revoked => Some(SubscriptionUsageUnavailable::CredentialState(
            SubscriptionUsageCredentialState::Revoked,
        )),
    }
}

/// Encodes the adapter-owned semantic projection without changing its JSON shape.
///
/// # Errors
///
/// Returns the same identity error for invalid serialization or evidence.
pub fn semantic_provider_evidence(
    revision: u32,
    projection: &impl Serialize,
) -> Result<ProviderSemanticEvidence, ProviderConfigurationError> {
    let bytes = serde_json::to_vec(projection).map_err(|_| ProviderConfigurationError::Identity)?;
    ProviderSemanticEvidence::new(revision, bytes)
}
