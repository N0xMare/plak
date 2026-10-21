//! Claimed refresh and revoke hooks and the selected credential lifecycle.

use super::{
    ACCESS_REFRESH_SKEW, Arc, AuthSingleFlightGuard, BuildRequestFailure, CancellationToken,
    ClaimedHttpDisposition, ClaimedHttpError, ClaimedHttpResponse, ClaimedRefreshHooks,
    ClaimedRevokeHooks, Client, CodexCredentialStatus, CodexSubscriptionCredentialManager,
    CredentialFence, CredentialGeneration, CredentialRecord, CredentialRefreshDetail,
    CredentialRefreshFailureReason, CredentialRefreshObserver, CredentialStatus, Digest, Duration,
    ExchangeControl, ExpectedSelection, Instant, MAX_AUTH_BODY_BYTES, MAX_TOKEN_BYTES, Method,
    NativeCodexAuthError, NativeCodexAuthorization, NativeCodexCatalogWitness,
    NativeCodexCredentialAuthority, NativeCodexPreflightRequest, OAuthRefreshTokenResponse,
    OPENAI_CODEX_COMPATIBILITY_PROFILE_ID, OPENAI_CODEX_NATIVE_CLIENT_ID,
    OPENAI_CODEX_OAUTH_SCOPES, PROFILE_EXPIRES_AT, PrivateCredentialStore, PrivateStoreError,
    PullBodyError, REVOKE_PATH, ReadyCredentialLease, RefreshClaim, Request,
    ResolvedModelDescriptor, RuntimeModelAuthority, RuntimeModelAuthorityError, Serialize,
    StatusCode, StoredCredential, SubscriptionRequestClass, SubscriptionRequestPermit, TOKEN_PATH,
    Timestamp, access_needs_refresh, claimed_request, credential_selection_epoch, json_headers,
    local_logout, network_failure, parse_stored, parse_strict_json, pull_claimed_body,
    pull_claimed_body_cancellable, run_blocking, run_claimed_refresh, run_claimed_revoke,
    stored_record, validate_exact_scopes, validate_profile, validate_profile_at,
};

#[derive(Serialize)]
pub(super) struct RefreshRequest<'a> {
    pub(super) client_id: &'a str,
    pub(super) grant_type: &'a str,
    pub(super) refresh_token: &'a str,
}

#[derive(Serialize)]
pub(super) struct RevokeRequest<'a> {
    pub(super) token: &'a str,
    pub(super) token_type_hint: &'a str,
    pub(super) client_id: &'a str,
}

/// Shared-orchestrator hooks for Codex claimed refresh (JSON body, RS256).
pub(super) struct CodexRefreshHooks<'a> {
    pub(super) manager: &'a CodexSubscriptionCredentialManager,
    /// A receipt renewal refreshes only its own login selection.
    pub(super) expected: Option<ExpectedSelection<'a>>,
    pub(super) deadline: Instant,
    pub(super) cancellation: Option<&'a plakous_model::CancellationToken>,
    pub(super) flight: Option<AuthSingleFlightGuard>,
    pub(super) request_permit: Option<Box<dyn SubscriptionRequestPermit>>,
    pub(super) transport_cancellation: Option<CancellationToken>,
    pub(super) cancelled_during_exchange: bool,
    /// Whether to refresh even when the stored expiry is outside the lead.
    pub(super) force: RefreshForce<'a>,
    /// Numbers and rejecting check of the received response, for diagnostics.
    pub(super) detail: CredentialRefreshDetail,
}

/// When a claimed refresh runs although the stored access token is not yet
/// inside its refresh lead (the provider rejected it with HTTP 401).
#[derive(Clone, Copy)]
pub(super) enum RefreshForce<'a> {
    /// Only when the access token is due.
    IfDue,
    /// Always (an explicit refresh).
    Always,
    /// Only while the store, read under the flight, still holds this exact
    /// rejected generation: a concurrent rotation of the same login is not
    /// rotated again.
    WhileCurrent(&'a CredentialFence),
}

impl RefreshForce<'_> {
    pub(super) fn forces(self, current: &CredentialFence) -> bool {
        match self {
            Self::IfDue => false,
            Self::Always => true,
            Self::WhileCurrent(rejected) => rejected == current,
        }
    }
}

