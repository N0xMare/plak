//! Claimed refresh and revoke hooks and their cancellation and restore helpers.

use super::{
    AccountBinding, BuildRequestFailure, ClaimedHttpDisposition, ClaimedHttpError,
    ClaimedHttpResponse, ClaimedRefreshHooks, ClaimedRevokeHooks, CredentialFence,
    CredentialGeneration, CredentialRecord, CredentialRefreshDetail, CredentialRefreshEvent,
    CredentialRefreshFailureClass, CredentialRefreshFailureReason, CredentialRefreshObserver,
    CredentialRefreshPhase, CredentialStatus, ExchangeControl, Instant, MAX_AUTH_BODY_BYTES,
    MAX_STRING_BYTES, MAX_TOKEN_BYTES, ManagerCore, PrivateCredentialStore, PrivateStoreError,
    PullBodyError, REQUEST_TIMEOUT, RefreshClaim, Request, RevokeResponse, StoredTokens,
    SubscriptionRequestClass, SubscriptionRequestPermit, TokenResponse, XAI_PUBLIC_CLIENT_ID,
    XaiNativeAuthError, Zeroizing, claimed_response_is_json, credential_record, decode_json,
    network_failure, pull_claimed_body, refresh_token_form, retired_or_quarantined,
    route_credential_authority_id, system_unix_seconds, urlencoded_form,
};

/// Prepared refresh state carried through form-body claimed refresh.
pub(super) struct XaiRefreshPrepared {
    pub(super) expected_subject: AccountBinding,
    pub(super) refresh_token: Zeroizing<String>,
    pub(super) selection_epoch: String,
}

/// Shared-orchestrator hooks for SuperGrok claimed refresh (form body, ES256).
pub(super) struct XaiRefreshHooks<'a> {
    /// Declared first so a dropped refresh restores its claim before the
    /// in-process flight is released.
    pub(super) cancel_guard: XaiRefreshCancelGuard<'a>,
    pub(super) core: &'a ManagerCore,
    pub(super) expected_account: Option<&'a AccountBinding>,
    pub(super) expected_selection_epoch: Option<&'a str>,
    pub(super) expected_authority: Option<&'a str>,
    pub(super) _flight: futures_util::lock::MutexGuard<'a, ()>,
    pub(super) request_permit: Option<Box<dyn SubscriptionRequestPermit>>,
    pub(super) started: Instant,
    /// Refresh even when the stored expiry is outside the lead (HTTP 401).
    pub(super) force: bool,
    /// Numbers of the received token response, for failure diagnostics.
    pub(super) detail: CredentialRefreshDetail,
}

