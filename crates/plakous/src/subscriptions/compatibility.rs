//! Compatibility acceptance and local credential status.

use super::{
    CompatibilityNotice, CompatibilityStatus, NativeAuthStatus, NativeProvider,
    NativeSubscriptionError, NativeSubscriptionStatus, NativeSubscriptions, PRIMARY_SLOT,
    PrivateFilesystemLock, SubscriptionState, map_claude_error, map_claude_offline_status,
    map_codex_error, map_codex_status, map_xai_error, map_xai_offline_status, run_blocking,
};

impl NativeSubscriptions {
    pub(super) async fn acquire_compatibility_lock(
        &self,
        provider: NativeProvider,
    ) -> Result<PrivateFilesystemLock, NativeSubscriptionError> {
        self.state.acquire_compatibility_lock(provider).await
    }

    #[cfg(test)]
    pub(super) async fn try_acquire_compatibility_lock(
        &self,
        provider: NativeProvider,
    ) -> Result<Option<PrivateFilesystemLock>, NativeSubscriptionError> {
        self.state.try_acquire_compatibility_lock(provider).await
    }

    /// Returns every immutable fact requiring explicit operator consent.
    #[must_use]
    pub fn notice(provider: NativeProvider) -> CompatibilityNotice {
        SubscriptionState::notice(provider)
    }

    /// Persists explicit consent only when the operator supplied the exact
    /// current notice revision.
    ///
    /// # Errors
    ///
    /// Rejects an inexact response, inactive profile, unsafe path, or failed
    /// atomic receipt write. No network operation occurs.
    pub async fn acknowledge_exact(
        &self,
        provider: NativeProvider,
        operator_response: &str,
    ) -> Result<(), NativeSubscriptionError> {
        let notice = Self::notice(provider);
        if operator_response != notice.notice_revision() {
            return Err(NativeSubscriptionError::AcceptanceRequired);
        }
        let lock = self.acquire_compatibility_lock(provider).await?;
        let paths = self.state.clone();
        run_blocking(move || {
            lock.verify()?;
            if paths.receipt_written_by_newer_build(notice)? {
                return Err(NativeSubscriptionError::CredentialWrittenByNewerBuild);
            }
            let now = paths.now()?;
            let previous = paths.stored_acceptance_generation(provider)?;
            let next = paths.store_acceptance(notice, now)?;
            if let Some(previous) = previous {
                paths.rebind_login_recovery_consent(provider, &previous, &next)?;
            }
            lock.verify()
        })
        .await?
    }

    /// Returns compatibility and credential state without provider network I/O.
    ///
    /// Absent storage is reported as signed out without creating any
    /// directory, lock, or record.
    ///
    /// # Errors
    ///
    /// Fails closed for unsafe, foreign, or malformed explicit storage.
    pub async fn status(
        &self,
        provider: NativeProvider,
    ) -> Result<NativeSubscriptionStatus, NativeSubscriptionError> {
        let paths = self.state.clone();
        if !run_blocking(move || paths.layout_exists()).await?? {
            return Ok(NativeSubscriptionStatus {
                compatibility: SubscriptionState::absent_compatibility(
                    Self::notice(provider),
                    self.state.now()?,
                ),
                credential: NativeAuthStatus::SignedOut,
            });
        }
        let lock = self.acquire_compatibility_lock(provider).await?;
        let status = self.status_unlocked(provider).await?;
        lock.verify()?;
        Ok(status)
    }

    /// Returns the compatibility acceptance state alone, without reading the
    /// credential record, so a caller can tell whether a login over a record
    /// [`Self::status`] refused as another profile needs the notice first.
    ///
    /// # Errors
    ///
    /// Fails closed for unsafe, foreign, or malformed explicit storage.
    pub async fn compatibility_status(
        &self,
        provider: NativeProvider,
    ) -> Result<CompatibilityStatus, NativeSubscriptionError> {
        let paths = self.state.clone();
        if !run_blocking(move || paths.layout_exists()).await?? {
            return Ok(SubscriptionState::absent_compatibility(
                Self::notice(provider),
                self.state.now()?,
            ));
        }
        let lock = self.acquire_compatibility_lock(provider).await?;
        let compatibility = self.compatibility_status_unlocked(provider).await?;
        lock.verify()?;
        Ok(compatibility)
    }

    pub(super) async fn status_unlocked(
        &self,
        provider: NativeProvider,
    ) -> Result<NativeSubscriptionStatus, NativeSubscriptionError> {
        let credential = self.offline_credential_unlocked(provider).await?;
        if credential == NativeAuthStatus::WrittenByNewerBuild
            || self.compatibility_written_by_newer_build(provider).await?
        {
            // Nothing below runs: the recovery gate may re-pin or quarantine.
            let compatibility = match self.compatibility_status_unlocked(provider).await {
                Err(NativeSubscriptionError::InvalidReceipt) => CompatibilityStatus::Required,
                compatibility => compatibility?,
            };
            return Ok(NativeSubscriptionStatus {
                compatibility,
                credential: NativeAuthStatus::WrittenByNewerBuild,
            });
        }
        self.status_after_credential(provider, credential).await
    }