impl CodexRefreshHooks<'_> {
    pub(super) fn check_expected(
        &self,
        fence: &CredentialFence,
        credential: &StoredCredential,
    ) -> Result<(), NativeCodexAuthError> {
        let Some(expected) = self.expected else {
            return Ok(());
        };
        expected.check(
            fence.account_binding(),
            credential_selection_epoch(
                self.manager.route_account_digest(),
                self.manager.authority_binding,
                &credential.selection_epoch,
            )?,
        )
    }
}

#[allow(
    clippy::unused_async_trait_impl,
    reason = "hook methods implement the orchestrator's async trait; some bodies finish without awaiting, and async keeps them lazy until the orchestrator polls"
)]
impl ClaimedRefreshHooks for CodexRefreshHooks<'_> {
    type Error = NativeCodexAuthError;
    type Prepared = StoredCredential;
    type Output = ();

    async fn check_profile(&mut self) -> Result<(), Self::Error> {
        self.manager.validate_profile_admission()
    }

    async fn fence_needing_refresh(&mut self) -> Result<Option<CredentialFence>, Self::Error> {
        let initial = self.manager.load_fence().await?;
        let selected = self.manager.selected_account()?;
        if initial.account_binding() != &selected {
            return Err(NativeCodexAuthError::AccountChanged);
        }
        if initial.status() == CredentialStatus::Ready {
            let lease = self.manager.acquire_ready_lease(initial.clone()).await?;
            let credential = parse_stored(lease.payload())?;
            self.check_expected(&initial, &credential)?;
            let fresh = !access_needs_refresh(
                credential.access_expires_at,
                self.manager.clock.now().seconds(),
                ACCESS_REFRESH_SKEW,
            );
            drop(credential);
            drop(lease);
            if fresh && !self.force.forces(&initial) {
                return Ok(None);
            }
        } else if initial.status() != CredentialStatus::RefreshRequired {
            return Err(NativeCodexAuthError::Unavailable);
        }

        self.flight = Some(self.manager.enter_flight(&initial).await?);
        let current = self.manager.load_fence().await?;
        if current.account_binding() != &selected {
            return Err(NativeCodexAuthError::AccountChanged);
        }
        if current.status() == CredentialStatus::Ready {
            let lease = self.manager.acquire_ready_lease(current.clone()).await?;
            let credential = parse_stored(lease.payload())?;
            self.check_expected(&current, &credential)?;
            let fresh = !access_needs_refresh(
                credential.access_expires_at,
                self.manager.clock.now().seconds(),
                ACCESS_REFRESH_SKEW,
            );
            drop(credential);
            drop(lease);
            if fresh && !self.force.forces(&current) {
                return Ok(None);
            }
        } else if current.status() != CredentialStatus::RefreshRequired {
            return Err(NativeCodexAuthError::Unavailable);
        }

        self.request_permit = self
            .manager
            .request_gate
            .as_ref()
            .map(|gate| gate.acquire(SubscriptionRequestClass::AuthRefresh))
            .transpose()
            .map_err(|_| NativeCodexAuthError::Unavailable)?;
        Ok(Some(current))
    }

    fn fresh_output(&mut self) -> Result<Self::Output, Self::Error> {
        Ok(())
    }

    fn build_refresh_request(
        &mut self,
        claim: &RefreshClaim,
    ) -> Result<(Self::Prepared, Request), BuildRequestFailure<Self::Error>> {
        let claim_fence = claim.fence();
        // Nothing was sent: a payload this build cannot use is restored as it
        // was, never quarantined, and needs a new login.
        let old = match parse_stored(claim.payload()) {
            Ok(old) => old,
            Err(error) => {
                self.detail.reason = Some(if error == NativeCodexAuthError::LoginRequired {
                    CredentialRefreshFailureReason::RetiredFormat
                } else {
                    CredentialRefreshFailureReason::StoredCredential
                });
                return Err(BuildRequestFailure::Restore(error));
            }
        };
        let scopes = match validate_exact_scopes(OPENAI_CODEX_OAUTH_SCOPES) {
            Ok(scopes) => scopes,
            Err(error) => return Err(BuildRequestFailure::LeaveQuarantined(error)),
        };
        if old.account_id != claim_fence.account_binding().as_str() || old.scopes != scopes {
            return Err(BuildRequestFailure::LeaveQuarantined(
                NativeCodexAuthError::Validation,
            ));
        }
        // Nothing was sent: a renewal that finds another login restores it.
        if let Err(error) = self.check_expected(&claim_fence, &old) {
            return Err(BuildRequestFailure::Restore(error));
        }
        let body = match serde_json::to_vec(&RefreshRequest {
            client_id: OPENAI_CODEX_NATIVE_CLIENT_ID,
            grant_type: "refresh_token",
            refresh_token: &old.refresh_token,
        }) {
            Ok(body) => body,
            Err(_) => {
                return Err(BuildRequestFailure::Restore(
                    NativeCodexAuthError::Validation,
                ));
            }
        };
        let headers = match json_headers(None) {
            Ok(headers) => headers,
            Err(error) => return Err(BuildRequestFailure::Restore(error)),
        };
        let request = match claimed_request(
            &self.manager.client,
            &self.manager.endpoints.auth_origin,
            Method::POST,
            TOKEN_PATH,
            headers,
            body,
        ) {
            Ok(request) => request,
            Err(error) => return Err(BuildRequestFailure::Restore(error)),
        };
        Ok((old, request))
    }

    async fn authorize_dispatch(&mut self) -> Result<(), Self::Error> {
        if self
            .cancellation
            .is_some_and(plakous_model::CancellationToken::is_cancelled)
        {
            return Err(NativeCodexAuthError::Transport(None));
        }
        if let Some(permit) = &mut self.request_permit
            && permit.authorize_dispatch().is_err()
        {
            return Err(NativeCodexAuthError::Unavailable);
        }
        Ok(())
    }

    fn exchange_control(&mut self) -> ExchangeControl {
        let transport = CancellationToken::new();
        self.transport_cancellation = Some(transport.clone());
        ExchangeControl::with_cancellation(
            self.deadline.min(Instant::now() + Duration::from_secs(30)),
            transport,
        )
    }

    async fn execute_claimed(
        &mut self,
        store: &PrivateCredentialStore,
        client: &Client,
        claim: RefreshClaim,
        request: Request,
        control: ExchangeControl,
    ) -> Result<ClaimedHttpResponse, ClaimedHttpError> {
        let mut exchange = Box::pin(store.execute_claimed_http(claim, client, request, control));
        if let Some(cancellation) = self.cancellation {
            tokio::select! {
                reply = &mut exchange => reply,
                () = cancellation.cancelled() => {
                    self.cancelled_during_exchange = true;
                    if let Some(transport) = &self.transport_cancellation {
                        transport.cancel();
                    }
                    exchange.await
                }
            }
        } else {
            exchange.await
        }
    }

    async fn validate_tokens(
        &mut self,
        prepared: Self::Prepared,
        response: &mut ClaimedHttpResponse,
    ) -> Result<CredentialRecord, Self::Error> {
        if self.cancelled_during_exchange {
            return Err(NativeCodexAuthError::Transport(None));
        }
        let body = pull_claimed_body_cancellable(response, self.cancellation).await?;
        if !response.status().is_success() {
            self.detail.reason = Some(CredentialRefreshFailureReason::ProviderRejected);
            return Err(NativeCodexAuthError::Provider);
        }
        let tokens: OAuthRefreshTokenResponse = parse_strict_json(&body).inspect_err(|_| {
            self.detail.reason = Some(CredentialRefreshFailureReason::TokenShape);
        })?;
        self.detail = CredentialRefreshDetail::from_response(
            self.manager.clock.now().seconds(),
            None,
            tokens.id_token.as_deref(),
            MAX_TOKEN_BYTES,
        );
        let mut validation = Box::pin(self.manager.validate_refresh_response(
            tokens,
            prepared,
            self.deadline,
            &mut self.detail.reason,
        ));
        let validated = if let Some(cancellation) = self.cancellation {
            tokio::select! {
                biased;
                result = &mut validation => Some(result),
                () = cancellation.cancelled() => None,
                () = tokio::time::sleep_until(self.deadline.into()) => None,
            }
        } else {
            Some(validation.await)
        };
        let replacement = match validated {
            None => return Err(NativeCodexAuthError::Transport(None)),
            Some(Ok(credential)) => credential,
            Some(Err(error)) => return Err(error),
        };
        let generation = response
            .refresh_fence()
            .generation()
            .next()
            .map_err(|_| NativeCodexAuthError::Store)?;
        stored_record(&replacement, generation)
    }

    async fn recheck_profile_before_complete(&mut self) -> Result<(), Self::Error> {
        self.manager.validate_profile_admission()?;
        validate_profile_at(
            self.manager.acceptance.accepted_at,
            self.manager.clock.now().seconds(),
        )
    }

    async fn map_claimed_http_error(
        &mut self,
        _store: &PrivateCredentialStore,
        error: ClaimedHttpError,
    ) -> Self::Error {
        map_codex_claimed_http_error(error)
    }

    fn map_store_error(&mut self, _error: PrivateStoreError) -> Self::Error {
        NativeCodexAuthError::Store
    }

    fn failure_detail(&mut self, error: &Self::Error) -> CredentialRefreshDetail {
        let fallback = match error {
            NativeCodexAuthError::AccountChanged => {
                Some(CredentialRefreshFailureReason::SubjectMismatch)
            }
            NativeCodexAuthError::SelectionChanged => {
                Some(CredentialRefreshFailureReason::SelectionChanged)
            }
            NativeCodexAuthError::Provider => {
                Some(CredentialRefreshFailureReason::ProviderRejected)
            }
            _ => None,
        };
        let reason = self.detail.reason.or(fallback);
        self.detail.with_reason(reason)
    }

    fn output_from_completed(
        &mut self,
        _fence: CredentialFence,
    ) -> Result<Self::Output, Self::Error> {
        Ok(())
    }

    fn refresh_observer(&self) -> Option<&dyn CredentialRefreshObserver> {
        self.manager.refresh_observer.as_deref()
    }
}