#[allow(
    clippy::unused_async_trait_impl,
    reason = "hook methods implement the orchestrator's async trait; some bodies finish without awaiting, and async keeps them lazy until the orchestrator polls"
)]
impl ClaimedRefreshHooks for XaiRefreshHooks<'_> {
    type Error = XaiNativeAuthError;
    type Prepared = XaiRefreshPrepared;
    type Output = ();

    async fn check_profile(&mut self) -> Result<(), Self::Error> {
        self.core.ensure_active()
    }

    async fn fence_needing_refresh(&mut self) -> Result<Option<CredentialFence>, Self::Error> {
        let fence = self
            .core
            .store
            .load()?
            .ok_or(XaiNativeAuthError::NotLoggedIn)?;
        if self
            .expected_account
            .is_some_and(|account| account != fence.account_binding())
        {
            return Err(XaiNativeAuthError::AccountChanged);
        }
        if fence.status() == CredentialStatus::Ready {
            self.core.validate_expected_selection(
                &fence,
                self.expected_selection_epoch,
                self.expected_authority,
            )?;
        }
        if !self.force && !self.core.needs_refresh(&fence)? {
            return Ok(None);
        }
        // Ready still pre-reads under a Ready lease. Unknown/RefreshRequired keep
        // the payload but cannot acquire_ready; epoch is checked from the claim
        // payload in build_refresh_request.
        match fence.status() {
            CredentialStatus::Ready => {
                let _ = self.core.read_stored(&fence)?;
            }
            CredentialStatus::RefreshRequired | CredentialStatus::RefreshOutcomeUnknown => {}
            _ => return Err(XaiNativeAuthError::NotLoggedIn),
        }
        self.request_permit = self
            .core
            .request_gate
            .as_ref()
            .map(|gate| gate.acquire(SubscriptionRequestClass::AuthRefresh))
            .transpose()
            .map_err(|_| XaiNativeAuthError::NotReady)?;
        Ok(Some(fence))
    }

    fn fresh_output(&mut self) -> Result<Self::Output, Self::Error> {
        Ok(())
    }

    fn build_refresh_request(
        &mut self,
        claim: &RefreshClaim,
    ) -> Result<(Self::Prepared, Request), BuildRequestFailure<Self::Error>> {
        self.cancel_guard.claimed(claim.fence());
        let mut old: StoredTokens = match serde_json::from_slice(claim.payload().expose_secret()) {
            Ok(old) => old,
            Err(_) => {
                return Err(BuildRequestFailure::LeaveQuarantined(
                    XaiNativeAuthError::InvalidStoredCredential,
                ));
            }
        };
        if let Err(error) = old.validate(claim.fence().account_binding()) {
            return Err(retired_or_quarantined(error));
        }
        if let Some(expected_epoch) = self.expected_selection_epoch
            && (old.selection_epoch != expected_epoch
                || self.expected_authority
                    != Some(route_credential_authority_id(&self.core.store).as_str()))
        {
            return Err(BuildRequestFailure::LeaveQuarantined(
                XaiNativeAuthError::SelectionChanged,
            ));
        }
        let expected_subject = claim.fence().account_binding().clone();
        let refresh_token = Zeroizing::new(std::mem::take(&mut old.refresh_token));
        let selection_epoch = old.selection_epoch.clone();
        let form = urlencoded_form(&refresh_token_form(&refresh_token, XAI_PUBLIC_CLIENT_ID));
        let request = match self.core.form_request(self.core.endpoints.token(), form) {
            Ok(request) => request,
            Err(error) => return Err(BuildRequestFailure::Restore(error)),
        };
        Ok((
            XaiRefreshPrepared {
                expected_subject,
                refresh_token,
                selection_epoch,
            },
            request,
        ))
    }

    async fn authorize_dispatch(&mut self) -> Result<(), Self::Error> {
        if let Some(permit) = &mut self.request_permit
            && permit.authorize_dispatch().is_err()
        {
            return Err(XaiNativeAuthError::NotReady);
        }
        Ok(())
    }

    fn exchange_control(&mut self) -> ExchangeControl {
        ExchangeControl::new(Instant::now() + REQUEST_TIMEOUT)
    }

    async fn validate_tokens(
        &mut self,
        prepared: Self::Prepared,
        response: &mut ClaimedHttpResponse,
    ) -> Result<CredentialRecord, Self::Error> {
        let status = response.status();
        if status.is_success() {
            let response_body = pull_claimed_body(response, MAX_TOKEN_BYTES)
                .await
                .map_err(map_xai_pull_body_error)?;
            let tokens: TokenResponse = decode_json(&response_body, MAX_TOKEN_BYTES)?;
            self.detail = CredentialRefreshDetail::from_response(
                system_unix_seconds().unwrap_or(0),
                tokens.expires_in,
                tokens.id_token.as_deref(),
                MAX_STRING_BYTES,
            );
            let stored = self
                .core
                .validate_tokens(
                    tokens,
                    None,
                    Some(&prepared.expected_subject),
                    Some(&prepared.refresh_token),
                    Some(&prepared.selection_epoch),
                    None,
                )
                .await?;
            let replacement_generation = response
                .refresh_fence()
                .generation()
                .next()
                .map_err(XaiNativeAuthError::from)?;
            return credential_record(prepared.expected_subject, replacement_generation, &stored);
        }
        if status.as_u16() == 408 || status.as_u16() == 429 || status.is_server_error() {
            return Err(XaiNativeAuthError::Transport(None));
        }
        Err(XaiNativeAuthError::RefreshQuarantined)
    }

    async fn recheck_profile_before_complete(&mut self) -> Result<(), Self::Error> {
        self.core.ensure_active()
    }

    async fn map_claimed_http_error(
        &mut self,
        store: &PrivateCredentialStore,
        error: ClaimedHttpError,
    ) -> Self::Error {
        map_xai_claimed_http_error(store, error)
    }

    fn map_store_error(&mut self, error: PrivateStoreError) -> Self::Error {
        // Complete failures historically surfaced as RefreshQuarantined; begin
        // and lease failures keep the Store(…) surface.
        match error {
            PrivateStoreError::HttpResponseBodyNotComplete
            | PrivateStoreError::GenerationNotSequential
            | PrivateStoreError::GenerationExhausted
            | PrivateStoreError::CredentialNotReady
            | PrivateStoreError::ForeignAccount
            | PrivateStoreError::InvalidRecord => XaiNativeAuthError::RefreshQuarantined,
            other => XaiNativeAuthError::Store(other),
        }
    }

    fn output_from_completed(
        &mut self,
        _fence: CredentialFence,
    ) -> Result<Self::Output, Self::Error> {
        Ok(())
    }

    fn refresh_observer(&self) -> Option<&dyn CredentialRefreshObserver> {
        self.core.refresh_observer.as_deref()
    }

    fn reclaim_unknown_refresh(&self) -> bool {
        true
    }

    fn failure_detail(&mut self, error: &Self::Error) -> CredentialRefreshDetail {
        self.detail.with_reason(xai_failure_reason(error))
    }

    fn after_post_start_failure(&mut self, store: &PrivateCredentialStore, error: &Self::Error) {
        if !xai_refresh_failure_is_retryable(error) {
            return;
        }
        let detail = self.failure_detail(error);
        let failure_class = match error {
            XaiNativeAuthError::Transport(Some(cause)) if cause.timed_out => {
                CredentialRefreshFailureClass::Timeout
            }
            XaiNativeAuthError::Store(_) => CredentialRefreshFailureClass::Store,
            XaiNativeAuthError::NotReady => CredentialRefreshFailureClass::NotReady,
            XaiNativeAuthError::InvalidTokenResponse(_) | XaiNativeAuthError::InvalidIdToken(_) => {
                CredentialRefreshFailureClass::Validation
            }
            _ => CredentialRefreshFailureClass::Transport,
        };
        let Some(claimed) = self.cancel_guard.claimed.as_ref() else {
            return;
        };
        restore_xai_unknown_to_ready(
            store,
            claimed,
            self.core.refresh_observer.as_deref(),
            failure_class,
            self.started,
            match error {
                XaiNativeAuthError::Transport(Some(cause)) => Some(http_phase(cause.phase)),
                _ => None,
            },
            detail,
        );
    }
}

