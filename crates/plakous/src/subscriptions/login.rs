//! Native login: instructions, compatibility consent and completion.

use super::{
    Arc, Duration, Future, Instant, LoginRecoveryOperation, NativeAuthStatus, NativeProvider,
    NativeSubscriptionError, NativeSubscriptions, SubscriptionState, fmt, map_claude_error,
    map_codex_error, map_codex_status, map_credential_status, map_xai_error, run_blocking,
    unix_now,
};

/// Supported native login interaction modes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeLoginMode {
    /// Authorization-code-with-PKCE through a fixed loopback callback.
    Browser,
    /// The browser runs on another machine: Codex uses the device-code flow;
    /// xAI and Claude complete with the code their hosted page displays.
    Remote,
}

/// How one displayed native-login instruction is completed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeLoginCompletion {
    /// The provider loopback callback completes without more input.
    Automatic,
    /// The operator enters the displayed one-time code at the verification
    /// URL; completion polls the provider without more local input.
    DeviceCode,
    /// The operator may paste the displayed authorization code or final URL.
    AuthorizationCodeOrRedirectUrl,
}

/// Display-only instructions for one pending native login.
///
/// The authorization URL is deliberately accessible because the operator must
/// act on it. Debug output remains redacted.
pub struct NativeLoginInstructions<'a> {
    verification_uri: &'a str,
    completion: NativeLoginCompletion,
    device_code: Option<NativeDeviceCode<'a>>,
}

/// The one-time code of a pending device-code login.
///
/// The code is short-lived and shown only to the operator who started the
/// login. Debug output is redacted.
#[derive(Clone, Copy)]
pub struct NativeDeviceCode<'a> {
    user_code: &'a str,
    deadline: Instant,
}

impl NativeDeviceCode<'_> {
    /// Returns the code the operator enters at the verification URL.
    #[must_use]
    pub const fn user_code(&self) -> &str {
        self.user_code
    }

    /// Returns how long the code stays valid from now.
    #[must_use]
    pub fn expires_in(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
}

impl fmt::Debug for NativeDeviceCode<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NativeDeviceCode(<redacted one-time code>)")
    }
}

impl NativeLoginInstructions<'_> {
    /// Returns the exact provider URL the operator should open.
    #[must_use]
    pub const fn verification_uri(&self) -> &str {
        self.verification_uri
    }

    /// Returns whether completion is automatic, polls a device code, or needs
    /// pasted input.
    #[must_use]
    pub const fn completion(&self) -> NativeLoginCompletion {
        self.completion
    }

    /// Returns the one-time code when completion is
    /// [`NativeLoginCompletion::DeviceCode`].
    #[must_use]
    pub const fn device_code(&self) -> Option<NativeDeviceCode<'_>> {
        self.device_code
    }
}

impl fmt::Debug for NativeLoginInstructions<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NativeLoginInstructions(<redacted operator instructions>)")
    }
}

/// Immutable, credential-free facts requiring explicit compatibility consent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompatibilityNotice {
    pub(super) provider: NativeProvider,
    pub(super) profile_id: &'static str,
    pub(super) source_revision: &'static str,
    pub(super) secondary_source_revision: Option<&'static str>,
    pub(super) notice_revision: &'static str,
    pub(super) provenance: &'static str,
    pub(super) reviewed_at: u64,
    pub(super) expires_at: u64,
    pub(super) kill_switch_revision: &'static str,
    pub(super) killed: bool,
}

impl CompatibilityNotice {
    /// Returns the integration this notice governs.
    #[must_use]
    pub const fn provider(self) -> NativeProvider {
        self.provider
    }

    /// Returns the exact compatibility profile identity.
    #[must_use]
    pub const fn profile_id(self) -> &'static str {
        self.profile_id
    }

    /// Returns the immutable reviewed source revision.
    #[must_use]
    pub const fn source_revision(self) -> &'static str {
        self.source_revision
    }

    /// Returns an additional immutable official source revision when the
    /// integration joins two independently reviewed sources.
    #[must_use]
    pub const fn secondary_source_revision(self) -> Option<&'static str> {
        self.secondary_source_revision
    }

    /// Returns the exact text the operator must enter to consent.
    #[must_use]
    pub const fn notice_revision(self) -> &'static str {
        self.notice_revision
    }

    /// Returns the compatibility provenance label.
    #[must_use]
    pub const fn provenance(self) -> &'static str {
        self.provenance
    }

    /// Returns the Unix timestamp of the compatibility review.
    #[must_use]
    pub const fn reviewed_at(self) -> u64 {
        self.reviewed_at
    }

    /// Returns the hard Unix expiry of this compatibility profile.
    #[must_use]
    pub const fn expires_at(self) -> u64 {
        self.expires_at
    }

    /// Returns the local kill-switch revision.
    #[must_use]
    pub const fn kill_switch_revision(self) -> &'static str {
        self.kill_switch_revision
    }

    /// Reports whether this build has disabled the profile.
    #[must_use]
    pub const fn is_killed(self) -> bool {
        self.killed
    }
}