/// Shared-orchestrator hooks for Codex claimed remote revoke (JSON body).
pub(super) struct CodexRevokeHooks<'a> {
    pub(super) manager: &'a CodexSubscriptionCredentialManager,
    pub(super) flight: Option<AuthSingleFlightGuard>,
}

#[allow(
    clippy::unused_async_trait_impl,
    reason = "hook methods implement the orchestrator's async trait; some bodies finish without awaiting, and async keeps them lazy until the orchestrator polls"
)]
impl ClaimedRevokeHooks for CodexRevokeHooks<'_> {
    type Error = NativeCodexAuthError;
    type Output = CodexCredentialStatus;

    async fn check_profile(&mut self) -> Result<(), Self::Error> {
        self.manager.validate_profile_admission()
    }

    async fn fence_to_revoke(&mut self) -> Result<CredentialFence, Self::Error> {
        let current = self.manager.load_fence().await?;
        if current.account_binding() != &self.manager.selected_account()? {
            return Err(NativeCodexAuthError::AccountChanged);
        }
        self.flight = Some(self.manager.enter_flight(&current).await?);
        let current = self.manager.load_fence().await?;
        if current.account_binding() != &self.manager.selected_account()? {
            return Err(NativeCodexAuthError::AccountChanged);
        }
        Ok(current)
    }

    fn build_revoke_request(
        &mut self,
        claim: &RefreshClaim,
    ) -> Result<Request, BuildRequestFailure<Self::Error>> {
        let claim_fence = claim.fence();
        // A login this build cannot parse (an earlier build's) is not
        // revoked remotely here; nothing was sent, so it is restored as it was.
        let credential = match parse_stored(claim.payload()) {
            Ok(credential) => credential,
            Err(error) => return Err(BuildRequestFailure::Restore(error)),
        };
        if credential.account_id != claim_fence.account_binding().as_str() {
            return Err(BuildRequestFailure::LeaveQuarantined(
                NativeCodexAuthError::AccountChanged,
            ));
        }
        let body = match serde_json::to_vec(&RevokeRequest {
            token: &credential.refresh_token,
            token_type_hint: "refresh_token",
            client_id: OPENAI_CODEX_NATIVE_CLIENT_ID,
        }) {
            Ok(body) => body,
            Err(_) => {
                return Err(BuildRequestFailure::Restore(
                    NativeCodexAuthError::Validation,
                ));
            }
        };
        let headers = match json_headers(None) {
            Ok(headers) => headers,
            Err(error) => return Err(BuildRequestFailure::Restore(error)),
        };
        claimed_request(
            &self.manager.client,
            &self.manager.endpoints.auth_origin,
            Method::POST,
            REVOKE_PATH,
            headers,
            body,
        )
        .map_err(BuildRequestFailure::Restore)
    }

    fn exchange_control(&mut self) -> ExchangeControl {
        ExchangeControl::new(Instant::now() + Duration::from_secs(30))
    }

    async fn accept_revoke_response(
        &mut self,
        response: &mut ClaimedHttpResponse,
    ) -> Result<(), Self::Error> {
        let _body = pull_claimed_body(response, MAX_AUTH_BODY_BYTES)
            .await
            .map_err(|error| match error {
                PullBodyError::Transport => NativeCodexAuthError::Transport(None),
                PullBodyError::TooLarge => NativeCodexAuthError::Validation,
                _ => NativeCodexAuthError::Transport(None),
            })?;
        if !matches!(response.status(), StatusCode::OK | StatusCode::NO_CONTENT) {
            return Err(NativeCodexAuthError::Provider);
        }
        Ok(())
    }

    async fn recheck_profile_before_complete(&mut self) -> Result<(), Self::Error> {
        self.manager.validate_profile_admission()?;
        validate_profile_at(
            self.manager.acceptance.accepted_at,
            self.manager.clock.now().seconds(),
        )
    }

    async fn map_claimed_http_error(
        &mut self,
        _store: &PrivateCredentialStore,
        error: ClaimedHttpError,
    ) -> Self::Error {
        map_codex_claimed_http_error(error)
    }

    fn map_store_error(&mut self, _error: PrivateStoreError) -> Self::Error {
        NativeCodexAuthError::Store
    }

    fn output_from_revoked(
        &mut self,
        _generation: CredentialGeneration,
    ) -> Result<Self::Output, Self::Error> {
        self.manager.clear_selected_account()?;
        Ok(CodexCredentialStatus::Revoked)
    }

    fn observer(&self) -> Option<&dyn CredentialRefreshObserver> {
        self.manager.refresh_observer.as_deref()
    }
}