/// Content-free sub-reason of an xAI refresh failure, when it names a check.
pub(super) fn xai_failure_reason(
    error: &XaiNativeAuthError,
) -> Option<CredentialRefreshFailureReason> {
    match error {
        XaiNativeAuthError::InvalidTokenResponse(reason)
        | XaiNativeAuthError::InvalidIdToken(reason) => Some(*reason),
        XaiNativeAuthError::UnknownSigningKey => {
            Some(CredentialRefreshFailureReason::IdTokenUnknownKid)
        }
        XaiNativeAuthError::InvalidJwks => Some(CredentialRefreshFailureReason::IdTokenSigningKey),
        XaiNativeAuthError::AccountChanged => Some(CredentialRefreshFailureReason::SubjectMismatch),
        XaiNativeAuthError::SelectionChanged => {
            Some(CredentialRefreshFailureReason::SelectionChanged)
        }
        XaiNativeAuthError::StoredCredentialFormatRetired => {
            Some(CredentialRefreshFailureReason::RetiredFormat)
        }
        XaiNativeAuthError::InvalidStoredCredential => {
            Some(CredentialRefreshFailureReason::StoredCredential)
        }
        XaiNativeAuthError::RefreshQuarantined | XaiNativeAuthError::HttpStatus(_) => {
            Some(CredentialRefreshFailureReason::ProviderRejected)
        }
        _ => None,
    }
}

/// Shared-orchestrator hooks for SuperGrok claimed remote revoke (form body).
pub(super) struct XaiRevokeHooks<'a> {
    pub(super) core: &'a ManagerCore,
    pub(super) _flight: futures_util::lock::MutexGuard<'a, ()>,
}

