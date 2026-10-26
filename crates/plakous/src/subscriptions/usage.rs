//! Subscription usage reads.

use super::vertical::NativeFuture;

use super::{
    Future, NativeAuthStatus, NativeProvider, NativeSubscriptionError, NativeSubscriptionStatus,
    NativeSubscriptions, PrivateFilesystemLock, SubscriptionUsageCredentialState,
    SubscriptionUsageRead, SubscriptionUsageUnavailable, Timestamp, run_blocking,
};

impl NativeSubscriptions {
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
        drop(lock);
        let observation = read.await;
        let lock = match self.usage_lock(provider).await {
            Ok(lock) => lock,
            Err(reason) => return self.usage_unavailable(reason),
        };
        match self.usage_snapshot(provider).await {
            Ok(after) if before == after && lock.verify().is_ok() => observation,
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
