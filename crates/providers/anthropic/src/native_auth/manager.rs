//! The app-owned manager for one explicit Claude selected-account slot.

use super::custodied::{ClaudeCustodiedLogin, ClaudeOAuthAccess};
use super::refresh::RefreshForce;
use super::{
    AccountBinding, Arc, AsyncMutex, AuthBodyError, CLAUDE_OAUTH_COMPATIBILITY_PROFILE_ID,
    CLAUDE_OAUTH_TOKEN_ORIGIN, CLAUDE_SUBSCRIPTION_PROFILE_PATH, CLAUDE_SUBSCRIPTION_USAGE_PATH,
    ClaudeCompatibilityAcceptance, ClaudeEnrolledModel, ClaudeNativeAuthError, ClaudeOAuthLogin,
    ClaudeRefreshHooks, ClaudeSubscriptionStatus, Client, CredentialFence, CredentialGeneration,
    CredentialRefreshDetail, CredentialRefreshObserver, CredentialSlotId, CredentialStatus,
    LOGIN_TIMEOUT, ManagerCore, PathBuf, PkceS256, PrivateCredentialStore, PrivateStoreError,
    StoredTokens, SubscriptionRequestGate, SubscriptionUsageCredentialState, SubscriptionUsageRead,
    SubscriptionUsageUnavailable, Timestamp, attach_claude_profile, auth_profile_id,
    claude_authorize_url, credential_record, fmt, network_failure, oauth_control_headers,
    oauth_control_headers_with, run_claimed_refresh, system_unix_seconds, usage_credential_refusal,
    validate_acceptance, validate_current_ready_record,
};
#[cfg(test)]
use super::{Origin, parse_claude_oauth_token_grant};

/// App-owned manager for one explicit Claude selected-account slot.
#[derive(Clone)]
pub struct ClaudeNativeAuthManager {
    pub(super) core: Arc<ManagerCore>,
    pub(super) request_gate: Option<Arc<dyn SubscriptionRequestGate>>,
    pub(super) refresh_observer: Option<Arc<dyn CredentialRefreshObserver>>,
    source_scope: Option<String>,
    /// The custodian holding the login; `None` under local custody.
    pub(super) custodied: Option<Arc<ClaudeCustodiedLogin>>,
}

impl ClaudeNativeAuthManager {
    #[cfg(test)]
    pub(crate) fn with_test_token_origin(
        mut self,
        origin: &Origin,
    ) -> Result<Self, ClaudeNativeAuthError> {
        let address = origin
            .binding_authority()
            .parse::<std::net::SocketAddr>()
            .map_err(|_| ClaudeNativeAuthError::NotReady)?;
        if !address.ip().is_loopback() {
            return Err(ClaudeNativeAuthError::NotReady);
        }
        let core = Arc::get_mut(&mut self.core).ok_or(ClaudeNativeAuthError::NotReady)?;
        core.token_origin = format!("https://{}", origin.binding_authority());
        Ok(self)
    }

    /// Opens one explicit private selected-slot record.
    ///
    /// # Errors
    ///
    /// Fails for an unaccepted/expired profile or unsafe explicit store path.
    pub fn open_selected_slot(
        path: impl Into<PathBuf>,
        slot: CredentialSlotId,
        client: Client,
        acceptance: ClaudeCompatibilityAcceptance,
        device_id: crate::ClaudeClientDeviceId,
    ) -> Result<Self, ClaudeNativeAuthError> {
        validate_acceptance(&acceptance, system_unix_seconds()?)?;
        Ok(Self {
            core: Arc::new(ManagerCore {
                device_id,
                store: Some(PrivateCredentialStore::open_slot(
                    path,
                    auth_profile_id()?,
                    slot,
                )?),
                client,
                acceptance,
                token_origin: CLAUDE_OAUTH_TOKEN_ORIGIN.to_owned(),
                #[cfg(test)]
                control_origin: None,
                refresh_flight: AsyncMutex::new(()),
            }),
            request_gate: None,
            refresh_observer: None,
            source_scope: None,
            custodied: None,
        })
    }