#[allow(
    clippy::unused_async_trait_impl,
    reason = "hook methods implement the orchestrator's async trait; some bodies finish without awaiting, and async keeps them lazy until the orchestrator polls"
)]
impl ClaimedRevokeHooks for XaiRevokeHooks<'_> {
    type Error = XaiNativeAuthError;
    type Output = CredentialGeneration;

    async fn check_profile(&mut self) -> Result<(), Self::Error> {
        self.core.ensure_active()
    }

    async fn fence_to_revoke(&mut self) -> Result<CredentialFence, Self::Error> {
        self.core
            .store
            .load()?
            .ok_or(XaiNativeAuthError::NotLoggedIn)
    }

    fn build_revoke_request(
        &mut self,
        claim: &RefreshClaim,
    ) -> Result<Request, BuildRequestFailure<Self::Error>> {
        let mut old: StoredTokens = match serde_json::from_slice(claim.payload().expose_secret()) {
            Ok(old) => old,
            Err(_) => {
                return Err(BuildRequestFailure::LeaveQuarantined(
                    XaiNativeAuthError::InvalidStoredCredential,
                ));
            }
        };
        if let Err(error) = old.validate(claim.fence().account_binding()) {
            return Err(retired_or_quarantined(error));
        }
        let refresh = Zeroizing::new(std::mem::take(&mut old.refresh_token));
        let form = urlencoded_form(&[
            ("token", &refresh),
            ("token_type_hint", "refresh_token"),
            ("client_id", XAI_PUBLIC_CLIENT_ID),
        ]);
        self.core
            .form_request(self.core.endpoints.revocation(), form)
            .map_err(BuildRequestFailure::Restore)
    }

    fn exchange_control(&mut self) -> ExchangeControl {
        ExchangeControl::new(Instant::now() + REQUEST_TIMEOUT)
    }

    async fn accept_revoke_response(
        &mut self,
        response: &mut ClaimedHttpResponse,
    ) -> Result<(), Self::Error> {
        if !response.status().is_success() || !claimed_response_is_json(response) {
            return Err(XaiNativeAuthError::RefreshQuarantined);
        }
        let response_body = pull_claimed_body(response, MAX_AUTH_BODY_BYTES)
            .await
            .map_err(map_xai_pull_body_error)?;
        if !response_body.is_empty()
            && decode_json::<RevokeResponse>(&response_body, MAX_AUTH_BODY_BYTES).is_err()
        {
            return Err(XaiNativeAuthError::RefreshQuarantined);
        }
        Ok(())
    }

    async fn recheck_profile_before_complete(&mut self) -> Result<(), Self::Error> {
        self.core.ensure_active()
    }

    async fn map_claimed_http_error(
        &mut self,
        store: &PrivateCredentialStore,
        error: ClaimedHttpError,
    ) -> Self::Error {
        map_xai_claimed_http_error(store, error)
    }

    fn map_store_error(&mut self, error: PrivateStoreError) -> Self::Error {
        match error {
            PrivateStoreError::CredentialQuarantined
            | PrivateStoreError::CredentialNotReady
            | PrivateStoreError::CredentialRevoked
            | PrivateStoreError::RecordMissing
            | PrivateStoreError::FenceMismatch
            | PrivateStoreError::StoreBusy
            | PrivateStoreError::LockFailed => XaiNativeAuthError::Store(error),
            _ => XaiNativeAuthError::RefreshQuarantined,
        }
    }

    fn output_from_revoked(
        &mut self,
        generation: CredentialGeneration,
    ) -> Result<Self::Output, Self::Error> {
        Ok(generation)
    }

    fn observer(&self) -> Option<&dyn CredentialRefreshObserver> {
        self.core.refresh_observer.as_deref()
    }
}

pub(super) fn map_xai_claimed_http_error(
    _store: &PrivateCredentialStore,
    error: ClaimedHttpError,
) -> XaiNativeAuthError {
    match error.disposition() {
        ClaimedHttpDisposition::RestoredNotStarted
        | ClaimedHttpDisposition::QuarantinedMayHaveTransmitted => {
            XaiNativeAuthError::Transport(error.transport_failure().and_then(network_failure))
        }
        ClaimedHttpDisposition::RestorationFailed | ClaimedHttpDisposition::ClaimRejected => {
            XaiNativeAuthError::RefreshQuarantined
        }
        ClaimedHttpDisposition::QuarantineVerificationFailed => {
            XaiNativeAuthError::RefreshQuarantined
        }
        _ => XaiNativeAuthError::RefreshQuarantined,
    }
}

pub(super) fn xai_refresh_failure_is_retryable(error: &XaiNativeAuthError) -> bool {
    matches!(
        error,
        XaiNativeAuthError::Transport(_)
            | XaiNativeAuthError::InvalidTokenResponse(_)
            | XaiNativeAuthError::InvalidIdToken(_)
            | XaiNativeAuthError::NotReady
            // Local lock contention and lock-identity failures prove nothing
            // about the provider's view of the grant.
            | XaiNativeAuthError::Store(
                PrivateStoreError::StoreBusy | PrivateStoreError::LockFailed
            )
    )
}