pub(super) fn map_codex_claimed_http_error(error: ClaimedHttpError) -> NativeCodexAuthError {
    match error.disposition() {
        ClaimedHttpDisposition::RestoredNotStarted
        | ClaimedHttpDisposition::QuarantinedMayHaveTransmitted => {
            NativeCodexAuthError::Transport(error.transport_failure().and_then(network_failure))
        }
        ClaimedHttpDisposition::RestorationFailed | ClaimedHttpDisposition::ClaimRejected => {
            NativeCodexAuthError::Store
        }
        _ => NativeCodexAuthError::Store,
    }
}

impl CodexSubscriptionCredentialManager {
    /// Refreshes only when the current access generation is near expiry.
    ///
    /// The old generation is durably Unknown before token I/O. After payload
    /// exposure, every transport, validation, `UserInfo`, or persistence failure
    /// leaves that generation quarantined and is never retried automatically.
    ///
    /// # Errors
    ///
    /// Rejects unavailable, changed, quarantined, or invalid credentials and
    /// any transport, provider, identity, profile, or persistence failure.
    pub async fn refresh_if_needed(&self) -> Result<(), NativeCodexAuthError> {
        let _lifecycle = self.lifecycle.lock().await;
        self.refresh_if_needed_locked(Instant::now() + Duration::from_secs(30), None)
            .await
    }

