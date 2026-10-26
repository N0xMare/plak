//! Native login: instructions, compatibility consent and completion.
use super::NativeLoginMode;

use super::{
    CompatibilityNotice, Future, LoginRecoveryOperation, NativeAuthStatus, NativeLoginInstructions,
    NativeProvider, NativeSubscriptionError, NativeSubscriptions, PrivateFilesystemLock,
    SubscriptionState, fmt, run_blocking,
};

/// One non-cloneable native login operation bound to its exact manager.
pub struct PendingNativeLogin {
    pub(super) inner: Box<dyn NativePendingLogin>,
    pub(super) consent: PendingCompatibilityConsent,
    pub(super) replaced_unusable: bool,
}

#[derive(Clone)]
pub(super) struct PendingCompatibilityConsent {
    pub(super) state: SubscriptionState,
    pub(super) notice: CompatibilityNotice,
    pub(super) exact_receipt: Vec<u8>,
    pub(super) generation: String,
}

impl PendingCompatibilityConsent {
    pub(super) async fn validate_at(&self, now: u64) -> Result<(), NativeSubscriptionError> {
        let paths = self.state.clone();
        let notice = self.notice;
        let expected = self.exact_receipt.clone();
        let expected_generation = self.generation.clone();
        run_blocking(move || {
            let current = paths.load_acceptance_exact(notice, now)?;
            if current.exact_bytes != expected || current.generation != expected_generation {
                return Err(NativeSubscriptionError::AcceptanceRequired);
            }
            Ok(())
        })
        .await?
    }

    async fn revoke_local_after_change(&self) -> Result<(), NativeSubscriptionError> {
        let status = NativeSubscriptions::from_state(self.state.clone())
            .logout_local_unlocked(self.notice.provider())
            .await?;
        if status != NativeAuthStatus::Revoked {
            return Err(NativeSubscriptionError::CredentialUnavailable);
        }
        Ok(())
    }

    fn arm_recovery(&self) -> Result<LoginRecoveryOperation, NativeSubscriptionError> {
        self.state
            .arm_login_recovery(self.notice.provider(), &self.generation)
    }

    fn clear_recovery(
        &self,
        operation: &LoginRecoveryOperation,
        credential_generation: u64,
    ) -> Result<(), NativeSubscriptionError> {
        self.state
            .clear_login_recovery(self.notice.provider(), operation, credential_generation)
    }

    fn restore_recovery(
        &self,
        operation: &LoginRecoveryOperation,
    ) -> Result<(), NativeSubscriptionError> {
        self.state
            .restore_login_recovery(self.notice.provider(), operation)
    }
}

pub(super) trait NativePendingLogin: Send {
    fn instructions(&self) -> NativeLoginInstructions<'_>;
    fn complete(
        self: Box<Self>,
        consent: PendingCompatibilityConsent,
    ) -> super::vertical::NativeFuture<'static, Result<NativeAuthStatus, NativeSubscriptionError>>;
    fn complete_remote(
        self: Box<Self>,
        consent: PendingCompatibilityConsent,
        authorization_input: &str,
    ) -> super::vertical::NativeFuture<'_, Result<NativeAuthStatus, NativeSubscriptionError>>;
}

impl PendingNativeLogin {
    /// Whether opening this login retired an unusable stored record.
    #[must_use]
    pub const fn replaced_unusable_record(&self) -> bool {
        self.replaced_unusable
    }

    /// Returns the exact instructions intended for the operator.
    #[must_use]
    pub fn instructions(&self) -> NativeLoginInstructions<'_> {
        self.inner.instructions()
    }

    /// Completes the admitted automatic or device login under its exact consent.
    ///
    /// # Errors
    /// Returns the provider's redacted authentication, profile or storage error.
    pub async fn complete(self) -> Result<NativeAuthStatus, NativeSubscriptionError> {
        self.inner.complete(self.consent).await
    }

    /// Completes the admitted paste-complete browser login under its exact consent.
    ///
    /// # Errors
    /// Returns the provider's redacted authentication, profile or storage error.
    pub async fn complete_remote(
        self,
        authorization_input: &str,
    ) -> Result<NativeAuthStatus, NativeSubscriptionError> {
        self.inner
            .complete_remote(self.consent, authorization_input)
            .await
    }
}

