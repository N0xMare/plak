//! The shared manager core: admission, stored credentials and leases.

use super::{
    AuthBodyError, Bytes, CLAUDE_OAUTH_MESSAGES_ORIGIN, CLAUDE_OAUTH_TOKEN_PATH,
    ClaudeNativeAuthError, ClaudeOAuthTokenGrant, ClaudeSubscriptionStatus, CredentialFence,
    CredentialGeneration, Duration, ExchangeControl, HEADER_ACCEPT, HEADER_CONTENT_TYPE,
    HEADER_USER_AGENT, HeaderMap, HeaderValue, Instant, MAX_CLAUDE_SUBSCRIPTION_USAGE_BODY_BYTES,
    MAX_TOKEN_BYTES, ManagerCore, Method, Origin, REQUEST_TIMEOUT, Request, SensitiveBytes,
    StatusCode, StoredTokens, USER_AGENT, Zeroizing, credential_record_from_grant, network_failure,
    remaining_until, system_unix_seconds, validate_acceptance,
};
use plakous_auth::oauth_orchestrator::{is_json_media_type, pull_auth_body};

impl ManagerCore {
    pub(super) fn ensure_active(&self) -> Result<(), ClaudeNativeAuthError> {
        validate_acceptance(&self.acceptance, system_unix_seconds()?)
    }

    pub(super) fn flow_deadline(limit: Duration) -> Result<Instant, ClaudeNativeAuthError> {
        Instant::now()
            .checked_add(limit)
            .ok_or(ClaudeNativeAuthError::LoginExpired)
    }

    pub(super) fn ensure_flow_active(
        &self,
        deadline: Instant,
    ) -> Result<(), ClaudeNativeAuthError> {
        self.ensure_active()?;
        remaining_until(deadline, Instant::now()).ok_or(ClaudeNativeAuthError::LoginExpired)?;
        Ok(())
    }

    pub(super) fn persist_grant(
        &self,
        baseline: Option<&CredentialFence>,
        grant: &ClaudeOAuthTokenGrant,
    ) -> Result<ClaudeSubscriptionStatus, ClaudeNativeAuthError> {
        let generation = match baseline {
            Some(fence) => fence.generation().next()?,
            None => CredentialGeneration::new(1)?,
        };
        let record = credential_record_from_grant(generation, grant)?;
        let fence = if let Some(expected) = baseline {
            self.store.replace_selected_login(expected, record)?
        } else {
            self.store.initialize_selected(&record)?;
            record.fence()
        };
        Ok(ClaudeSubscriptionStatus {
            generation: fence.generation(),
            status: fence.status(),
        })
    }

    pub(super) async fn post_json(
        &self,
        body: secrecy::SecretString,
    ) -> Result<Zeroizing<Vec<u8>>, ClaudeNativeAuthError> {
        let request = self.json_request(body)?;
        let control = ExchangeControl::new(Instant::now() + REQUEST_TIMEOUT);
        let response = self
            .client
            .execute(request, control)
            .await
            .map_err(|failure| ClaudeNativeAuthError::Transport(network_failure(failure)))?;
        let status = response.status();
        let bytes = pull_auth_body(response.into_body(), MAX_TOKEN_BYTES)
            .await
            .map_err(|error| match error {
                AuthBodyError::Failure(failure) => {
                    ClaudeNativeAuthError::Transport(network_failure(failure))
                }
                AuthBodyError::TooLarge => ClaudeNativeAuthError::InvalidTokenResponse,
            })?;
        if !status.is_success() {
            return Err(ClaudeNativeAuthError::LoginTokenExchange);
        }
        Ok(bytes)
    }

    pub(super) fn json_request(
        &self,
        body: secrecy::SecretString,
    ) -> Result<Request, ClaudeNativeAuthError> {
        use secrecy::ExposeSecret as _;
        let mut headers = HeaderMap::new();
        headers.insert(
            HEADER_CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        headers.insert(HEADER_ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(HEADER_USER_AGENT, HeaderValue::from_static(USER_AGENT));
        Request::new(
            Origin::parse(&self.token_origin)?,
            Method::POST,
            CLAUDE_OAUTH_TOKEN_PATH,
            headers,
            SensitiveBytes::new(body.expose_secret().as_bytes().to_vec()).into_bytes(),
            self.client.limits(),
        )
        .map_err(Into::into)
    }

    pub(super) fn oauth_control_get(
        &self,
        path: &'static str,
        headers: HeaderMap,
    ) -> Result<Request, ClaudeNativeAuthError> {
        let origin = CLAUDE_OAUTH_MESSAGES_ORIGIN;
        #[cfg(test)]
        let origin = self.control_origin.as_deref().unwrap_or(origin);
        Request::new(
            Origin::parse(origin)?,
            Method::GET,
            path,
            headers,
            Bytes::new(),
            self.client.limits(),
        )
        .map_err(Into::into)
    }

    pub(super) async fn execute_usage(
        &self,
        request: Request,
    ) -> Result<(StatusCode, bool, Zeroizing<Vec<u8>>), AuthBodyError> {
        let control = ExchangeControl::new(Instant::now() + REQUEST_TIMEOUT);
        let response = self
            .client
            .execute(request, control)
            .await
            .map_err(AuthBodyError::Failure)?;
        let status = response.status();
        let is_json = response.headers().iter().any(|(name, value)| {
            *name == HEADER_CONTENT_TYPE && value.to_str().is_ok_and(is_json_media_type)
        });
        let bytes = pull_auth_body(
            response.into_body(),
            MAX_CLAUDE_SUBSCRIPTION_USAGE_BODY_BYTES,
        )
        .await?;
        Ok((status, is_json, bytes))
    }

    pub(super) fn read_stored(
        &self,
        fence: &CredentialFence,
    ) -> Result<StoredTokens, ClaudeNativeAuthError> {
        let lease = self.store.acquire_ready(fence)?;
        let stored: StoredTokens = serde_json::from_slice(lease.payload().expose_secret())
            .map_err(|_| ClaudeNativeAuthError::InvalidStoredCredential)?;
        stored.validate()?;
        if stored.grant_binding != fence.account_binding().as_str() {
            return Err(ClaudeNativeAuthError::InvalidStoredCredential);
        }
        Ok(stored)
    }
}
