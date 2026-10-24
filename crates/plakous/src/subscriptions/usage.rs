//! Subscription usage reads.

use super::{
    NativeAuthStatus, NativeProvider, NativeSubscriptionError, NativeSubscriptionStatus,
    NativeSubscriptions, SubscriptionUsageCredentialState, SubscriptionUsageRead,
    SubscriptionUsageUnavailable, Timestamp,
};

fn is_usage_rejection(read: &SubscriptionUsageRead) -> bool {
    matches!(
        read,
        SubscriptionUsageRead::Unavailable {
            reason: SubscriptionUsageUnavailable::Unauthorized,
            ..
        }
    )
}

impl NativeSubscriptions {
    /// Performs one explicit, observational subscription allowance read.
    ///
    /// Codex, Grok, and Claude each use their reviewed provider-owned
    /// control-plane transport. No inferred request or inference dispatch
    /// occurs.
    pub async fn subscription_usage(&self, provider: NativeProvider) -> SubscriptionUsageRead {
        let unavailable = |reason| SubscriptionUsageRead::Unavailable {
            observed_at: Timestamp::new(self.state.now().unwrap_or(0), 0)
                .unwrap_or_else(|_| unreachable!("Unix epoch is a valid timestamp")),
            reason,
        };
        // The compatibility lock is an `flock` whose parent-directory guard is
        // shared by every provider, so a concurrent dispatch validation, another
        // Plak process, or a sibling provider's read makes this non-blocking
        // attempt miss. That is contention, never a transport failure.
        let lock = match self.try_acquire_compatibility_lock(provider).await {
            Ok(Some(lock)) => lock,
            Ok(None) => return unavailable(SubscriptionUsageUnavailable::Busy),
            Err(_) => return unavailable(SubscriptionUsageUnavailable::LocalState),
        };
        // Opening the provider manager performs no network I/O: every failure
        // here is local state, and a fenced credential is named by its exact
        // lifecycle rather than reported as absent or as a transport fault.
        let observation = match provider {
            NativeProvider::Codex => match self.codex_online().await {
                Ok(online) => online.manager.subscription_usage().await,
                Err(NativeSubscriptionError::CredentialUnavailable) => {
                    return unavailable(self.fenced_credential_reason(provider).await);
                }
                // A record this build refuses needs a new login.
                Err(NativeSubscriptionError::CredentialProfileChanged) => {
                    return unavailable(SubscriptionUsageUnavailable::CredentialState(
                        SubscriptionUsageCredentialState::LoginRequired,
                    ));
                }
                Err(NativeSubscriptionError::CredentialWrittenByNewerBuild) => {
                    return unavailable(SubscriptionUsageUnavailable::CredentialState(
                        SubscriptionUsageCredentialState::WrittenByNewerBuild,
                    ));
                }
                Err(_) => return unavailable(SubscriptionUsageUnavailable::LocalState),
            },
            NativeProvider::Xai => match self.xai_online().await {
                Ok(online) => online.manager.subscription_usage().await,
                Err(NativeSubscriptionError::CredentialUnavailable) => {
                    return unavailable(self.fenced_credential_reason(provider).await);
                }
                // A record this build refuses needs a new login.
                Err(NativeSubscriptionError::CredentialProfileChanged) => {
                    return unavailable(SubscriptionUsageUnavailable::CredentialState(
                        SubscriptionUsageCredentialState::LoginRequired,
                    ));
                }
                Err(NativeSubscriptionError::CredentialWrittenByNewerBuild) => {
                    return unavailable(SubscriptionUsageUnavailable::CredentialState(
                        SubscriptionUsageCredentialState::WrittenByNewerBuild,
                    ));
                }
                Err(_) => return unavailable(SubscriptionUsageUnavailable::LocalState),
            },
            NativeProvider::Claude => match self.claude_online().await {
                Ok(online) => online.manager.subscription_usage().await,
                Err(NativeSubscriptionError::CredentialUnavailable) => {
                    return unavailable(self.fenced_credential_reason(provider).await);
                }
                // A record this build refuses needs a new login.
                Err(NativeSubscriptionError::CredentialProfileChanged) => {
                    return unavailable(SubscriptionUsageUnavailable::CredentialState(
                        SubscriptionUsageCredentialState::LoginRequired,
                    ));
                }
                Err(NativeSubscriptionError::CredentialWrittenByNewerBuild) => {
                    return unavailable(SubscriptionUsageUnavailable::CredentialState(
                        SubscriptionUsageCredentialState::WrittenByNewerBuild,
                    ));
                }
                Err(_) => return unavailable(SubscriptionUsageUnavailable::LocalState),
            },
        };
        let observation = if is_usage_rejection(&observation) {
            self.recover_rejected_usage(provider, observation).await
        } else {
            observation
        };
        if lock.verify().is_err() {
            return unavailable(SubscriptionUsageUnavailable::AccountChanged);
        }
        observation
    }

