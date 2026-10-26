use super::NativeAuthStatus;

#[allow(
    clippy::match_same_arms,
    reason = "unknown non-exhaustive lifecycle states fail closed as quarantined"
)]
pub(super) fn map_credential_status(status: plakous_auth::CredentialStatus) -> NativeAuthStatus {
    match status {
        plakous_auth::CredentialStatus::Ready => NativeAuthStatus::Ready,
        plakous_auth::CredentialStatus::RefreshRequired => NativeAuthStatus::RefreshRequired,
        plakous_auth::CredentialStatus::RefreshOutcomeUnknown => NativeAuthStatus::Quarantined,
        plakous_auth::CredentialStatus::Revoked => NativeAuthStatus::Revoked,
        _ => NativeAuthStatus::Quarantined,
    }
}