    pub(super) async fn refresh_if_needed_locked(
        &self,
        deadline: Instant,
        cancellation: Option<&plakous_model::CancellationToken>,
    ) -> Result<(), NativeCodexAuthError> {
        self.refresh_locked_with_force(deadline, cancellation, RefreshForce::IfDue, None)
            .await
    }

    /// Runs the claimed refresh ladder; `force` claims a new generation even
    /// when the stored access token is not yet inside the refresh skew (the
    /// provider rejected that access token with HTTP 401).
    pub(super) async fn refresh_locked_with_force(
        &self,
        deadline: Instant,
        cancellation: Option<&plakous_model::CancellationToken>,
        force: RefreshForce<'_>,
        expected: Option<ExpectedSelection<'_>>,
    ) -> Result<(), NativeCodexAuthError> {
        let mut hooks = CodexRefreshHooks {
            manager: self,
            expected,
            deadline,
            cancellation,
            flight: None,
            request_permit: None,
            transport_cancellation: None,
            cancelled_during_exchange: false,
            force,
            detail: CredentialRefreshDetail::default(),
        };
        run_claimed_refresh(&self.store, &self.client, &mut hooks).await
    }

    /// Forces one claimed refresh after the provider answered an inference
    /// dispatch with HTTP 401, regardless of the stored expiry.
    ///
    /// # Errors
    ///
    /// Same fail-closed failures as [`Self::refresh_if_needed`]; a rejected
    /// grant leaves the generation not Ready and is never retried here.
    pub async fn refresh_after_rejection(&self) -> Result<(), NativeCodexAuthError> {
        let _lifecycle = self.lifecycle.lock().await;
        self.refresh_locked_with_force(
            Instant::now() + Duration::from_secs(30),
            None,
            RefreshForce::Always,
            None,
        )
        .await
    }