    /// A usage endpoint that rejected the access token gets one forced
    /// refresh grant through the same claimed single-flight ladder model
    /// dispatch uses, then exactly one more read. A failed grant reports the
    /// credential's resulting state; the refresh failure itself is in
    /// telemetry for the display layer to name.
    async fn recover_rejected_usage(
        &self,
        provider: NativeProvider,
        rejected: SubscriptionUsageRead,
    ) -> SubscriptionUsageRead {
        match self.refresh_locked(provider, true).await {
            Ok(NativeAuthStatus::Ready) => self.read_usage_once(provider).await.unwrap_or(rejected),
            Ok(_) | Err(_) => match self.fenced_credential_reason(provider).await {
                SubscriptionUsageUnavailable::LocalState => rejected,
                reason => SubscriptionUsageRead::Unavailable {
                    observed_at: Timestamp::new(self.state.now().unwrap_or(0), 0)
                        .unwrap_or_else(|_| unreachable!("Unix epoch is a valid timestamp")),
                    reason,
                },
            },
        }
    }

    async fn read_usage_once(&self, provider: NativeProvider) -> Option<SubscriptionUsageRead> {
        Some(match provider {
            NativeProvider::Codex => {
                self.codex_online()
                    .await
                    .ok()?
                    .manager
                    .subscription_usage()
                    .await
            }
            NativeProvider::Xai => {
                self.xai_online()
                    .await
                    .ok()?
                    .manager
                    .subscription_usage()
                    .await
            }
            NativeProvider::Claude => {
                self.claude_online()
                    .await
                    .ok()?
                    .manager
                    .subscription_usage()
                    .await
            }
        })
    }

    /// Names why the selected credential was refused before any network I/O:
    /// absent is `NotConnected`; a present record is its exact lifecycle; a
    /// Ready record fenced by armed login recovery is local state.
    async fn fenced_credential_reason(
        &self,
        provider: NativeProvider,
    ) -> SubscriptionUsageUnavailable {
        match self
            .status_unlocked(provider)
            .await
            .map(NativeSubscriptionStatus::credential)
        {
            Ok(NativeAuthStatus::SignedOut) => SubscriptionUsageUnavailable::NotConnected,
            Ok(NativeAuthStatus::RefreshRequired) => SubscriptionUsageUnavailable::CredentialState(
                SubscriptionUsageCredentialState::RefreshRequired,
            ),
            Ok(NativeAuthStatus::Quarantined) => SubscriptionUsageUnavailable::CredentialState(
                SubscriptionUsageCredentialState::Quarantined,
            ),
            Ok(NativeAuthStatus::Refreshing) => SubscriptionUsageUnavailable::Busy,
            Ok(NativeAuthStatus::Revoked) => SubscriptionUsageUnavailable::CredentialState(
                SubscriptionUsageCredentialState::Revoked,
            ),
            Ok(NativeAuthStatus::LoginRequired) => SubscriptionUsageUnavailable::CredentialState(
                SubscriptionUsageCredentialState::LoginRequired,
            ),
            Ok(NativeAuthStatus::WrittenByNewerBuild) => {
                SubscriptionUsageUnavailable::CredentialState(
                    SubscriptionUsageCredentialState::WrittenByNewerBuild,
                )
            }
            Ok(NativeAuthStatus::Ready) | Err(_) => SubscriptionUsageUnavailable::LocalState,
        }
    }
}