    pub(super) async fn compatibility_written_by_newer_build(
        &self,
        provider: NativeProvider,
    ) -> Result<bool, NativeSubscriptionError> {
        let paths = self.state.clone();
        let notice = Self::notice(provider);
        run_blocking(move || paths.compatibility_written_by_newer_build(notice)).await?
    }

    /// Whether any of this provider's stored login state was written by a
    /// newer build. Reads only; never claims, leases or writes.
    pub(super) async fn written_by_newer_build_unlocked(
        &self,
        provider: NativeProvider,
    ) -> Result<bool, NativeSubscriptionError> {
        Ok(self.offline_credential_unlocked(provider).await?
            == NativeAuthStatus::WrittenByNewerBuild
            || self.compatibility_written_by_newer_build(provider).await?)
    }

    pub(super) async fn offline_credential_unlocked(
        &self,
        provider: NativeProvider,
    ) -> Result<NativeAuthStatus, NativeSubscriptionError> {
        let paths = self.state.clone();
        run_blocking(move || paths.prepare()).await??;
        Ok(match provider {
            NativeProvider::Codex => plakous_openai::CodexSubscriptionLocalAdmin::open(
                self.state.provider_record(provider),
            )
            .await
            .map_err(map_codex_error)?
            .status()
            .await
            .map(map_codex_status)
            .map_err(map_codex_error)?,
            NativeProvider::Xai => {
                let path = self.state.provider_record(provider);
                run_blocking(move || {
                    let slot = plakous_auth::CredentialSlotId::new(PRIMARY_SLOT)
                        .map_err(|_| NativeSubscriptionError::Internal)?;
                    let admin = plakous_xai::XaiOfflineSelectedSlot::open_explicit(path, slot)
                        .map_err(map_xai_error)?;
                    admin
                        .status()
                        .map(map_xai_offline_status)
                        .map_err(map_xai_error)
                })
                .await??
            }
            NativeProvider::Claude => {
                let path = self.state.provider_record(provider);
                run_blocking(move || {
                    let slot = plakous_auth::CredentialSlotId::new(PRIMARY_SLOT)
                        .map_err(|_| NativeSubscriptionError::Internal)?;
                    let admin =
                        plakous_anthropic::ClaudeOfflineSelectedSlot::open_explicit(path, slot)
                            .map_err(map_claude_error)?;
                    admin
                        .status()
                        .map(map_claude_offline_status)
                        .map_err(map_claude_error)
                })
                .await??
            }
        })
    }

    async fn status_after_credential(
        &self,
        provider: NativeProvider,
        credential: NativeAuthStatus,
    ) -> Result<NativeSubscriptionStatus, NativeSubscriptionError> {
        let notice = Self::notice(provider);
        let paths = self.state.clone();
        let consent_generation = run_blocking(move || {
            let now = paths.now()?;
            match paths.load_acceptance_exact(notice, now) {
                Ok(acceptance) => Ok(Some(acceptance.generation)),
                // A stale or inactive acceptance is reported as compatibility;
                // the login stays judged against the acceptance it was made under.
                Err(
                    NativeSubscriptionError::AcceptanceRequired
                    | NativeSubscriptionError::ProfileInactive,
                ) => paths.stored_acceptance_generation(provider),
                Err(error) => Err(error),
            }
        })
        .await??;
        let paths = self.state.clone();
        let recovery_required = run_blocking(move || {
            paths.login_recovery_required_for_status(
                provider,
                consent_generation.as_deref(),
                credential,
            )
        })
        .await??;
        let compatibility = self.compatibility_status_unlocked(provider).await?;
        Ok(NativeSubscriptionStatus {
            compatibility,
            credential: if recovery_required {
                NativeAuthStatus::Quarantined
            } else {
                credential
            },
        })
    }

    async fn compatibility_status_unlocked(
        &self,
        provider: NativeProvider,
    ) -> Result<CompatibilityStatus, NativeSubscriptionError> {
        let paths = self.state.clone();
        let notice = Self::notice(provider);
        match run_blocking(move || {
            let now = paths.now()?;
            paths.load_acceptance(notice, now)
        })
        .await?
        {
            Ok(_) => Ok(CompatibilityStatus::Current),
            Err(NativeSubscriptionError::AcceptanceRequired) => Ok(CompatibilityStatus::Required),
            Err(NativeSubscriptionError::ProfileInactive) => Ok(CompatibilityStatus::Inactive),
            Err(error) => Err(error),
        }
    }
}