    /// Opens the login a credential custodian holds for `slot`: facts, the
    /// `authorization` field and refresh are the custodian's, the client
    /// device id is the one its facts report, and no local slot is opened.
    /// `client` is the route's client (relayed through a gateway).
    ///
    /// # Errors
    ///
    /// Fails for an unaccepted or expired profile, an unreachable custodian,
    /// or a login that is not Ready with its account identity.
    pub async fn custodied(
        custodian: Arc<dyn plakous_auth::custody::CredentialCustodian>,
        slot: plakous_auth::custody::CustodySlot,
        client: Client,
        acceptance: ClaudeCompatibilityAcceptance,
    ) -> Result<Self, ClaudeNativeAuthError> {
        validate_acceptance(&acceptance, system_unix_seconds()?)?;
        let login = ClaudeCustodiedLogin::open(custodian, slot).await?;
        let ((_, _, _), identity) = login.identity_credentials()?;
        Ok(Self {
            core: Arc::new(ManagerCore {
                device_id: identity.device_id().clone(),
                store: None,
                client,
                acceptance,
                token_origin: CLAUDE_OAUTH_TOKEN_ORIGIN.to_owned(),
                #[cfg(test)]
                control_origin: None,
                refresh_flight: AsyncMutex::new(()),
            }),
            request_gate: None,
            refresh_observer: None,
            source_scope: None,
            custodied: Some(Arc::new(login)),
        })
    }

    /// Scopes this source independently of its provider-defined upstream account.
    /// Unscoped callers retain their existing credential-authority bytes.
    #[must_use]
    pub fn with_credential_source_scope(mut self, scope: String) -> Self {
        self.source_scope = Some(scope);
        self
    }

    pub(crate) fn credential_source_scope(&self) -> Option<&str> {
        self.source_scope.as_deref()
    }

    /// Attaches the classified physical-request gate required for inference.
    ///
    /// Catalog observation is classified by the caller. Leaving the gate absent
    /// keeps login/status usable; smoke and campaign dispatch attach it.
    #[must_use]
    pub fn with_subscription_request_gate(
        mut self,
        gate: Arc<dyn SubscriptionRequestGate>,
    ) -> Self {
        self.request_gate = Some(gate);
        self
    }

    /// Attaches a content-free claimed-refresh observer (product telemetry).
    #[must_use]
    pub fn with_credential_refresh_observer(
        mut self,
        observer: Arc<dyn CredentialRefreshObserver>,
    ) -> Self {
        self.refresh_observer = Some(observer);
        self
    }

    pub(crate) fn request_gate(&self) -> Option<Arc<dyn SubscriptionRequestGate>> {
        self.request_gate.clone()
    }

    /// Returns the current credential-free status.
    ///
    /// # Errors
    ///
    /// Fails closed on malformed or unsafe storage.
    pub fn status(&self) -> Result<Option<ClaudeSubscriptionStatus>, ClaudeNativeAuthError> {
        if let Some(login) = &self.custodied {
            return Ok(login.status());
        }
        validate_current_ready_record(self.core.store()?)?;
        Ok(self
            .core
            .store()?
            .load()?
            .map(|fence| ClaudeSubscriptionStatus {
                generation: fence.generation(),
                status: fence.status(),
            }))
    }

    pub(crate) async fn async_status(
        &self,
    ) -> Result<Option<ClaudeSubscriptionStatus>, ClaudeNativeAuthError> {
        if let Some(login) = &self.custodied {
            login.read().await?;
            return Ok(login.status());
        }
        let manager = self.clone();
        tokio::task::spawn_blocking(move || manager.status())
            .await
            .map_err(|_| ClaudeNativeAuthError::NotReady)?
    }

    pub(crate) async fn wait_for_ready(
        &self,
        parked: Option<ClaudeSubscriptionStatus>,
    ) -> Result<(), ClaudeNativeAuthError> {
        let mut previous = parked.clone();
        let mut delay = std::time::Duration::from_millis(250);
        loop {
            let current = self.async_status().await?;
            if current != parked
                && current
                    .as_ref()
                    .is_some_and(|status| status.status() == CredentialStatus::Ready)
            {
                return self.ensure_account_identity().await;
            }
            if current != previous {
                delay = std::time::Duration::from_millis(250);
                previous = current;
            }
            // Login is cross-process; observe the durable generation/readiness,
            // not an in-process notification that another process cannot send.
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(std::time::Duration::from_secs(2));
        }
    }

