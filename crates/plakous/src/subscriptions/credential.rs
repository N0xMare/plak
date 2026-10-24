//! Dispatch admission, refresh, revoke and local logout.

use super::{
    Arc, Duration, ForcedRefresh, Instant, NativeAuthStatus, NativeProvider,
    NativeSubscriptionError, NativeSubscriptionStatus, NativeSubscriptions, PRIMARY_SLOT, detached,
    map_claude_error, map_claude_offline_status, map_codex_error, map_codex_status,
    map_credential_status, map_xai_error, map_xai_offline_status, run_blocking,
};

/// Bound on how long dispatch admission waits for another owner's claimed
/// refresh to settle; a refresh exchange is bounded well inside it.
const REFRESH_IN_FLIGHT_WAIT: Duration = Duration::from_secs(30);
const REFRESH_IN_FLIGHT_POLL: Duration = Duration::from_millis(200);

/// The single credential admission rule for every dispatch-shaped path: model
/// catalog, inference, and child spawn.
///
/// Ready is admitted for every provider. xAI Quarantined is admitted because
/// its durable Unknown is retryable on the stored refresh token;
/// the next refresh reclaims it. Codex and Claude stay Ready-only.
#[must_use]
pub const fn dispatch_credential_admitted(
    provider: NativeProvider,
    credential: NativeAuthStatus,
) -> bool {
    matches!(
        (provider, credential),
        (_, NativeAuthStatus::Ready) | (NativeProvider::Xai, NativeAuthStatus::Quarantined)
    )
}

impl NativeSubscriptions {
    /// Admits one provider for a dispatch-shaped use (child spawn) exactly as
    /// the model-catalog and inference paths do: refresh first, then read the
    /// durable status, then apply [`dispatch_credential_admitted`].
    ///
    /// The refresh is best effort: its failure is reported through the
    /// fail-closed status read that follows, never swallowed into Ready. A
    /// durable xAI Unknown is reclaimed on the stored refresh token here, so a
    /// crash mid-refresh does not refuse the next child.
    ///
    /// # Errors
    ///
    /// Returns [`NativeSubscriptionError::CredentialUnavailable`] when the
    /// admission rule refuses the credential, and the status read's own
    /// storage errors otherwise.
    pub async fn admit_for_dispatch(
        &self,
        provider: NativeProvider,
    ) -> Result<NativeSubscriptionStatus, NativeSubscriptionError> {
        self.await_refresh_in_flight(provider).await?;
        let _ = self.refresh(provider).await;
        let status = self.await_refresh_in_flight(provider).await?;
        if !dispatch_credential_admitted(provider, status.credential()) {
            return Err(NativeSubscriptionError::CredentialUnavailable);
        }
        Ok(status)
    }

    /// Reads the status, waiting a bounded time while a refresh another task
    /// or process claimed is in flight, so admission judges its outcome
    /// rather than the transient Unknown record it writes first.
    async fn await_refresh_in_flight(
        &self,
        provider: NativeProvider,
    ) -> Result<NativeSubscriptionStatus, NativeSubscriptionError> {
        let deadline = Instant::now() + REFRESH_IN_FLIGHT_WAIT;
        loop {
            let status = self.status(provider).await?;
            if status.credential() != NativeAuthStatus::Refreshing || Instant::now() >= deadline {
                return Ok(status);
            }
            run_blocking(|| std::thread::sleep(REFRESH_IN_FLIGHT_POLL)).await?;
        }
    }

    /// Ensures the exact selected subscription credential is fresh enough for
    /// inference, without claiming that an already-fresh credential rotated.
    ///
    /// # Errors
    ///
    /// Codex/Claude leave an ambiguous rotating-token result quarantined.
    /// xAI retries the still-stored refresh token on a new claimed generation.
    ///
    pub async fn refresh(
        &self,
        provider: NativeProvider,
    ) -> Result<NativeAuthStatus, NativeSubscriptionError> {
        let lock = self.acquire_compatibility_lock(provider).await?;
        let status = self.refresh_locked(provider, false).await?;
        lock.verify()?;
        Ok(status)
    }

    /// Claims one refresh grant regardless of the stored expiry and reports
    /// the new access-token expiry. It never reports success without a grant.
    ///
    /// # Errors
    ///
    /// The same fail-closed failures as [`Self::refresh`].
    pub async fn refresh_now(
        &self,
        provider: NativeProvider,
    ) -> Result<ForcedRefresh, NativeSubscriptionError> {
        let lock = self.acquire_compatibility_lock(provider).await?;
        let status = self.refresh_locked(provider, true).await?;
        let access_expires_at = self.access_expires_at_locked(provider).await?;
        lock.verify()?;
        Ok(ForcedRefresh {
            status,
            access_expires_at,
        })
    }