    /// The 401 recovery of one receipt: forces a claimed refresh only while
    /// the store, read under the refresh flight, still holds the rejected
    /// generation, and only for its account and login selection. A generation
    /// another process already rotated is refreshed only if due; another login
    /// fails `SelectionChanged` or `AccountChanged` before any claim.
    pub(super) async fn refresh_rejected_selection(
        &self,
        rejected: &CredentialFence,
        selection_epoch: Digest,
    ) -> Result<(), NativeCodexAuthError> {
        let _lifecycle = self.lifecycle.lock().await;
        self.refresh_locked_with_force(
            Instant::now() + Duration::from_secs(30),
            None,
            RefreshForce::WhileCurrent(rejected),
            Some(ExpectedSelection {
                account: rejected.account_binding(),
                selection_epoch,
            }),
        )
        .await
    }

    /// Revokes the rotating refresh token and commits a local tombstone.
    /// Any post-start failure remains quarantined.
    ///
    /// # Errors
    ///
    /// Rejects unavailable or changed credentials and any failed remote revoke
    /// or exact local tombstone transition.
    pub async fn logout(&self) -> Result<CodexCredentialStatus, NativeCodexAuthError> {
        let _lifecycle = self.lifecycle.lock().await;
        let mut hooks = CodexRevokeHooks {
            manager: self,
            flight: None,
        };
        run_claimed_revoke(&self.store, &self.client, &mut hooks).await
    }

    /// Alias making explicit that logout performs provider revocation first.
    ///
    /// # Errors
    ///
    /// Returns the same redaction-safe failures as [`Self::logout`].
    pub async fn revoke(&self) -> Result<CodexCredentialStatus, NativeCodexAuthError> {
        self.logout().await
    }

    /// Commits an always-available local revoked tombstone without network I/O.
    ///
    /// This cleanup remains available after profile expiry or kill-switch
    /// activation and is deliberately distinct from [`Self::revoke`].
    ///
    /// # Errors
    ///
    /// Fails only when the explicit private slot cannot be reloaded or changed
    /// by exact generation CAS.
    pub async fn logout_local(&self) -> Result<CodexCredentialStatus, NativeCodexAuthError> {
        let _lifecycle = self.lifecycle.lock().await;
        let store = Arc::clone(&self.store);
        let status = run_blocking(move || local_logout(&store)).await?;
        self.clear_selected_account()?;
        Ok(status)
    }

    pub(super) async fn load_fence(&self) -> Result<CredentialFence, NativeCodexAuthError> {
        let store = Arc::clone(&self.store);
        run_blocking(move || {
            store
                .load()
                .map_err(|_| NativeCodexAuthError::Store)?
                .ok_or(NativeCodexAuthError::Unavailable)
        })
        .await
    }

    pub(super) async fn try_load_fence(&self) -> Result<CredentialFence, NativeCodexAuthError> {
        let store = Arc::clone(&self.store);
        run_blocking(move || {
            store
                .try_load()
                .map_err(|error| match error {
                    PrivateStoreError::StoreBusy => NativeCodexAuthError::StoreBusy,
                    _ => NativeCodexAuthError::Store,
                })?
                .ok_or(NativeCodexAuthError::Unavailable)
        })
        .await
    }

    /// Whether a durable Unknown `fence` is a settled outcome rather than a
    /// refresh still in flight in this or another process; see
    /// [`PrivateCredentialStore::refresh_outcome_settled`].
    pub(super) async fn refresh_outcome_settled(
        &self,
        fence: CredentialFence,
    ) -> Result<bool, NativeCodexAuthError> {
        let store = Arc::clone(&self.store);
        run_blocking(move || {
            store
                .refresh_outcome_settled(&fence)
                .map_err(|_| NativeCodexAuthError::Store)
        })
        .await
    }

    pub(super) async fn acquire_ready_lease(
        &self,
        fence: CredentialFence,
    ) -> Result<ReadyCredentialLease, NativeCodexAuthError> {
        let store = Arc::clone(&self.store);
        run_blocking(move || {
            store
                .acquire_ready(&fence)
                .map_err(|_| NativeCodexAuthError::Store)
        })
        .await
    }

