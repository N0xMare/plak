//! Subscription usage reads.

use super::vertical::NativeFuture;

mod cache;

use super::{
    Future, NativeAuthStatus, NativeProvider, NativeSubscriptionError, NativeSubscriptionStatus,
    NativeSubscriptions, PrivateFilesystemLock, SubscriptionUsageCredentialState,
    SubscriptionUsageRead, SubscriptionUsageUnavailable, Timestamp, run_blocking,
};

#[cfg(feature = "test-hooks")]
pub(super) mod test_usage;
#[cfg(not(feature = "test-hooks"))]
pub(super) mod test_usage {
    #[derive(Clone)]
    pub(in crate::subscriptions) struct Fixture;
    pub(in crate::subscriptions) const EMPTY: Fixture = Fixture;
    impl Fixture {
        #[allow(
            clippy::unused_self,
            reason = "disabled fixture matches the enabled boundary"
        )]
        pub fn read(
            &self,
            _provider: super::NativeProvider,
            _refreshing: bool,
        ) -> Option<super::NativeFuture<'static, super::SubscriptionUsageRead>> {
            None
        }
    }
}

impl NativeSubscriptions {
    /// Non-secret identity of the exact local consent and credential generations.
    /// This is an observation fence, not authority to admit work or reuse data
    /// without revalidating the selected account.
    ///
    /// # Errors
    /// Reports busy or unreadable storage without creating an absent layout.
    pub async fn account_observation_revision(
        &self,
        provider: NativeProvider,
    ) -> Result<Option<[u8; 32]>, SubscriptionUsageUnavailable> {
        if super::vertical::vertical(provider).is_none() {
            return Err(SubscriptionUsageUnavailable::NotConnected);
        }
        self.state
            .require_provider(provider)
            .map_err(|_| SubscriptionUsageUnavailable::LocalState)?;
        let state = self.state.clone();
        if !run_blocking(move || state.layout_exists())
            .await
            .map_err(|_| SubscriptionUsageUnavailable::LocalState)?
            .map_err(|_| SubscriptionUsageUnavailable::LocalState)?
        {
            return Ok(None);
        }
        let lock = self.usage_lock(provider).await?;
        let revision = self
            .usage_snapshot(provider)
            .await
            .map_err(|_| SubscriptionUsageUnavailable::LocalState)?;
        lock.verify()
            .map_err(|_| SubscriptionUsageUnavailable::LocalState)?;
        let encoded =
            serde_json::to_vec(&revision).map_err(|_| SubscriptionUsageUnavailable::LocalState)?;
        Ok(Some(plakous_model::sha256_bytes(&[
            b"plak.console-observation.v1",
            &encoded,
        ])))
    }
    /// Reads allowance once without login, token refresh or a refresh retry.
    /// Console reads coalesce and may consume a fresh exact-fenced observation
    /// produced by a legacy read. Legacy reads never consume this cache.
    /// `before_get` is awaited only for a real GET; its output is retained as
    /// the caller's concurrency permit until that read finishes.
    pub async fn subscription_usage_read_only<Permit>(
        &self,
        provider: NativeProvider,
        before_get: impl Future<Output = Permit>,
    ) -> SubscriptionUsageRead {
        if let Some(fixture) = self.usage_fixture.read(provider, false) {
            let _permit = before_get.await;
            return fixture.await;
        }
        if super::vertical::vertical(provider).is_none() {
            return self.usage_unavailable(SubscriptionUsageUnavailable::NotConnected);
        }
        if self.state.require_provider(provider).is_err() {
            return self.usage_unavailable(SubscriptionUsageUnavailable::LocalState);
        }
        let state = self.state.clone();
        match run_blocking(move || state.layout_exists()).await {
            Ok(Ok(true)) => {}
            Ok(Ok(false)) => {
                return self.usage_unavailable(SubscriptionUsageUnavailable::NotConnected);
            }
            _ => return self.usage_unavailable(SubscriptionUsageUnavailable::LocalState),
        }
        let Some(entry) = cache::entry(self.state.observation_paths(), provider) else {
            return self.usage_unavailable(SubscriptionUsageUnavailable::Busy);
        };
        let _console = entry.console.lock().await;
        let mut before_get = Some(before_get);
        let mut _permit = None;
        loop {
            if let Some(mut changed) = entry.pending() {
                let _ = changed.changed().await;
                continue;
            }
            let lock = match self.usage_lock(provider).await {
                Ok(lock) => lock,
                Err(reason) => return self.usage_unavailable(reason),
            };
            // Recheck under the same guard a legacy read uses to register its
            // in-flight observation, closing the check-then-GET race.
            if let Some(mut changed) = entry.pending() {
                drop(lock);
                let _ = changed.changed().await;
                continue;
            }
            if !matches!(self.status_unlocked(provider).await, Ok(status) if status.compatibility() == super::CompatibilityStatus::Current && status.credential() == NativeAuthStatus::Ready)
            {
                return self.usage_unavailable(self.fenced_credential_reason(provider).await);
            }
            let revision = match self.usage_snapshot(provider).await {
                Ok(revision) if lock.verify().is_ok() => revision,
                _ => return self.usage_unavailable(SubscriptionUsageUnavailable::LocalState),
            };
            if let Some(observation) = entry.cached(&revision) {
                return observation;
            }
            if !cache::can_fetch() {
                return self.usage_unavailable(SubscriptionUsageUnavailable::Busy);
            }
            if let Some(acquire) = before_get.take() {
                drop(lock);
                _permit = Some(acquire.await);
                continue;
            }
            return Box::pin(self.read_usage_prepared(provider, false, lock)).await;
        }
    }