    /// Reports whether the stored access token of a `Ready` credential has
    /// expired, from local state only: no provider network I/O and no claim.
    /// The offline status stays `Ready` for such a token because the next
    /// refresh renews it.
    ///
    /// # Errors
    ///
    /// Fails closed for a missing, non-`Ready`, or unreadable record.
    pub async fn access_token_expired(
        &self,
        provider: NativeProvider,
    ) -> Result<bool, NativeSubscriptionError> {
        let lock = self.acquire_compatibility_lock(provider).await?;
        let access_expires_at = self.access_expires_at_locked(provider).await?;
        lock.verify()?;
        Ok(access_expires_at <= self.state.now()?)
    }

    /// Stored access-token expiry under the held compatibility lock; opening
    /// the provider manager performs no network I/O.
    async fn access_expires_at_locked(
        &self,
        provider: NativeProvider,
    ) -> Result<u64, NativeSubscriptionError> {
        match provider {
            NativeProvider::Codex => self
                .codex_online()
                .await?
                .manager
                .access_expires_at()
                .map_err(map_codex_error),
            NativeProvider::Claude => self
                .claude_online()
                .await?
                .manager
                .access_expires_at()
                .map_err(map_claude_error),
            NativeProvider::Xai => self
                .xai_online()
                .await?
                .manager
                .access_expires_at()
                .map_err(map_xai_error),
        }
    }

    /// Runs one refresh under the held compatibility lock; `force` claims a
    /// grant even when the stored access token is still fresh.
    pub(super) async fn refresh_locked(
        &self,
        provider: NativeProvider,
        force: bool,
    ) -> Result<NativeAuthStatus, NativeSubscriptionError> {
        let status = match provider {
            NativeProvider::Codex => {
                let online = self.codex_online().await?;
                let manager = Arc::clone(&online.manager);
                if force {
                    detached(async move { manager.refresh_after_rejection().await })
                        .await?
                        .map_err(map_codex_error)?;
                } else {
                    online
                        .manager
                        .refresh_if_needed()
                        .await
                        .map_err(map_codex_error)?;
                }
                online
                    .manager
                    .status()
                    .map(map_codex_status)
                    .map_err(map_codex_error)
            }
            NativeProvider::Claude => {
                let online = self.claude_online().await?;
                let manager = Arc::clone(&online.manager);
                if force {
                    detached(async move { manager.refresh_after_rejection().await })
                        .await?
                        .map_err(map_claude_error)?;
                } else {
                    online
                        .manager
                        .refresh_if_needed()
                        .await
                        .map_err(map_claude_error)?;
                }
                online
                    .manager
                    .status()
                    .map(|status| {
                        status.map_or(NativeAuthStatus::SignedOut, |value| {
                            map_credential_status(value.status())
                        })
                    })
                    .map_err(map_claude_error)
            }
            NativeProvider::Xai => {
                let online = self.xai_online().await?;
                let manager = Arc::clone(&online.manager);
                if force {
                    detached(async move { manager.refresh_now().await })
                        .await?
                        .map_err(map_xai_error)?;
                } else {
                    online
                        .manager
                        .refresh_if_needed()
                        .await
                        .map_err(map_xai_error)?;
                }
                online
                    .manager
                    .status()
                    .map(|status| {
                        status.map_or(NativeAuthStatus::SignedOut, |value| {
                            map_credential_status(value.status())
                        })
                    })
                    .map_err(map_xai_error)
            }
        }?;
        // Refresh legitimately advances durable credential generations. Re-pin
        // the cleared login-recovery marker so the recovery gate does not treat
        // the owner-managed rotation as a foreign slot takeover.
        if matches!(
            status,
            NativeAuthStatus::Ready | NativeAuthStatus::RefreshRequired
        ) {
            let paths = self.state.clone();
            let consent = {
                let paths = self.state.clone();
                let notice = Self::notice(provider);
                let now = self.state.now()?;
                run_blocking(move || paths.load_acceptance_exact(notice, now)).await??
            };
            let consent_generation = consent.generation.clone();
            run_blocking(move || paths.repin_after_refresh(provider, &consent_generation))
                .await??;
        }
        Ok(status)
    }