    /// Attempts a shared Ready lease without waiting. `Ok(None)` means another
    /// holder currently has the store lease; it is contention, not a fault.
    pub(super) async fn try_acquire_ready_lease(
        &self,
        fence: CredentialFence,
    ) -> Result<Option<ReadyCredentialLease>, NativeCodexAuthError> {
        let store = Arc::clone(&self.store);
        run_blocking(move || {
            store
                .try_acquire_ready(&fence)
                .map_err(|_| NativeCodexAuthError::Store)
        })
        .await
    }

    pub(super) async fn enter_flight(
        &self,
        fence: &CredentialFence,
    ) -> Result<AuthSingleFlightGuard, NativeCodexAuthError> {
        let flights = self.flights.clone();
        let profile = fence.profile_id().clone();
        let account = fence.account_binding().clone();
        tokio::task::spawn_blocking(move || flights.enter(&profile, &account))
            .await
            .map_err(|_| NativeCodexAuthError::Coordination)?
            .map_err(|_| NativeCodexAuthError::Coordination)
    }

    pub(super) async fn revalidate_model_runtime_authority(
        &self,
        descriptor: &ResolvedModelDescriptor,
        witness: &NativeCodexCatalogWitness,
        checked_at: Timestamp,
    ) -> Result<RuntimeModelAuthority, RuntimeModelAuthorityError> {
        if witness.observed_at() > checked_at || witness.expires_at() <= checked_at {
            return Err(RuntimeModelAuthorityError::Stale);
        }
        validate_profile(&self.acceptance, checked_at.seconds())
            .map_err(|_| RuntimeModelAuthorityError::Stale)?;
        if descriptor.profile().as_str() != OPENAI_CODEX_COMPATIBILITY_PROFILE_ID
            || self.validate_catalog_witness(descriptor, witness).is_err()
        {
            return Err(RuntimeModelAuthorityError::Unavailable);
        }
        let _lifecycle = self.lifecycle.lock().await;
        validate_profile(&self.acceptance, checked_at.seconds())
            .map_err(|_| RuntimeModelAuthorityError::Stale)?;
        if self.validate_catalog_witness(descriptor, witness).is_err() {
            return Err(RuntimeModelAuthorityError::Unavailable);
        }
        let expected = witness.credential_fence();
        let current = self
            .load_fence()
            .await
            .map_err(|_| RuntimeModelAuthorityError::Unavailable)?;
        let selected = self
            .selected_account()
            .map_err(|_| RuntimeModelAuthorityError::Unavailable)?;
        let account_binding = self.route_account_digest();
        if current != *expected
            || current.status() != CredentialStatus::Ready
            || current.profile_id().as_str() != OPENAI_CODEX_COMPATIBILITY_PROFILE_ID
            || current.account_binding() != &selected
        {
            return Err(RuntimeModelAuthorityError::Unavailable);
        }
        let lease = self
            .acquire_ready_lease(current.clone())
            .await
            .map_err(|_| RuntimeModelAuthorityError::Unavailable)?;
        let credential =
            parse_stored(lease.payload()).map_err(|_| RuntimeModelAuthorityError::Unavailable)?;
        let stored_epoch = credential_selection_epoch(
            account_binding,
            self.authority_binding,
            &credential.selection_epoch,
        )
        .map_err(|_| RuntimeModelAuthorityError::Invalid)?;
        let usable_until_seconds = credential
            .access_expires_at
            .saturating_sub(ACCESS_REFRESH_SKEW);
        if lease.fence() != current
            || witness.credential_selection_epoch() != stored_epoch
            || credential.account_id != selected.as_str()
            || usable_until_seconds <= checked_at.seconds()
            || validate_exact_scopes(&credential.scopes.join(" ")).is_err()
        {
            return Err(RuntimeModelAuthorityError::Unavailable);
        }
        let access_until = Timestamp::new(usable_until_seconds, 0)
            .map_err(|_| RuntimeModelAuthorityError::Invalid)?;
        let profile_until = Timestamp::new(PROFILE_EXPIRES_AT, 0)
            .map_err(|_| RuntimeModelAuthorityError::Invalid)?;
        let valid_until = witness.expires_at().min(access_until).min(profile_until);
        if valid_until <= checked_at {
            return Err(RuntimeModelAuthorityError::Stale);
        }
        RuntimeModelAuthority::new(
            descriptor,
            account_binding,
            self.authority_binding,
            current.generation().get(),
            checked_at,
            Some(valid_until),
        )
    }

