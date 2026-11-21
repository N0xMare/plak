use super::NativeAuthStatus;

#[cfg(any(feature = "codex", feature = "claude"))]
#[allow(
    clippy::match_same_arms,
    reason = "unknown non-exhaustive lifecycle states fail closed as quarantined"
)]
pub(super) fn map_offline_lifecycle(
    status: plakous_auth::OfflineCredentialLifecycle,
) -> NativeAuthStatus {
    use plakous_auth::OfflineCredentialLifecycle as Lifecycle;
    match status {
        Lifecycle::SignedOut => NativeAuthStatus::SignedOut,
        Lifecycle::Ready => NativeAuthStatus::Ready,
        Lifecycle::RefreshRequired => NativeAuthStatus::RefreshRequired,
        Lifecycle::Quarantined => NativeAuthStatus::Quarantined,
        Lifecycle::Refreshing => NativeAuthStatus::Refreshing,
        Lifecycle::Revoked => NativeAuthStatus::Revoked,
        Lifecycle::LoginRequired => NativeAuthStatus::LoginRequired,
        Lifecycle::WrittenByNewerBuild => NativeAuthStatus::WrittenByNewerBuild,
        _ => NativeAuthStatus::Quarantined,
    }
}

#[cfg(any(feature = "grok", feature = "claude"))]
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