    async fn usage_lock(
        &self,
        provider: NativeProvider,
    ) -> Result<PrivateFilesystemLock, SubscriptionUsageUnavailable> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(100);
        loop {
            match self.try_acquire_compatibility_lock(provider).await {
                Ok(Some(lock)) => return Ok(lock),
                Err(_) => return Err(SubscriptionUsageUnavailable::LocalState),
                Ok(None) if tokio::time::Instant::now() >= deadline => {
                    return Err(SubscriptionUsageUnavailable::Busy);
                }
                Ok(None) => tokio::time::sleep(std::time::Duration::from_millis(5)).await,
            }
        }
    }

    /// Performs one observational subscription allowance read, with one claimed
    /// refresh and retry if the endpoint rejects the access token. Provider
    /// credential leases protect the network read; the compatibility guard is
    /// held only while preparing and checking its local consent snapshot.
    pub async fn subscription_usage(&self, provider: NativeProvider) -> SubscriptionUsageRead {
        if let Some(fixture) = self.usage_fixture.read(provider, true) {
            return fixture.await;
        }
        if super::vertical::vertical(provider).is_none() {
            return self.usage_unavailable(SubscriptionUsageUnavailable::NotConnected);
        }
        let observation = Box::pin(self.read_usage_once(provider, false)).await;
        if matches!(
            observation,
            SubscriptionUsageRead::Unavailable {
                reason: SubscriptionUsageUnavailable::Unauthorized,
                ..
            }
        ) {
            Box::pin(self.read_usage_once(provider, true)).await
        } else {
            observation
        }
    }

    fn usage_unavailable(&self, reason: SubscriptionUsageUnavailable) -> SubscriptionUsageRead {
        SubscriptionUsageRead::Unavailable {
            observed_at: Timestamp::new(self.state.now().unwrap_or(0), 0)
                .unwrap_or_else(|_| unreachable!("Unix epoch is a valid timestamp")),
            reason,
        }
    }

    async fn read_usage_once(
        &self,
        provider: NativeProvider,
        refresh: bool,
    ) -> SubscriptionUsageRead {
        // A pending login may retain this guard through a browser wait. Local
        // siblings can settle, but Usage never waits indefinitely on login.
        let lock = match self.usage_lock(provider).await {
            Ok(lock) => lock,
            Err(reason) => return self.usage_unavailable(reason),
        };
        Box::pin(self.read_usage_prepared(provider, refresh, lock)).await
    }

    async fn read_usage_prepared(
        &self,
        provider: NativeProvider,
        refresh: bool,
        lock: PrivateFilesystemLock,
    ) -> SubscriptionUsageRead {
        if refresh
            && !matches!(
                self.refresh_locked(provider, true).await,
                Ok(NativeAuthStatus::Ready)
            )
        {
            return self.usage_unavailable(self.fenced_credential_reason(provider).await);
        }
        let reader = match self.usage_reader(provider).await {
            Ok(reader) => reader,
            Err(error) => {
                let reason = match error {
                    NativeSubscriptionError::CredentialUnavailable => {
                        self.fenced_credential_reason(provider).await
                    }
                    NativeSubscriptionError::CredentialProfileChanged => {
                        SubscriptionUsageUnavailable::CredentialState(
                            SubscriptionUsageCredentialState::LoginRequired,
                        )
                    }
                    NativeSubscriptionError::CredentialWrittenByNewerBuild => {
                        SubscriptionUsageUnavailable::CredentialState(
                            SubscriptionUsageCredentialState::WrittenByNewerBuild,
                        )
                    }
                    NativeSubscriptionError::NotCompiled(_) => {
                        SubscriptionUsageUnavailable::NotConnected
                    }
                    _ => SubscriptionUsageUnavailable::LocalState,
                };
                return self.usage_unavailable(reason);
            }
        };
        Box::pin(self.observe_usage_unlocked(provider, lock, reader)).await
    }

    async fn usage_reader(
        &self,
        provider: NativeProvider,
    ) -> Result<NativeFuture<'static, SubscriptionUsageRead>, NativeSubscriptionError> {
        super::vertical::require_vertical(provider)?
            .usage_reader(self)
            .await
    }

    async fn usage_snapshot(
        &self,
        provider: NativeProvider,
    ) -> Result<(Option<String>, Option<u64>), NativeSubscriptionError> {
        let state = self.state.clone();
        run_blocking(move || {
            Ok((
                state.stored_acceptance_generation(provider)?,
                state.current_credential_generation(provider)?,
            ))
        })
        .await?
    }

    async fn observe_usage_unlocked(
        &self,
        provider: NativeProvider,
        lock: PrivateFilesystemLock,
        read: impl Future<Output = SubscriptionUsageRead>,
    ) -> SubscriptionUsageRead {
        let before = match self.usage_snapshot(provider).await {
            Ok(snapshot) if lock.verify().is_ok() => snapshot,
            _ => return self.usage_unavailable(SubscriptionUsageUnavailable::LocalState),
        };
        let flight = cache::Flight::start(self.state.observation_paths(), provider);
        drop(lock);
        let observation = read.await;
        let lock = match self.usage_lock(provider).await {
            Ok(lock) => lock,
            Err(reason) => return self.usage_unavailable(reason),
        };
        match self.usage_snapshot(provider).await {
            Ok(after) if before == after && lock.verify().is_ok() => {
                flight.publish(after, &observation);
                observation
            }
            _ => self.usage_unavailable(SubscriptionUsageUnavailable::AccountChanged),
        }
    }

    /// Names the credential lifecycle rather than claiming a transport fault.
    async fn fenced_credential_reason(
        &self,
        provider: NativeProvider,
    ) -> SubscriptionUsageUnavailable {
        use SubscriptionUsageCredentialState as C;
        match self
            .status_unlocked(provider)
            .await
            .map(NativeSubscriptionStatus::credential)
        {
            Ok(NativeAuthStatus::SignedOut) => SubscriptionUsageUnavailable::NotConnected,
            Ok(NativeAuthStatus::RefreshRequired) => {
                SubscriptionUsageUnavailable::CredentialState(C::RefreshRequired)
            }
            Ok(NativeAuthStatus::Quarantined) => {
                SubscriptionUsageUnavailable::CredentialState(C::Quarantined)
            }
            Ok(NativeAuthStatus::Refreshing) => SubscriptionUsageUnavailable::Busy,
            Ok(NativeAuthStatus::Revoked) => {
                SubscriptionUsageUnavailable::CredentialState(C::Revoked)
            }
            Ok(NativeAuthStatus::LoginRequired) => {
                SubscriptionUsageUnavailable::CredentialState(C::LoginRequired)
            }
            Ok(NativeAuthStatus::WrittenByNewerBuild) => {
                SubscriptionUsageUnavailable::CredentialState(C::WrittenByNewerBuild)
            }
            Ok(NativeAuthStatus::Ready) | Err(_) => SubscriptionUsageUnavailable::LocalState,
        }
    }
}

#[cfg(test)]
mod tests;