pub(super) fn http_phase(phase: plakous_model::ModelRetryPhase) -> plakous_http::Phase {
    use plakous_http::Phase as H;
    use plakous_model::ModelRetryPhase as M;
    match phase {
        M::Resolution => H::Resolution,
        M::Connect => H::Connect,
        M::Tls => H::Tls,
        M::RequestWrite => H::RequestWrite,
        M::RequestWriteOrResponseHead => H::RequestWriteOrResponseHead,
        M::ResponseHead => H::ResponseHead,
        M::ResponseBody => H::ResponseBody,
        M::Cleanup => H::Cleanup,
    }
}

/// Restores this refresh's own claimed Unknown record to Ready on the same
/// refresh token and reports the restoration to the credential-lifecycle
/// observer, so `plak doctor`'s latest refresh row reflects the real last
/// transition. A peer that reclaimed the record since owns it: the fence
/// mismatch leaves it alone.
pub(super) fn restore_xai_unknown_to_ready(
    store: &PrivateCredentialStore,
    claimed: &CredentialFence,
    observer: Option<&dyn CredentialRefreshObserver>,
    failure_class: CredentialRefreshFailureClass,
    started: Instant,
    transport_phase: Option<plakous_http::Phase>,
    detail: CredentialRefreshDetail,
) {
    if store.restore_unknown_to_ready(claimed).is_err() {
        return;
    }
    let generation = claimed.generation().get();
    if let Some(observer) = observer {
        observer.observe(CredentialRefreshEvent {
            phase: CredentialRefreshPhase::Restored,
            failure_class: Some(failure_class),
            transport_phase,
            http_status: None,
            elapsed: started.elapsed(),
            generation,
            detail,
        });
    }
}

/// Restores this refresh's own claimed Unknown record when the xAI refresh
/// future is dropped without finishing (inference cancellation, before or
/// after the POST). It does nothing until `begin_refresh` has written the
/// claim, restores only that exact fence, never a peer's, and never blocks:
/// on store contention it leaves Unknown, which the next xAI refresh
/// reclaims. Every completed ladder exit calls [`Self::finished`] first so a
/// terminal Unknown stays put.
pub(super) struct XaiRefreshCancelGuard<'a> {
    pub(super) store: &'a PrivateCredentialStore,
    pub(super) observer: Option<&'a dyn CredentialRefreshObserver>,
    pub(super) started: Instant,
    pub(super) claimed: Option<CredentialFence>,
}

impl<'a> XaiRefreshCancelGuard<'a> {
    pub(super) fn new(
        store: &'a PrivateCredentialStore,
        observer: Option<&'a dyn CredentialRefreshObserver>,
    ) -> Self {
        Self {
            store,
            observer,
            started: Instant::now(),
            claimed: None,
        }
    }

    pub(super) fn claimed(&mut self, unknown: CredentialFence) {
        self.claimed = Some(unknown);
    }

    pub(super) fn finished(&mut self) {
        self.claimed = None;
    }
}

impl Drop for XaiRefreshCancelGuard<'_> {
    fn drop(&mut self) {
        let Some(claimed) = self.claimed.take() else {
            return;
        };
        let Ok(Some(_)) = self.store.try_restore_unknown_to_ready(&claimed) else {
            return;
        };
        if let Some(observer) = self.observer {
            observer.observe(CredentialRefreshEvent {
                phase: CredentialRefreshPhase::Restored,
                failure_class: Some(CredentialRefreshFailureClass::Cancelled),
                transport_phase: None,
                http_status: None,
                elapsed: self.started.elapsed(),
                generation: claimed.generation().get(),
                detail: CredentialRefreshDetail::default(),
            });
        }
    }
}

pub(super) fn map_xai_pull_body_error(error: PullBodyError) -> XaiNativeAuthError {
    match error {
        PullBodyError::Transport => XaiNativeAuthError::Transport(None),
        PullBodyError::TooLarge => XaiNativeAuthError::InvalidTokenResponse(
            CredentialRefreshFailureReason::ResponseTooLarge,
        ),
        _ => XaiNativeAuthError::Transport(None),
    }
}
