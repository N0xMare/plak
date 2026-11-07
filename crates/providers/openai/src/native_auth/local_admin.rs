//! Offline local administration of the built-in Codex slot.

use super::{
    Arc, AsyncMutex, AuthProfileId, CodexCredentialStatus, CodexSubscriptionIntegrationProfile,
    CredentialSlotId, NativeCodexAuthError, OPENAI_CODEX_COMPATIBILITY_PROFILE_ID, PathBuf,
    PrivateCredentialStore, PrivateStoreError, fmt, local_credential_status, local_logout,
    run_blocking,
};

/// Offline local administration for the exact built-in native Codex slot.
///
/// This facade owns no HTTP client, endpoint, acceptance, account identifier,
/// or credential material. It can therefore inspect credential-free state and
/// commit a local tombstone even after the online compatibility profile expires
/// or its kill switch is activated. The explicit path is accepted only at open
/// time and is never exposed again.
pub struct CodexSubscriptionLocalAdmin {
    pub(super) store: Arc<PrivateCredentialStore>,
    pub(super) lifecycle: AsyncMutex<()>,
}

impl CodexSubscriptionLocalAdmin {
    /// Opens the exact application-selected slot for the pinned
    /// native Codex profile without consulting ambient state or the network.
    ///
    /// Existing state is reloaded during open. A record written by a newer
    /// build, or under another (older) compatibility profile, opens, so
    /// [`Self::status`] can name it (`WrittenByNewerBuild` or
    /// `ProfileChanged`) and [`Self::logout_local`] can retire it. A foreign
    /// slot scope, malformed record or unsafe path fails before the facade is
    /// returned. Current compatibility acceptance, expiry, and kill-switch
    /// state are deliberately irrelevant to this local cleanup authority.
    ///
    /// # Errors
    ///
    /// Rejects relative or unsafe paths and any malformed or foreign-slot
    /// record.
    pub async fn open(
        slot_path: impl Into<PathBuf>,
        slot: CredentialSlotId,
    ) -> Result<Self, NativeCodexAuthError> {
        let slot_path = slot_path.into();
        let store = run_blocking(move || {
            let profile = AuthProfileId::new(OPENAI_CODEX_COMPATIBILITY_PROFILE_ID)
                .map_err(|_| NativeCodexAuthError::Validation)?;
            let store = Arc::new(
                PrivateCredentialStore::open_slot(&slot_path, profile, slot)
                    .map_err(|_| NativeCodexAuthError::Store)?,
            );
            // A newer build's record, or one written under another
            // compatibility profile, is opened so status can name it and
            // local logout can retire it.
            match store.load() {
                Ok(_)
                | Err(PrivateStoreError::WrittenByNewerBuild | PrivateStoreError::ForeignProfile) =>
                    {}
                Err(_) => return Err(NativeCodexAuthError::Store),
            }
            Ok(store)
        })
        .await?;
        Ok(Self {
            store,
            lifecycle: AsyncMutex::new(()),
        })
    }

    /// Returns immutable, credential-free facts for the pinned integration.
    #[must_use]
    pub const fn profile(&self) -> CodexSubscriptionIntegrationProfile {
        CodexSubscriptionIntegrationProfile
    }

    /// Reloads and returns only the credential-free lifecycle classification.
    ///
    /// # Errors
    ///
    /// Fails closed if the explicit slot is replaced, malformed, foreign, or
    /// otherwise unsafe.
    pub async fn status(&self) -> Result<CodexCredentialStatus, NativeCodexAuthError> {
        let _lifecycle = self.lifecycle.lock().await;
        let store = Arc::clone(&self.store);
        run_blocking(move || local_credential_status(&store)).await
    }

    /// Commits an offline local revoked tombstone without acceptance or network I/O.
    ///
    /// This never contacts the provider and does not claim remote revocation.
    /// It is safe to use for local cleanup after profile expiry or kill-switch
    /// activation.
    ///
    /// # Errors
    ///
    /// Fails closed if the explicit slot cannot be reloaded and changed by its
    /// exact generation under the pinned profile and slot scope.
    pub async fn logout_local(&self) -> Result<CodexCredentialStatus, NativeCodexAuthError> {
        let _lifecycle = self.lifecycle.lock().await;
        let store = Arc::clone(&self.store);
        run_blocking(move || local_logout(&store)).await
    }
}

impl fmt::Debug for CodexSubscriptionLocalAdmin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(
            "CodexSubscriptionLocalAdmin(<pinned profile; explicit slot; credential redacted>)",
        )
    }
}
