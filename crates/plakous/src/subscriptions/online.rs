use std::path::PathBuf;

use plakous_auth::CredentialSlotId;

use super::custody::SubscriptionGateway;
use super::{
    CompatibilityNotice, LoadedAcceptance, NativeAuthStatus, NativeProvider,
    NativeSubscriptionError, NativeSubscriptions, Timestamp, detached, run_blocking,
};

pub(super) struct LocalLogin {
    pub(super) notice: CompatibilityNotice,
    pub(super) acceptance: LoadedAcceptance,
    pub(super) path: PathBuf,
    pub(super) slot: CredentialSlotId,
}

impl NativeSubscriptions {
    pub(super) async fn prepare_local_login(
        &self,
        provider: NativeProvider,
        enforce_login_recovery: bool,
    ) -> Result<LocalLogin, NativeSubscriptionError> {
        let paths = self.state.clone();
        let notice = Self::notice(provider)?;
        let now = self.state.now()?;
        run_blocking(move || paths.prepare()).await??;
        // Refuse before reading consent or opening a provider manager:
        // an online path must not write over a newer build's state.
        if self.written_by_newer_build_unlocked(provider).await? {
            return Err(NativeSubscriptionError::CredentialWrittenByNewerBuild);
        }
        let paths = self.state.clone();
        let acceptance = run_blocking(move || paths.load_acceptance_exact(notice, now)).await??;
        let path = self.state.provider_record(provider);
        let slot = self.state.slot_id()?;
        let status = super::vertical::require_vertical(provider)?
            .offline_status(self)
            .await?;
        if enforce_login_recovery {
            self.require_recovered_login(provider, &acceptance.generation, status)
                .await?;
        }
        Ok(LocalLogin {
            notice,
            acceptance,
            path,
            slot,
        })
    }

    pub(super) async fn gateway_login_consent(
        &self,
        gateway: &SubscriptionGateway,
        provider: NativeProvider,
    ) -> Result<(CompatibilityNotice, Timestamp), NativeSubscriptionError> {
        let notice = Self::notice(provider)?;
        self.require_gateway_consent(gateway, provider).await?;
        let timestamp =
            Timestamp::new(self.state.now()?, 0).map_err(|_| NativeSubscriptionError::Internal)?;
        Ok((notice, timestamp))
    }
}

pub(super) fn auth_client() -> Result<plakous_http::Client, NativeSubscriptionError> {
    // A claimed OAuth POST cannot safely retry after an HTTP/2 stream reset.
    plakous_http::Client::new_http1(plakous_http::Limits::default())
        .map_err(|_| NativeSubscriptionError::Internal)
}

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

pub(super) async fn run_refresh<E: Send + 'static>(
    force: bool,
    forced: impl Future<Output = Result<(), E>> + Send + 'static,
    if_due: impl Future<Output = Result<(), E>>,
    map_error: fn(E) -> NativeSubscriptionError,
) -> Result<(), NativeSubscriptionError> {
    if force {
        detached(forced).await?
    } else {
        if_due.await
    }
    .map_err(map_error)
}