    pub(super) fn validate_preflight_target(
        &self,
        request: &NativeCodexPreflightRequest,
    ) -> Result<(), NativeCodexAuthError> {
        self.validate_profile_admission()
            .map_err(|_| NativeCodexAuthError::Unavailable)?;
        let target = request.target();
        if target.route_id().as_str() != crate::OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID
            || target.profile().as_str() != OPENAI_CODEX_COMPATIBILITY_PROFILE_ID
            || target.account_binding() != self.route_account_digest()
            || target.credential_authority_binding() != self.authority_binding
            || target.model().as_str().is_empty()
        {
            return Err(NativeCodexAuthError::AccountChanged);
        }
        if request.cancellation().is_cancelled() || Instant::now() >= request.deadline() {
            return Err(NativeCodexAuthError::Transport(None));
        }
        let Some((expected_fence, _)) = target.native_codex_credential_selection() else {
            return Err(NativeCodexAuthError::Unavailable);
        };
        let selected_account = self.selected_account()?;
        if expected_fence.status() != CredentialStatus::Ready
            || expected_fence.profile_id().as_str() != OPENAI_CODEX_COMPATIBILITY_PROFILE_ID
            || expected_fence.account_binding() != &selected_account
        {
            return Err(NativeCodexAuthError::Unavailable);
        }
        Ok(())
    }

    pub(super) async fn preflight_authorization(
        &self,
        request: NativeCodexPreflightRequest,
    ) -> Result<NativeCodexAuthorization, NativeCodexAuthError> {
        let deadline = request.deadline();
        let cancellation = request.cancellation().clone();
        let operation = self.preflight_authorization_inner(request);
        tokio::pin!(operation);
        tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(NativeCodexAuthError::Transport(None)),
            () = tokio::time::sleep_until(deadline.into()) => Err(NativeCodexAuthError::Transport(None)),
            result = &mut operation => result,
        }
    }

    pub(super) async fn preflight_authorization_inner(
        &self,
        request: NativeCodexPreflightRequest,
    ) -> Result<NativeCodexAuthorization, NativeCodexAuthError> {
        self.validate_preflight_target(&request)?;
        let lifecycle = self.lifecycle.lock();
        let _lifecycle = tokio::select! {
            guard = lifecycle => guard,
            () = request.cancellation().cancelled() => {
                return Err(NativeCodexAuthError::Transport(None));
            }
        };
        self.validate_preflight_target(&request)?;
        // Official Codex refreshes the access JWT before every authenticated
        // catalog or Responses dispatch. Catalog fetch already refreshes; an
        // expired generation must not reach inference preflight as Unavailable.
        self.refresh_if_needed_locked(request.deadline(), Some(request.cancellation()))
            .await?;
        self.validate_preflight_target(&request)?;
        let (expected_fence, expected_selection_epoch) = request
            .target()
            .native_codex_credential_selection()
            .map(|(fence, epoch)| (fence.clone(), epoch))
            .ok_or(NativeCodexAuthError::Unavailable)?;
        let current = self.load_fence().await?;
        if current != expected_fence {
            return Err(NativeCodexAuthError::Unavailable);
        }
        let lease = self.acquire_ready_lease(expected_fence.clone()).await?;
        let credential = parse_stored(lease.payload()).map_err(|error| match error {
            NativeCodexAuthError::Validation => NativeCodexAuthError::CatalogCredential,
            other => other,
        })?;
        let stored_epoch = credential_selection_epoch(
            request.target().account_binding(),
            request.target().credential_authority_binding(),
            &credential.selection_epoch,
        )?;
        if lease.fence() != expected_fence
            || stored_epoch != expected_selection_epoch
            || credential.account_id != expected_fence.account_binding().as_str()
            || credential.access_expires_at
                <= self
                    .clock
                    .now()
                    .seconds()
                    .saturating_add(ACCESS_REFRESH_SKEW)
            || validate_exact_scopes(&credential.scopes.join(" ")).is_err()
        {
            return Err(NativeCodexAuthError::Unavailable);
        }
        self.validate_preflight_target(&request)?;
        Ok(NativeCodexAuthorization {
            lease,
            expected_fence,
            expected_selection_epoch,
            credential,
            preflight: request,
            physical_request: None,
            headers_consumed: false,
        })
    }
}
