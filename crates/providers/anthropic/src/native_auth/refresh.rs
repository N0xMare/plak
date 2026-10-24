//! Claimed refresh hooks for the Claude OAuth token endpoint.

use super::{
    Arc, BuildRequestFailure, CLAUDE_OAUTH_INFERENCE_SCOPE, ClaimedHttpDisposition,
    ClaimedHttpError, ClaimedHttpResponse, ClaimedRefreshHooks, ClaudeNativeAuthError,
    CredentialFence, CredentialRecord, CredentialRefreshDetail, CredentialRefreshFailureReason,
    CredentialRefreshObserver, CredentialStatus, ExchangeControl, Instant, MAX_TOKEN_BYTES,
    ManagerCore, PrivateCredentialStore, PrivateStoreError, REFRESH_SKEW_SECONDS, REQUEST_TIMEOUT,
    RefreshClaim, Request, StoredTokens, SubscriptionRequestGate, SubscriptionRequestPermit,
    Zeroizing, access_needs_refresh, acquire_refresh_permit, claude_oauth_scope_admits_inference,
    claude_token_refresh_body, credential_record, map_pull_body_error, network_failure,
    parse_claude_oauth_token_grant, pull_claimed_body, stored_tokens, system_unix_seconds,
};

pub(super) struct ClaudeRefreshHooks<'a> {
    pub(super) core: &'a ManagerCore,
    pub(super) request_gate: Option<&'a Arc<dyn SubscriptionRequestGate>>,
    pub(super) observer: Option<&'a dyn CredentialRefreshObserver>,
    pub(super) _flight: futures_util::lock::MutexGuard<'a, ()>,
    pub(super) request_permit: Option<Box<dyn SubscriptionRequestPermit>>,
    /// Refresh even when the stored expiry is outside the skew (HTTP 401).
    pub(super) force: bool,
    /// Numbers and rejecting check of the received response, for diagnostics.
    pub(super) detail: CredentialRefreshDetail,
}

pub(super) struct ClaudeRefreshPrepared {
    pub(super) account_uuid: Option<crate::ClaudeAccountUuid>,
    pub(super) refresh_token: Zeroizing<String>,
}