    /// Performs the Claude Code `/usage` control-plane reads.
    ///
    /// `GET /api/oauth/usage` supplies the session, all-models weekly, and
    /// Fable-scoped meters. `GET /api/oauth/profile` supplies the account
    /// e-mail Claude Code's `/status` shows. Profile failure does not fail
    /// the meters. This observation acquires no inference permit and grants
    /// no catalog, admission, retry, billing, or inference authority.
    #[allow(
        clippy::too_many_lines,
        reason = "Resolve the selected Claude receipt and retain its diagnostics while classifying the bounded usage response."
    )]
    pub async fn subscription_usage(&self) -> SubscriptionUsageRead {
        let unavailable = |reason| SubscriptionUsageRead::Unavailable {
            observed_at: Timestamp::new(system_unix_seconds().unwrap_or(0), 0)
                .unwrap_or_else(|_| unreachable!("Unix epoch is a valid timestamp")),
            reason,
        };
        if self.core.ensure_active().is_err() {
            return unavailable(SubscriptionUsageUnavailable::Unqualified);
        }
        if let Some(login) = &self.custodied {
            return self.custodied_usage(login).await;
        }
        let Ok(store) = self.core.store() else {
            return unavailable(SubscriptionUsageUnavailable::LocalState);
        };
        // Usage never starts a refresh. Claude refresh tokens rotate and a
        // claimed refresh cannot be restored once it may have reached the
        // provider, so a usage read dropped by its caller's timeout mid-claim
        // would leave a durable Unknown fence that only a new login clears.
        // An expired access token is named `RefreshRequired` below; the next
        // turn refreshes it.
        let Some(_lifecycle) = self.core.refresh_flight.try_lock() else {
            return unavailable(SubscriptionUsageUnavailable::Busy);
        };
        match validate_current_ready_record(store) {
            Ok(()) => {}
            Err(
                ClaudeNativeAuthError::StoredCredentialWrittenByNewerBuild
                | ClaudeNativeAuthError::Store(PrivateStoreError::WrittenByNewerBuild),
            ) => {
                return unavailable(SubscriptionUsageUnavailable::CredentialState(
                    SubscriptionUsageCredentialState::WrittenByNewerBuild,
                ));
            }
            Err(_) => return unavailable(SubscriptionUsageUnavailable::LocalState),
        }
        let fence = match store.load() {
            Ok(Some(fence)) => fence,
            Ok(None) => return unavailable(SubscriptionUsageUnavailable::NotConnected),
            Err(_) => return unavailable(SubscriptionUsageUnavailable::LocalState),
        };
        // A durable Unknown record whose claim another process still holds is
        // a refresh in flight, not a settled quarantine.
        if fence.status() == CredentialStatus::RefreshOutcomeUnknown {
            match store.refresh_outcome_settled(&fence) {
                Ok(true) => {}
                Ok(false) => return unavailable(SubscriptionUsageUnavailable::Busy),
                Err(_) => return unavailable(SubscriptionUsageUnavailable::LocalState),
            }
        }
        if let Some(reason) = usage_credential_refusal(fence.status()) {
            return unavailable(reason);
        }
        let lease = match store.try_acquire_ready(&fence) {
            Ok(Some(lease)) => lease,
            Ok(None) => return unavailable(SubscriptionUsageUnavailable::Busy),
            Err(_) => return unavailable(SubscriptionUsageUnavailable::LocalState),
        };
        let stored: StoredTokens = match serde_json::from_slice(lease.payload().expose_secret()) {
            Ok(stored) => stored,
            Err(_) => return unavailable(SubscriptionUsageUnavailable::InvalidResponse),
        };
        if stored.validate().is_err() {
            return unavailable(SubscriptionUsageUnavailable::InvalidResponse);
        }
        let Ok(now) = system_unix_seconds() else {
            return unavailable(SubscriptionUsageUnavailable::LocalState);
        };
        if stored.expires_at <= now {
            return unavailable(SubscriptionUsageUnavailable::CredentialState(
                SubscriptionUsageCredentialState::RefreshRequired,
            ));
        }
        if lease.fence() != fence || stored.grant_binding != fence.account_binding().as_str() {
            return unavailable(SubscriptionUsageUnavailable::AccountChanged);
        }

        let headers = match oauth_control_headers(&stored) {
            Ok(headers) => headers,
            Err(reason) => return unavailable(reason),
        };
        let Ok(usage_request) = self
            .core
            .oauth_control_get(CLAUDE_SUBSCRIPTION_USAGE_PATH, headers.clone())
        else {
            return unavailable(SubscriptionUsageUnavailable::LocalState);
        };
        let profile_request = self
            .core
            .oauth_control_get(CLAUDE_SUBSCRIPTION_PROFILE_PATH, headers)
            .ok();
        let (usage_reply, profile_reply) =
            tokio::join!(self.core.execute_usage(usage_request), async {
                match profile_request {
                    Some(request) => self.core.execute_usage(request).await.ok(),
                    None => None,
                }
            });
        let (status, is_json, body) = match usage_reply {
            Ok(reply) => reply,
            Err(AuthBodyError::TooLarge) => {
                return unavailable(SubscriptionUsageUnavailable::InvalidResponse);
            }
            Err(AuthBodyError::Failure(failure)) => {
                return unavailable(SubscriptionUsageUnavailable::Transport(network_failure(
                    failure,
                )));
            }
        };
        let observed_at = Timestamp::new(system_unix_seconds().unwrap_or(now), 0)
            .unwrap_or_else(|_| unreachable!("Unix epoch is a valid timestamp"));
        let reason = match status.as_u16() {
            200 => None,
            401 | 403 => Some(SubscriptionUsageUnavailable::Unauthorized),
            429 => Some(SubscriptionUsageUnavailable::RateLimited),
            _ => Some(SubscriptionUsageUnavailable::ProviderUnavailable),
        };
        if let Some(reason) = reason {
            return SubscriptionUsageRead::Unavailable {
                observed_at,
                reason,
            };
        }
        if !is_json {
            return SubscriptionUsageRead::Unavailable {
                observed_at,
                reason: SubscriptionUsageUnavailable::InvalidResponse,
            };
        }
        if lease.fence() != fence {
            return SubscriptionUsageRead::Unavailable {
                observed_at,
                reason: SubscriptionUsageUnavailable::AccountChanged,
            };
        }
        match crate::decode_claude_subscription_usage(&body, observed_at) {
            Ok(snapshot) if snapshot.contains_ended_window() => {
                SubscriptionUsageRead::Unavailable {
                    observed_at,
                    reason: SubscriptionUsageUnavailable::Stale,
                }
            }
            Ok(snapshot) => SubscriptionUsageRead::Snapshot(attach_claude_profile(
                snapshot,
                profile_reply.as_ref(),
            )),
            Err(_) => SubscriptionUsageRead::Unavailable {
                observed_at,
                reason: SubscriptionUsageUnavailable::InvalidResponse,
            },
        }
    }

    /// The usage read of a custodied login: the usage GET alone, its
    /// `authorization` the custodian's field, relayed through the route's
    /// client. Profile hydration is never relayed, so no e-mail is attached.
    async fn custodied_usage(&self, login: &ClaudeCustodiedLogin) -> SubscriptionUsageRead {
        let unavailable = |reason| SubscriptionUsageRead::Unavailable {
            observed_at: Timestamp::new(system_unix_seconds().unwrap_or(0), 0)
                .unwrap_or_else(|_| unreachable!("Unix epoch is a valid timestamp")),
            reason,
        };
        match login.read().await {
            Ok(_) => {}
            Err(ClaudeNativeAuthError::Transport(cause)) => {
                return unavailable(SubscriptionUsageUnavailable::Transport(cause));
            }
            Err(_) => return unavailable(SubscriptionUsageUnavailable::LocalState),
        }
        let Some(status) = login.status() else {
            return unavailable(SubscriptionUsageUnavailable::NotConnected);
        };
        if let Some(reason) = usage_credential_refusal(status.status()) {
            return unavailable(reason);
        }
        let (Ok(expires_at), Ok(now), Some(field)) = (
            login.access_expires_at(),
            system_unix_seconds(),
            login.field(),
        ) else {
            return unavailable(SubscriptionUsageUnavailable::LocalState);
        };
        if expires_at <= now {
            return unavailable(SubscriptionUsageUnavailable::CredentialState(
                SubscriptionUsageCredentialState::RefreshRequired,
            ));
        }
        let headers = match oauth_control_headers_with(
            plakous_auth::custody::CredentialField::exact(field.to_vec()),
        ) {
            Ok(headers) => headers,
            Err(reason) => return unavailable(reason),
        };
        let Ok(usage_request) = self
            .core
            .oauth_control_get(CLAUDE_SUBSCRIPTION_USAGE_PATH, headers)
        else {
            return unavailable(SubscriptionUsageUnavailable::LocalState);
        };
        let (status, is_json, body) = match self.core.execute_usage(usage_request).await {
            Ok(reply) => reply,
            Err(AuthBodyError::TooLarge) => {
                return unavailable(SubscriptionUsageUnavailable::InvalidResponse);
            }
            Err(AuthBodyError::Failure(failure)) => {
                return unavailable(SubscriptionUsageUnavailable::Transport(network_failure(
                    failure,
                )));
            }
        };
        let observed_at = Timestamp::new(system_unix_seconds().unwrap_or(now), 0)
            .unwrap_or_else(|_| unreachable!("Unix epoch is a valid timestamp"));
        let reason = match status.as_u16() {
            200 if is_json => None,
            200 => Some(SubscriptionUsageUnavailable::InvalidResponse),
            401 | 403 => Some(SubscriptionUsageUnavailable::Unauthorized),
            429 => Some(SubscriptionUsageUnavailable::RateLimited),
            _ => Some(SubscriptionUsageUnavailable::ProviderUnavailable),
        };
        if let Some(reason) = reason {
            return SubscriptionUsageRead::Unavailable {
                observed_at,
                reason,
            };
        }
        match crate::decode_claude_subscription_usage(&body, observed_at) {
            Ok(snapshot) if snapshot.contains_ended_window() => {
                SubscriptionUsageRead::Unavailable {
                    observed_at,
                    reason: SubscriptionUsageUnavailable::Stale,
                }
            }
            Ok(snapshot) => SubscriptionUsageRead::Snapshot(snapshot),
            Err(_) => SubscriptionUsageRead::Unavailable {
                observed_at,
                reason: SubscriptionUsageUnavailable::InvalidResponse,
            },
        }
    }

    /// Begins PKCE Claude OAuth. Completion is always paste of code or callback URL.
    ///
    /// # Errors
    ///
    /// Fails if the compatibility profile is inactive or randomness is unavailable.
    pub fn begin_login(&self) -> Result<ClaudeOAuthLogin, ClaudeNativeAuthError> {
        self.core.ensure_active()?;
        let deadline = ManagerCore::flow_deadline(LOGIN_TIMEOUT)?;
        let baseline = self.core.store()?.load()?;
        let pkce = PkceS256::generate()?;
        let state = plakous_auth::generate_oauth_correlation_token()?;
        let authorization_url =
            claude_authorize_url(pkce.challenge().as_str(), state.expose_secret());
        self.core.ensure_flow_active(deadline)?;
        Ok(ClaudeOAuthLogin {
            core: Arc::clone(&self.core),
            baseline,
            pkce,
            state,
            authorization_url,
            deadline,
            replacement: plakous_auth::LoginReplacement::AnyAccount,
        })
    }

    /// Refreshes the selected login when its exact stored expiry enters skew.
    ///
    /// # Errors
    ///
    /// Ambiguous rotating-token results remain quarantined.
    pub async fn refresh_if_needed(&self) -> Result<(), ClaudeNativeAuthError> {
        self.refresh_with_force(RefreshForce::IfDue).await
    }

    /// Forces one claimed refresh after the provider answered a Messages
    /// dispatch with HTTP 401, regardless of the stored expiry.
    ///
    /// # Errors
    ///
    /// Same fail-closed failures as [`Self::refresh_if_needed`]; a rejected
    /// grant leaves the credential not Ready and is never retried here.
    pub async fn refresh_after_rejection(&self) -> Result<(), ClaudeNativeAuthError> {
        self.refresh_with_force(RefreshForce::Always).await
    }

    /// Stored access-token expiry (Unix seconds) of the Ready credential.
    ///
    /// # Errors
    ///
    /// Fails for a missing, non-Ready, or unreadable record.
    pub fn access_expires_at(&self) -> Result<u64, ClaudeNativeAuthError> {
        if let Some(login) = &self.custodied {
            return login.access_expires_at();
        }
        let fence = self
            .core
            .store()?
            .load()?
            .ok_or(ClaudeNativeAuthError::NotLoggedIn)?;
        Ok(self.core.read_stored(&fence)?.expires_at)
    }

    pub(super) async fn refresh_with_force(
        &self,
        force: RefreshForce,
    ) -> Result<(), ClaudeNativeAuthError> {
        if let Some(login) = &self.custodied {
            match force {
                RefreshForce::IfDue => login.refresh_if_due().await?,
                RefreshForce::Always => login.refresh_after_rejection(None).await?,
                RefreshForce::WhileCurrent(generation) => {
                    login.refresh_after_rejection(Some(generation)).await?
                }
            };
            return if login.is_ready() {
                Ok(())
            } else {
                Err(ClaudeNativeAuthError::NotReady)
            };
        }
        validate_current_ready_record(self.core.store()?)?;
        let flight = self.core.refresh_flight.lock().await;
        let mut hooks = ClaudeRefreshHooks {
            core: &self.core,
            request_gate: self.request_gate.as_ref(),
            observer: self.refresh_observer.as_deref(),
            _flight: flight,
            request_permit: None,
            force,
            detail: CredentialRefreshDetail::default(),
        };
        run_claimed_refresh(self.core.store()?, &self.core.client, &mut hooks).await
    }

    /// Replaces the current credential with a local revoked tombstone.
    ///
    /// Claude Code OAuth has no pinned remote revoke endpoint.
    ///
    /// # Errors
    ///
    /// Rejects stale or missing state.
    pub fn logout_local(&self) -> Result<CredentialGeneration, ClaudeNativeAuthError> {
        let fence = self
            .core
            .store()?
            .load()?
            .ok_or(ClaudeNativeAuthError::NotLoggedIn)?;
        Ok(self.core.store()?.logout(fence.generation())?)
    }

    /// Local-only retirement. There is no pinned Claude remote revoke.
    ///
    /// # Errors
    ///
    /// Rejects missing state or a changed account. If the generation moves
    /// again after one re-read, returns retryable store contention.
    pub fn revoke(&self) -> Result<CredentialGeneration, ClaudeNativeAuthError> {
        let store = self.core.store()?;
        Self::revoke_from_store(store, || {
            store.load()?.ok_or(ClaudeNativeAuthError::NotLoggedIn)
        })
    }

    pub(super) fn revoke_from_store(
        store: &PrivateCredentialStore,
        mut load: impl FnMut() -> Result<CredentialFence, ClaudeNativeAuthError>,
    ) -> Result<CredentialGeneration, ClaudeNativeAuthError> {
        let fence = load()?;
        match store.logout(fence.generation()) {
            Err(PrivateStoreError::StaleWriter) => {
                let next = load()?;
                if next.account_binding() != fence.account_binding() {
                    return Err(PrivateStoreError::ForeignAccount.into());
                }
                store.logout(next.generation()).map_err(|error| {
                    if error == PrivateStoreError::StaleWriter {
                        PrivateStoreError::StoreBusy
                    } else {
                        error
                    }
                    .into()
                })
            }
            result => result.map_err(Into::into),
        }
    }

    /// Returns the pinned enrolled coding catalog when the selected slot is Ready.
    ///
    /// # Errors
    ///
    /// Requires a Ready selected credential.
    pub fn enrolled_catalog(&self) -> Result<Vec<ClaudeEnrolledModel>, ClaudeNativeAuthError> {
        if let Some(login) = &self.custodied {
            if !login.is_ready() {
                return Err(ClaudeNativeAuthError::NotReady);
            }
        } else {
            validate_current_ready_record(self.core.store()?)?;
            let fence = self
                .core
                .store()?
                .load()?
                .ok_or(ClaudeNativeAuthError::NotLoggedIn)?;
            if fence.status() != CredentialStatus::Ready {
                return Err(ClaudeNativeAuthError::NotReady);
            }
        }
        Ok(crate::oauth::CLAUDE_ENROLLMENTS
            .iter()
            .filter(|row| row.oauth)
            .map(|row| ClaudeEnrolledModel {
                exact_id: row.model,
                input_tokens: row.context_window,
            })
            .collect())
    }

    /// Resolves missing account metadata using the authenticated profile endpoint.
    /// The credential lease fences the GET; the existing generation CAS fences
    /// persistence after releasing that lease. A concurrent login/refresh wins.
    pub(crate) async fn ensure_account_identity(&self) -> Result<(), ClaudeNativeAuthError> {
        if let Some(login) = &self.custodied {
            // The custodian hydrates the identity before it reports it.
            login.read().await?;
            return if login.has_account_identity() {
                Ok(())
            } else {
                Err(ClaudeNativeAuthError::AccountIdentityUnavailable)
            };
        }
        let _flight = self.core.refresh_flight.lock().await;
        self.core.ensure_active()?;
        let mut observed_account = None;
        let mut observed_uuid = None;
        for _ in 0..3 {
            let core = Arc::clone(&self.core);
            let (fence, lease) = tokio::task::spawn_blocking(move || {
                let fence = core
                    .store()?
                    .load()?
                    .ok_or(ClaudeNativeAuthError::NotLoggedIn)?;
                let lease = core.store()?.acquire_ready(&fence)?;
                Ok::<_, ClaudeNativeAuthError>((fence, lease))
            })
            .await
            .map_err(|_| ClaudeNativeAuthError::NotReady)??;
            if observed_account
                .as_ref()
                .is_some_and(|account| account != fence.account_binding())
            {
                return Err(ClaudeNativeAuthError::AccountIdentityUnavailable);
            }
            observed_account = Some(fence.account_binding().clone());
            let mut stored: StoredTokens = serde_json::from_slice(lease.payload().expose_secret())
                .map_err(|_| ClaudeNativeAuthError::InvalidStoredCredential)?;
            stored.validate()?;
            if stored.grant_binding != fence.account_binding().as_str() {
                return Err(ClaudeNativeAuthError::InvalidStoredCredential);
            }
            if let Some(uuid) = &stored.account_uuid {
                if observed_uuid
                    .as_ref()
                    .is_some_and(|observed| observed != uuid)
                {
                    return Err(ClaudeNativeAuthError::AccountIdentityUnavailable);
                }
                return Ok(());
            }
            if stored.expires_at <= system_unix_seconds()? {
                return Err(ClaudeNativeAuthError::NotReady);
            }
            let headers = oauth_control_headers(&stored)
                .map_err(|_| ClaudeNativeAuthError::InvalidStoredCredential)?;
            let request = self
                .core
                .oauth_control_get(CLAUDE_SUBSCRIPTION_PROFILE_PATH, headers)?;
            let (status, is_json, body) =
                self.core.execute_usage(request).await.map_err(|error| {
                    ClaudeNativeAuthError::Transport(match error {
                        AuthBodyError::Failure(failure) => network_failure(failure),
                        AuthBodyError::TooLarge => None,
                    })
                })?;
            if status.as_u16() != 200 || !is_json {
                return Err(ClaudeNativeAuthError::AccountIdentityUnavailable);
            }
            let profile = crate::decode_claude_oauth_profile(&body)
                .map_err(|_| ClaudeNativeAuthError::AccountIdentityUnavailable)?;
            stored.account_uuid = Some(
                profile
                    .account_uuid
                    .ok_or(ClaudeNativeAuthError::AccountIdentityUnavailable)?,
            );
            if observed_uuid
                .as_ref()
                .is_some_and(|observed| Some(observed) != stored.account_uuid.as_ref())
            {
                return Err(ClaudeNativeAuthError::AccountIdentityUnavailable);
            }
            observed_uuid = stored.account_uuid.clone();
            drop(lease);
            let replacement = credential_record(fence.generation().next()?, &stored)?;
            // Another manager can retain a Ready lease across its profile GET.
            // Never block the executor while waiting for that reader to finish.
            let core = Arc::clone(&self.core);
            self.core.store()?;
            let replaced = tokio::task::spawn_blocking(move || {
                core.store
                    .as_ref()
                    .ok_or(PrivateStoreError::InvalidPath)?
                    .replace(fence.generation(), &replacement)
            })
            .await
            .map_err(|_| ClaudeNativeAuthError::NotReady)?;
            match replaced {
                Ok(()) => return Ok(()),
                Err(PrivateStoreError::StaleWriter) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err(ClaudeNativeAuthError::NotReady)
    }

    #[cfg(test)]
    pub(crate) fn request_identity(
        &self,
    ) -> Result<crate::ClaudeOAuthRequestIdentity, ClaudeNativeAuthError> {
        self.identity_credentials().map(|(_, identity)| identity)
    }

    pub(crate) fn identity_credentials(
        &self,
    ) -> Result<
        (
            (ClaudeOAuthAccess, u64, AccountBinding),
            crate::ClaudeOAuthRequestIdentity,
        ),
        ClaudeNativeAuthError,
    > {
        if let Some(login) = &self.custodied {
            return login.identity_credentials();
        }
        let fence = self
            .core
            .store()?
            .load()?
            .ok_or(ClaudeNativeAuthError::NotLoggedIn)?;
        if fence.status() != CredentialStatus::Ready {
            return Err(ClaudeNativeAuthError::NotReady);
        }
        let stored = self.core.read_stored(&fence)?;
        let account = stored
            .account_uuid
            .clone()
            .ok_or(ClaudeNativeAuthError::AccountIdentityUnavailable)?;
        Ok((
            (
                ClaudeOAuthAccess::Token(stored.access_token.clone()),
                fence.generation().get(),
                fence.account_binding().clone(),
            ),
            crate::ClaudeOAuthRequestIdentity::new(account, self.core.device_id.clone()),
        ))
    }

    /// Returns the Ready access token and credential generation for dispatch.
    pub(crate) fn dispatch_credentials(
        &self,
    ) -> Result<(ClaudeOAuthAccess, u64, AccountBinding), ClaudeNativeAuthError> {
        if let Some(login) = &self.custodied {
            return login
                .identity_credentials()
                .map(|(credentials, _)| credentials);
        }
        validate_current_ready_record(self.core.store()?)?;
        let fence = self
            .core
            .store()?
            .load()?
            .ok_or(ClaudeNativeAuthError::NotLoggedIn)?;
        if fence.status() != CredentialStatus::Ready {
            return Err(ClaudeNativeAuthError::NotReady);
        }
        let stored = self.core.read_stored(&fence)?;
        Ok((
            ClaudeOAuthAccess::Token(stored.access_token.clone()),
            fence.generation().get(),
            fence.account_binding().clone(),
        ))
    }

    #[cfg(test)]
    pub(crate) fn persist_test_grant(
        &self,
        access_token: &str,
        refresh_token: &str,
        expires_in: i64,
        scope: &str,
    ) -> Result<ClaudeSubscriptionStatus, ClaudeNativeAuthError> {
        self.persist_test_grant_for_account(
            access_token,
            refresh_token,
            expires_in,
            scope,
            "12345678-1234-4234-8234-123456789abc",
        )
    }

    #[cfg(test)]
    pub(crate) fn persist_test_grant_for_account(
        &self,
        access_token: &str,
        refresh_token: &str,
        expires_in: i64,
        scope: &str,
        account: &str,
    ) -> Result<ClaudeSubscriptionStatus, ClaudeNativeAuthError> {
        let body = format!(
            r#"{{"access_token":"{access_token}","refresh_token":"{refresh_token}","expires_in":{expires_in},"scope":"{scope}","account":{{"uuid":"{account}"}}}}"#
        );
        let grant = parse_claude_oauth_token_grant(body.as_bytes())
            .map_err(|_| ClaudeNativeAuthError::InvalidTokenResponse)?;
        self.core.persist_grant(
            self.core.store()?.load()?.as_ref(),
            &grant,
            plakous_auth::LoginReplacement::AnyAccount,
        )
    }
}

impl fmt::Debug for ClaudeNativeAuthManager {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClaudeNativeAuthManager")
            .field("profile", &CLAUDE_OAUTH_COMPATIBILITY_PROFILE_ID)
            .finish_non_exhaustive()
    }
}