/// One non-cloneable native login operation bound to its exact manager.
pub struct PendingNativeLogin {
    inner: PendingNativeLoginKind,
    consent: PendingCompatibilityConsent,
    replaced_unusable: bool,
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

enum PendingNativeLoginKind {
    CodexLocal {
        manager: Arc<plakous_openai::CodexSubscriptionCredentialManager>,
        login: plakous_openai::CodexBrowserLogin,
    },
    CodexDevice {
        manager: Arc<plakous_openai::CodexSubscriptionCredentialManager>,
        login: plakous_openai::CodexDeviceLogin,
    },
    XaiRemote(plakous_xai::XaiRemoteBrowserLogin),
    Claude(plakous_anthropic::ClaudeOAuthLogin),
}

impl PendingNativeLogin {
    /// Whether opening this login retired a stored record this build could not
    /// use (a retired payload format or another profile). Hosts say so.
    #[must_use]
    pub const fn replaced_unusable_record(&self) -> bool {
        self.replaced_unusable
    }

    /// Returns the exact authorization URL intended for display.
    #[must_use]
    pub fn instructions(&self) -> NativeLoginInstructions<'_> {
        match &self.inner {
            PendingNativeLoginKind::CodexLocal { login, .. } => NativeLoginInstructions {
                verification_uri: login.authorization_url(),
                completion: NativeLoginCompletion::Automatic,
                device_code: None,
            },
            PendingNativeLoginKind::CodexDevice { login, .. } => NativeLoginInstructions {
                verification_uri: login.verification_url(),
                completion: NativeLoginCompletion::DeviceCode,
                device_code: Some(NativeDeviceCode {
                    user_code: login.user_code(),
                    deadline: login.deadline(),
                }),
            },
            PendingNativeLoginKind::XaiRemote(login) => NativeLoginInstructions {
                verification_uri: login.authorization_url(),
                completion: NativeLoginCompletion::AuthorizationCodeOrRedirectUrl,
                device_code: None,
            },
            PendingNativeLoginKind::Claude(login) => NativeLoginInstructions {
                verification_uri: login.authorization_url(),
                completion: NativeLoginCompletion::AuthorizationCodeOrRedirectUrl,
                device_code: None,
            },
        }
    }

    /// Consumes the pending operation, validates the provider identity, and
    /// commits a Ready selected credential. A device-code login first waits
    /// for the operator's approval without touching local state, so dropping
    /// the future while it waits leaves no credential and no login recovery.
    ///
    /// The exact local compatibility-acceptance bytes captured when login
    /// began are revalidated immediately before provider completion and again
    /// before `Ready` is reported. Provider protocols cannot revoke an OAuth
    /// exchange already in flight when local consent changes mid-flow, so a
    /// post-completion local change fails closed without claiming readiness.
    ///
    /// # Errors
    ///
    /// Returns a redacted authentication, transport, profile, or storage error.
    pub async fn complete(self) -> Result<NativeAuthStatus, NativeSubscriptionError> {
        let Self { inner, consent, .. } = self;
        match inner {
            PendingNativeLoginKind::CodexLocal { manager, login } => {
                complete_pending_with_consent(&consent, unix_now, || async move {
                    manager
                        .complete_browser_login(login)
                        .await
                        .map(map_codex_status)
                        .map_err(map_codex_error)
                })
                .await
            }
            PendingNativeLoginKind::CodexDevice { manager, login } => {
                let poller = Arc::clone(&manager);
                Box::pin(complete_approved_with_consent(
                    &consent,
                    unix_now,
                    async move {
                        poller
                            .poll_device_authorization(login)
                            .await
                            .map_err(map_codex_error)
                    },
                    |authorization| async move {
                        manager
                            .complete_device_authorization(authorization)
                            .await
                            .map(map_codex_status)
                            .map_err(map_codex_error)
                    },
                ))
                .await
            }
            PendingNativeLoginKind::XaiRemote(_) | PendingNativeLoginKind::Claude(_) => {
                Err(NativeSubscriptionError::Internal)
            }
        }
    }

