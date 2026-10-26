//! Subscription usage reads.

use super::{
    Future, NativeAuthStatus, NativeProvider, NativeSubscriptionError, NativeSubscriptionStatus,
    NativeSubscriptions, PrivateFilesystemLock, SubscriptionUsageCredentialState,
    SubscriptionUsageRead, SubscriptionUsageUnavailable, Timestamp, run_blocking,
};

enum UsageReader {
    Codex(std::sync::Arc<plakous_openai::CodexSubscriptionCredentialManager>),
    Xai(std::sync::Arc<plakous_xai::XaiNativeAuthManager>),
    Claude(std::sync::Arc<plakous_anthropic::ClaudeNativeAuthManager>),
}

impl UsageReader {
    async fn read(self) -> SubscriptionUsageRead {
        match self {
            Self::Codex(manager) => Box::pin(manager.subscription_usage()).await,
            Self::Xai(manager) => Box::pin(manager.subscription_usage()).await,
            Self::Claude(manager) => Box::pin(manager.subscription_usage()).await,
        }
    }
}

impl NativeSubscriptions {
    /// Performs one observational subscription allowance read, with one claimed
    /// refresh and retry if the endpoint rejects the access token. Provider
    /// credential leases protect the network read; the compatibility guard is
    /// held only while preparing and checking its local consent snapshot.
    pub async fn subscription_usage(&self, provider: NativeProvider) -> SubscriptionUsageRead {
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
        // Blocking acquisition is confined to local snapshot work. Sibling
        // providers queue briefly rather than misreporting one another as Busy.
        let Ok(lock) = self.acquire_compatibility_lock(provider).await else {
            return self.usage_unavailable(SubscriptionUsageUnavailable::LocalState);
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
                    _ => SubscriptionUsageUnavailable::LocalState,
                };
                return self.usage_unavailable(reason);
            }
        };
        Box::pin(self.observe_usage_unlocked(provider, lock, reader.read())).await
    }

    async fn usage_reader(
        &self,
        provider: NativeProvider,
    ) -> Result<UsageReader, NativeSubscriptionError> {
        Ok(match provider {
            NativeProvider::Codex => UsageReader::Codex(self.codex_online().await?.manager),
            NativeProvider::Xai => UsageReader::Xai(self.xai_online().await?.manager),
            NativeProvider::Claude => UsageReader::Claude(self.claude_online().await?.manager),
        })
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
        let Ok(lock) = self.acquire_compatibility_lock(provider).await else {
            return self.usage_unavailable(SubscriptionUsageUnavailable::LocalState);
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