#[allow(
    clippy::unused_async_trait_impl,
    reason = "hook methods implement the orchestrator's async trait; some bodies finish without awaiting, and async keeps them lazy until the orchestrator polls"
)]
impl ClaimedRefreshHooks for ClaudeRefreshHooks<'_> {
    type Error = ClaudeNativeAuthError;
    type Prepared = ClaudeRefreshPrepared;
    type Output = ();

    async fn check_profile(&mut self) -> Result<(), Self::Error> {
        self.core.ensure_active()
    }

    async fn fence_needing_refresh(&mut self) -> Result<Option<CredentialFence>, Self::Error> {
        let fence = self
            .core
            .store
            .load()?
            .ok_or(ClaudeNativeAuthError::NotLoggedIn)?;
        if fence.status() == CredentialStatus::RefreshRequired {
            self.request_permit = acquire_refresh_permit(self.request_gate)?;
            return Ok(Some(fence));
        }
        if fence.status() != CredentialStatus::Ready {
            return Err(ClaudeNativeAuthError::NotReady);
        }
        let stored = self.core.read_stored(&fence)?;
        if self.force
            || access_needs_refresh(
                stored.expires_at,
                system_unix_seconds()?,
                REFRESH_SKEW_SECONDS,
            )
        {
            self.request_permit = acquire_refresh_permit(self.request_gate)?;
            Ok(Some(fence))
        } else {
            Ok(None)
        }
    }

    fn fresh_output(&mut self) -> Result<Self::Output, Self::Error> {
        Ok(())
    }

    fn build_refresh_request(
        &mut self,
        claim: &RefreshClaim,
    ) -> Result<(Self::Prepared, Request), BuildRequestFailure<Self::Error>> {
        let mut old: StoredTokens = match serde_json::from_slice(claim.payload().expose_secret()) {
            Ok(old) => old,
            Err(_) => {
                return Err(BuildRequestFailure::LeaveQuarantined(
                    ClaudeNativeAuthError::InvalidStoredCredential,
                ));
            }
        };
        if let Err(error) = old.validate() {
            return Err(BuildRequestFailure::LeaveQuarantined(error));
        }
        if old.grant_binding != claim.fence().account_binding().as_str() {
            return Err(BuildRequestFailure::LeaveQuarantined(
                ClaudeNativeAuthError::InvalidStoredCredential,
            ));
        }
        let refresh_token = Zeroizing::new(std::mem::take(&mut old.refresh_token));
        let request = match self
            .core
            .json_request(claude_token_refresh_body(&refresh_token))
        {
            Ok(request) => request,
            Err(error) => return Err(BuildRequestFailure::Restore(error)),
        };
        Ok((
            ClaudeRefreshPrepared {
                refresh_token,
                account_uuid: old.account_uuid.clone(),
            },
            request,
        ))
    }

    async fn authorize_dispatch(&mut self) -> Result<(), Self::Error> {
        if let Some(permit) = &mut self.request_permit
            && permit.authorize_dispatch().is_err()
        {
            return Err(ClaudeNativeAuthError::NotReady);
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
        let mut reject = |reason| {
            self.detail.reason = Some(reason);
            ClaudeNativeAuthError::RefreshQuarantined
        };
        if !response.status().is_success() {
            return Err(reject(CredentialRefreshFailureReason::ProviderRejected));
        }
        let body = pull_claimed_body(response, MAX_TOKEN_BYTES)
            .await
            .map_err(map_pull_body_error)?;
        let grant = parse_claude_oauth_token_grant(&body)
            .map_err(|_| reject(CredentialRefreshFailureReason::TokenShape))?;
        self.detail = CredentialRefreshDetail::from_response(
            system_unix_seconds().unwrap_or(0),
            u64::try_from(grant.expires_in()).ok(),
            None,
            MAX_TOKEN_BYTES,
        );
        if grant
            .scope()
            .is_none_or(|scope| !claude_oauth_scope_admits_inference(scope))
        {
            self.detail.reason = Some(CredentialRefreshFailureReason::TokenShape);
            return Err(ClaudeNativeAuthError::RefreshQuarantined);
        }
        if let (Some(old), Some(new)) = (&prepared.account_uuid, grant.account_uuid())
            && old != new
        {
            self.detail.reason = Some(CredentialRefreshFailureReason::SubjectMismatch);
            return Err(ClaudeNativeAuthError::RefreshQuarantined);
        }
        let account_uuid = grant.account_uuid().cloned().or(prepared.account_uuid);
        let refresh = grant
            .refresh_token()
            .unwrap_or(prepared.refresh_token.as_str());
        let stored = stored_tokens(
            grant.access_token(),
            refresh,
            grant.expires_in(),
            grant.scope().unwrap_or(CLAUDE_OAUTH_INFERENCE_SCOPE),
            response.refresh_fence().account_binding().as_str(),
            account_uuid,
        )?;
        let generation = response
            .refresh_fence()
            .generation()
            .next()
            .map_err(ClaudeNativeAuthError::from)?;
        credential_record(generation, &stored)
    }

    async fn map_claimed_http_error(
        &mut self,
        _store: &PrivateCredentialStore,
        error: ClaimedHttpError,
    ) -> Self::Error {
        match error.disposition() {
            ClaimedHttpDisposition::RestoredNotStarted => ClaudeNativeAuthError::Transport(
                error.transport_failure().and_then(network_failure),
            ),
            ClaimedHttpDisposition::RestorationFailed | ClaimedHttpDisposition::ClaimRejected => {
                ClaudeNativeAuthError::RefreshQuarantined
            }
            _ => ClaudeNativeAuthError::RefreshQuarantined,
        }
    }

    fn map_store_error(&mut self, error: PrivateStoreError) -> Self::Error {
        match error {
            PrivateStoreError::HttpResponseBodyNotComplete
            | PrivateStoreError::GenerationNotSequential
            | PrivateStoreError::GenerationExhausted
            | PrivateStoreError::CredentialNotReady
            | PrivateStoreError::ForeignAccount
            | PrivateStoreError::InvalidRecord => ClaudeNativeAuthError::RefreshQuarantined,
            other => ClaudeNativeAuthError::Store(other),
        }
    }

    fn output_from_completed(
        &mut self,
        _fence: CredentialFence,
    ) -> Result<Self::Output, Self::Error> {
        Ok(())
    }

    fn refresh_observer(&self) -> Option<&dyn CredentialRefreshObserver> {
        self.observer
    }

    fn failure_detail(&mut self, error: &Self::Error) -> CredentialRefreshDetail {
        let fallback = match error {
            ClaudeNativeAuthError::InvalidTokenResponse => {
                Some(CredentialRefreshFailureReason::TokenShape)
            }
            ClaudeNativeAuthError::InvalidStoredCredential => {
                Some(CredentialRefreshFailureReason::StoredCredential)
            }
            ClaudeNativeAuthError::StoredCredentialFormatRetired => {
                Some(CredentialRefreshFailureReason::RetiredFormat)
            }
            _ => None,
        };
        let reason = self.detail.reason.or(fallback);
        self.detail.with_reason(reason)
    }
}