pub(super) async fn complete_pending_with_consent<F, Fut, C>(
    consent: &PendingCompatibilityConsent,
    mut now: C,
    operation: F,
) -> Result<NativeAuthStatus, NativeSubscriptionError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<NativeAuthStatus, NativeSubscriptionError>>,
    C: FnMut() -> Result<u64, NativeSubscriptionError>,
{
    let subscriptions = NativeSubscriptions::from_state(consent.state.clone());
    let lock = subscriptions
        .acquire_compatibility_lock(consent.notice.provider())
        .await?;
    consent.validate_at(now()?).await?;
    lock.verify()?;
    let mut armed = ArmedLoginRecovery {
        consent,
        operation: Some(consent.arm_recovery()?),
    };
    if let Err(error) = consent.validate_at(now()?).await {
        lock.verify()?;
        consent.restore_recovery(&armed.settle())?;
        return Err(error);
    }
    lock.verify()?;
    let result = operation().await;
    // From here every exit settles the marker explicitly; only a future
    // dropped while the provider exchange was pending reaches the guard.
    let recovery = armed.settle();
    let status = match result {
        Ok(status) => status,
        Err(error) => {
            lock.verify()?;
            if consent
                .state
                .current_credential_generation(consent.notice.provider())?
                == recovery.credential_generation_before
            {
                consent.restore_recovery(&recovery)?;
                lock.verify()?;
            }
            return Err(error);
        }
    };
    lock.verify()?;
    if let Err(error) = consent.validate_at(now()?).await {
        consent.revoke_local_after_change().await?;
        lock.verify()?;
        consent
            .state
            .retire_login_recovery(consent.notice.provider())?;
        return Err(error);
    }
    if status != NativeAuthStatus::Ready {
        return Err(NativeSubscriptionError::CredentialUnavailable);
    }
    lock.verify()?;
    let generation = consent
        .state
        .current_credential_generation(consent.notice.provider())?
        .ok_or(NativeSubscriptionError::CredentialUnavailable)?;
    consent.clear_recovery(&recovery, generation)?;
    lock.verify()?;
    Ok(NativeAuthStatus::Ready)
}

/// Login recovery armed for one in-flight completion.
///
/// A completion future dropped before its provider exchange settles (Ctrl-C
/// or a timeout while the browser callback is pending) restores the marker
/// exactly as the error path does: only while the credential generation is
/// still the one seen at arming, so a credential the exchange may already
/// have committed keeps its recovery marker. A killed process never runs
/// this, so crash recovery still sees the armed marker.
struct ArmedLoginRecovery<'a> {
    consent: &'a PendingCompatibilityConsent,
    pub(super) operation: Option<LoginRecoveryOperation>,
}

impl ArmedLoginRecovery<'_> {
    fn settle(&mut self) -> LoginRecoveryOperation {
        self.operation
            .take()
            .unwrap_or_else(|| unreachable!("login recovery is settled exactly once"))
    }
}

impl Drop for ArmedLoginRecovery<'_> {
    fn drop(&mut self) {
        let Some(operation) = self.operation.take() else {
            return;
        };
        if self
            .consent
            .state
            .current_credential_generation(self.consent.notice.provider())
            .is_ok_and(|current| current == operation.credential_generation_before)
        {
            let _ = self.consent.restore_recovery(&operation);
        }
    }
}

impl fmt::Debug for PendingNativeLogin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PendingNativeLogin(<redacted non-cloneable login>)")
    }
}

impl NativeSubscriptions {
    /// Starts one exact local or remote login after compatibility admission.
    ///
    /// # Errors
    ///
    /// Rejects missing consent, unsafe storage, unavailable profile, local
    /// binding failure, or provider transport failure.
    pub async fn begin_login(
        &self,
        provider: NativeProvider,
        mode: NativeLoginMode,
    ) -> Result<PendingNativeLogin, NativeSubscriptionError> {
        let lock = self.acquire_compatibility_lock(provider).await?;
        let replaced_unusable = self.retire_unusable_login(provider, &lock).await?;
        let pending = super::vertical::require_vertical(provider)?
            .begin_login(self, mode, replaced_unusable)
            .await?;
        lock.verify()?;
        Ok(pending)
    }

    /// Login must open even when a prior credential is quarantined / recovery is
    /// armed — the operator's explicit login is how they heal that state. Retire
    /// only a quarantined slot or a record this build cannot use (a retired
    /// payload format or another profile); the latter is announced through
    /// `replaced_unusable_record`. A healthy credential remains in place until
    /// the new login commits. Returns whether an unusable record was replaced.
    async fn retire_unusable_login(
        &self,
        provider: NativeProvider,
        lock: &PrivateFilesystemLock,
    ) -> Result<bool, NativeSubscriptionError> {
        let retire = match self.status_unlocked(provider).await {
            Ok(status) => match status.credential() {
                // Only the operator's confirmed replacement may retire it.
                NativeAuthStatus::WrittenByNewerBuild => {
                    return Err(NativeSubscriptionError::CredentialWrittenByNewerBuild);
                }
                NativeAuthStatus::Quarantined => Some(false),
                NativeAuthStatus::LoginRequired => Some(true),
                _ => None,
            },
            Err(NativeSubscriptionError::CredentialProfileChanged) => Some(true),
            Err(error) => return Err(error),
        };
        let replaced_unusable = retire == Some(true);
        if retire.is_some() {
            let status = self.logout_local_unlocked(provider).await?;
            if !matches!(
                status,
                NativeAuthStatus::Revoked | NativeAuthStatus::SignedOut
            ) {
                return Err(NativeSubscriptionError::CredentialUnavailable);
            }
            let paths = self.state.clone();
            run_blocking(move || paths.retire_login_recovery(provider)).await??;
            lock.verify()?;
        }
        Ok(replaced_unusable)
    }
}