    /// Completes one paste-complete browser login from the provider's result.
    ///
    /// xAI (local and remote) accepts the displayed Grok Build authorization code or exact
    /// callback URL — xAI's hosted page does not complete via loopback.
    /// Claude accepts the HTTPS callback URL, `code=` query, `code#state`, or
    /// a bare authorization code. Attempt-owned PKCE is retained.
    ///
    /// # Errors
    ///
    /// Returns a redacted authentication, transport, profile, or storage error.
    pub async fn complete_remote(
        self,
        authorization_input: &str,
    ) -> Result<NativeAuthStatus, NativeSubscriptionError> {
        let Self { inner, consent, .. } = self;
        complete_pending_with_consent(&consent, unix_now, || async move {
            match inner {
                PendingNativeLoginKind::XaiRemote(login) => login
                    .complete(authorization_input)
                    .await
                    .map(|status| map_credential_status(status.status()))
                    .map_err(map_xai_error),
                PendingNativeLoginKind::Claude(login) => login
                    .complete(authorization_input)
                    .await
                    .map(|status| map_credential_status(status.status()))
                    .map_err(map_claude_error),
                PendingNativeLoginKind::CodexLocal { .. }
                | PendingNativeLoginKind::CodexDevice { .. } => {
                    Err(NativeSubscriptionError::Internal)
                }
            }
        })
        .await
    }
}

/// Waits for an out-of-band approval before any local state changes, then
/// completes it under the same consent, lock and recovery rules as every login.
pub(super) async fn complete_approved_with_consent<A, T, F, Fut, C>(
    consent: &PendingCompatibilityConsent,
    now: C,
    approval: A,
    operation: F,
) -> Result<NativeAuthStatus, NativeSubscriptionError>
where
    A: Future<Output = Result<T, NativeSubscriptionError>>,
    F: FnOnce(T) -> Fut,
    Fut: Future<Output = Result<NativeAuthStatus, NativeSubscriptionError>>,
    C: FnMut() -> Result<u64, NativeSubscriptionError>,
{
    let approved = approval.await?;
    complete_pending_with_consent(consent, now, || operation(approved)).await
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
        // Login must open even when a prior credential is quarantined / recovery
        // is armed — the operator's explicit login is how they heal that state.
        // Retire only a quarantined slot or a record this build cannot use (a
        // retired payload format or another profile); the latter is announced
        // through `replaced_unusable_record`. A healthy credential remains in
        // place until the new login commits.
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
        let pending = match provider {
            NativeProvider::Codex => {
                let online = self.codex_online_for_login().await?;
                let consent = PendingCompatibilityConsent {
                    state: self.state.clone(),
                    notice: Self::notice(provider),
                    exact_receipt: online.acceptance.exact_bytes.clone(),
                    generation: online.acceptance.generation.clone(),
                };
                let manager = online.manager;
                let inner = match mode {
                    NativeLoginMode::Browser => PendingNativeLoginKind::CodexLocal {
                        login: manager
                            .begin_browser_login()
                            .await
                            .map_err(map_codex_error)?,
                        manager,
                    },
                    NativeLoginMode::Remote => PendingNativeLoginKind::CodexDevice {
                        login: manager
                            .begin_device_login()
                            .await
                            .map_err(map_codex_error)?,
                        manager,
                    },
                };
                Ok(PendingNativeLogin {
                    inner,
                    consent,
                    replaced_unusable,
                })
            }
            NativeProvider::Xai => {
                let online = self.xai_online_for_login().await?;
                let consent = PendingCompatibilityConsent {
                    state: self.state.clone(),
                    notice: Self::notice(provider),
                    exact_receipt: online.acceptance.exact_bytes.clone(),
                    generation: online.acceptance.generation.clone(),
                };
                // xAI's hosted page always shows a code to paste into Grok Build,
                // local or remote. Do not bind or wait on loopback.
                let inner = match mode {
                    NativeLoginMode::Browser | NativeLoginMode::Remote => {
                        PendingNativeLoginKind::XaiRemote(
                            online
                                .manager
                                .begin_remote_browser_login()
                                .map_err(map_xai_error)?,
                        )
                    }
                };
                Ok(PendingNativeLogin {
                    inner,
                    consent,
                    replaced_unusable,
                })
            }
            NativeProvider::Claude => {
                let online = self.claude_online_for_login().await?;
                let consent = PendingCompatibilityConsent {
                    state: self.state.clone(),
                    notice: Self::notice(provider),
                    exact_receipt: online.acceptance.exact_bytes.clone(),
                    generation: online.acceptance.generation.clone(),
                };
                Ok(PendingNativeLogin {
                    inner: PendingNativeLoginKind::Claude(
                        online.manager.begin_login().map_err(map_claude_error)?,
                    ),
                    consent,
                    replaced_unusable,
                })
            }
        }?;
        lock.verify()?;
        Ok(pending)
    }
}