    /// Attempts exact remote revocation and commits the provider-defined local
    /// tombstone only after the claimed transaction reaches that terminal.
    ///
    /// # Errors
    ///
    /// Ambiguous remote outcomes remain quarantined; they are not reported as
    /// local logout success.
    pub async fn revoke(
        &self,
        provider: NativeProvider,
    ) -> Result<NativeAuthStatus, NativeSubscriptionError> {
        let lock = self.acquire_compatibility_lock(provider).await?;
        let status = match provider {
            NativeProvider::Codex => self
                .codex_online()
                .await?
                .manager
                .revoke()
                .await
                .map(map_codex_status)
                .map_err(map_codex_error),
            NativeProvider::Xai => {
                self.xai_online()
                    .await?
                    .manager
                    .revoke()
                    .await
                    .map_err(map_xai_error)?;
                Ok(NativeAuthStatus::Revoked)
            }
            NativeProvider::Claude => {
                self.claude_online()
                    .await?
                    .manager
                    .revoke()
                    .map_err(map_claude_error)?;
                Ok(NativeAuthStatus::Revoked)
            }
        }?;
        lock.verify()?;
        Ok(status)
    }

    /// Retires local credential and login-recovery state without acceptance or
    /// network I/O.
    ///
    /// An existing credential is replaced by a revoked tombstone; an absent
    /// credential remains signed out. In both cases, any paired login-recovery
    /// marker is retired. No remote revocation claim is made.
    ///
    /// # Errors
    ///
    /// Fails closed for unsafe, malformed, or stale selected-slot state.
    pub async fn logout_local(
        &self,
        provider: NativeProvider,
    ) -> Result<NativeAuthStatus, NativeSubscriptionError> {
        let lock = self.acquire_compatibility_lock(provider).await?;
        let status = self.logout_local_unlocked(provider).await?;
        lock.verify()?;
        if matches!(
            status,
            NativeAuthStatus::Revoked | NativeAuthStatus::SignedOut
        ) {
            let paths = self.state.clone();
            run_blocking(move || paths.retire_login_recovery(provider)).await??;
        }
        lock.verify()?;
        Ok(status)
    }

    /// Replaces a newer build's login state after the operator explicitly
    /// confirmed it: the stored login is retired locally (that build must log
    /// in again), its recovery state is cleared and a newer receipt removed.
    /// A receipt this build wrote is kept.
    ///
    /// # Errors
    ///
    /// Fails closed for unsafe or stale selected-slot state.
    pub async fn replace_newer_build_state(
        &self,
        provider: NativeProvider,
    ) -> Result<NativeAuthStatus, NativeSubscriptionError> {
        let lock = self.acquire_compatibility_lock(provider).await?;
        if !self.written_by_newer_build_unlocked(provider).await? {
            let status = self.status_unlocked(provider).await?.credential();
            lock.verify()?;
            return Ok(status);
        }
        let status = if self.offline_credential_unlocked(provider).await?
            == NativeAuthStatus::WrittenByNewerBuild
        {
            self.logout_local_unlocked(provider).await?
        } else {
            self.offline_credential_unlocked(provider).await?
        };
        lock.verify()?;
        let paths = self.state.clone();
        let notice = Self::notice(provider);
        run_blocking(move || {
            paths.retire_login_recovery(provider)?;
            paths.retire_newer_receipt(notice)
        })
        .await??;
        lock.verify()?;
        Ok(status)
    }

    pub(super) async fn logout_local_unlocked(
        &self,
        provider: NativeProvider,
    ) -> Result<NativeAuthStatus, NativeSubscriptionError> {
        let paths = self.state.clone();
        run_blocking(move || paths.prepare()).await??;
        match provider {
            NativeProvider::Codex => plakous_openai::CodexSubscriptionLocalAdmin::open(
                self.state.provider_record(provider),
            )
            .await
            .map_err(map_codex_error)?
            .logout_local()
            .await
            .map(map_codex_status)
            .map_err(map_codex_error),
            NativeProvider::Xai => {
                let path = self.state.provider_record(provider);
                run_blocking(move || {
                    let slot = plakous_auth::CredentialSlotId::new(PRIMARY_SLOT)
                        .map_err(|_| NativeSubscriptionError::Internal)?;
                    plakous_xai::XaiOfflineSelectedSlot::open_explicit(path, slot)
                        .map_err(map_xai_error)?
                        .logout_local()
                        .map(map_xai_offline_status)
                        .map_err(map_xai_error)
                })
                .await?
            }
            NativeProvider::Claude => {
                let path = self.state.provider_record(provider);
                run_blocking(move || {
                    let slot = plakous_auth::CredentialSlotId::new(PRIMARY_SLOT)
                        .map_err(|_| NativeSubscriptionError::Internal)?;
                    plakous_anthropic::ClaudeOfflineSelectedSlot::open_explicit(path, slot)
                        .map_err(map_claude_error)?
                        .logout_local()
                        .map(map_claude_offline_status)
                        .map_err(map_claude_error)
                })
                .await?
            }
        }
    }
}
