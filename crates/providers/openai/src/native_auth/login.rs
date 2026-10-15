//! Browser, remote-browser and device login flows and their loopback hooks.

use super::{
    AUTH_REQUEST_CAP, AccountBinding, Arc, AuthSecretString, AuthorizeUrlRequest,
    BROWSER_CALLBACK_PATH, BROWSER_DEADLINE, BrowserLoopbackHooks, CODEX_OAUTH,
    CallbackQueryPolicy, CodexCredentialStatus, CodexSubscriptionCredentialManager,
    CredentialFence, CredentialGeneration, DEVICE_DEADLINE, DEVICE_REDIRECT_PATH,
    DEVICE_TOKEN_PATH, DEVICE_USER_CODE_PATH, DEVICE_VERIFICATION_PATH, Deserialize,
    DevicePollSchedule, Duration, Instant, LoopbackCallbackOutcome, LoopbackCallbackPage,
    LoopbackConnectionEvent, MAX_CALLBACK_BYTES, MAX_DEVICE_INTERVAL_SECONDS,
    MAX_OAUTH_VALUE_BYTES, Method, NativeCodexAuthError, OAuthCallbackOutcome,
    OAuthLoginTokenResponse, OPENAI_CODEX_NATIVE_CLIENT_ID, OPENAI_CODEX_OAUTH_SCOPES,
    REMOTE_BROWSER_REDIRECT_URI, Serialize, StatusCode, StoredCredential, TOKEN_PATH, TcpListener,
    Zeroizing, authorization_code_token_form, bind_loopback_listener, build_authorization_url,
    capped_login_deadline, constant_time_value_eq, endpoint_url, execute_bounded, fmt, form_body,
    form_headers, generate_oauth_correlation_token, json_headers, map_oauth_workflow_error,
    now_unix_secs, parse_http11_get_request, parse_oauth_callback_query,
    parse_remote_authorization_input, parse_strict_json, remaining_login_time,
    require_json_media_type, require_status, run_blocking, run_browser_loopback_session,
    stored_record, validate_oauth_value, validate_profile_at, validate_user_code,
};
#[cfg(test)]
use super::{CALLBACK_RESPONSE_CAP, format_http11_text_response, write_all_before_deadline};
use zeroize::Zeroize as _;

/// Non-cloneable browser login handle. Diagnostics redact URL correlation data.
pub struct CodexBrowserLogin {
    pub(super) listener: TcpListener,
    pub(super) attempt: BrowserLoginAttempt,
}

pub(super) struct BrowserLoginAttempt {
    pub(super) authorization_url: Zeroizing<String>,
    pub(super) redirect_uri: String,
    pub(super) credential_baseline: Option<CredentialFence>,
    pub(super) state: AuthSecretString,
    pub(super) pkce: plakous_auth::PkceS256,
    pub(super) deadline: Instant,
}

impl CodexBrowserLogin {
    /// Returns the exact provider URL the application may open for the operator.
    #[must_use]
    pub fn authorization_url(&self) -> &str {
        &self.attempt.authorization_url
    }
}

impl fmt::Debug for CodexBrowserLogin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CodexBrowserLogin(<redacted one-use login>)")
    }
}

/// Non-cloneable browser login handle for a callback pasted from another machine.
/// Diagnostics redact URL correlation data.
pub struct CodexRemoteBrowserLogin {
    pub(super) attempt: BrowserLoginAttempt,
}

impl CodexRemoteBrowserLogin {
    /// Returns the exact provider URL the application may show to the operator.
    #[must_use]
    pub fn authorization_url(&self) -> &str {
        &self.attempt.authorization_url
    }
}

impl fmt::Debug for CodexRemoteBrowserLogin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CodexRemoteBrowserLogin(<redacted one-use login>)")
    }
}

/// Non-cloneable pinned custom-device-flow login handle.
pub struct CodexDeviceLogin {
    pub(super) verification_url: String,
    pub(super) user_code: AuthSecretString,
    pub(super) device_auth_id: AuthSecretString,
    pub(super) interval: Duration,
    pub(super) deadline: Instant,
    pub(super) credential_baseline: Option<CredentialFence>,
}

impl CodexDeviceLogin {
    /// Returns the exact verification origin/path.
    #[must_use]
    pub fn verification_url(&self) -> &str {
        &self.verification_url
    }

    /// Exposes the short-lived operator code only to the explicit login UI.
    #[must_use]
    pub fn user_code(&self) -> &str {
        self.user_code.expose_secret()
    }
}

impl fmt::Debug for CodexDeviceLogin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CodexDeviceLogin(<redacted one-use login>)")
    }
}

impl CodexSubscriptionCredentialManager {
    pub(super) fn new_browser_login_attempt(
        &self,
        redirect_uri: String,
    ) -> Result<BrowserLoginAttempt, NativeCodexAuthError> {
        self.validate_profile_admission()?;
        let credential_baseline = self.store.load().map_err(|_| NativeCodexAuthError::Store)?;
        let state =
            generate_oauth_correlation_token().map_err(|_| NativeCodexAuthError::Binding)?;
        let pkce = plakous_auth::PkceS256::generate().map_err(|_| NativeCodexAuthError::Binding)?;
        // Dialect-owned extras stay local; shared builder owns the PKCE core.
        // Manager endpoint origin stays authoritative (including test doubles).
        let authorization_url = build_authorization_url(AuthorizeUrlRequest {
            pins: CODEX_OAUTH,
            auth_origin: Some(self.endpoints.auth_origin.as_str()),
            redirect_uri: &redirect_uri,
            code_challenge: pkce.challenge().as_str(),
            state: state.expose_secret(),
            nonce: None,
            extra_pairs: &[
                ("id_token_add_organizations", "true"),
                ("codex_cli_simplified_flow", "true"),
                ("originator", "plak"),
            ],
        })
        .map_err(|_| NativeCodexAuthError::Binding)?;
        Ok(BrowserLoginAttempt {
            authorization_url,
            redirect_uri,
            credential_baseline,
            state,
            pkce,
            deadline: Instant::now() + BROWSER_DEADLINE,
        })
    }

    /// Starts an exact loopback authorization-code-with-PKCE login.
    ///
    /// The returned handle owns the listener, state, verifier, and deadline and
    /// can be completed only once by consuming it.
    ///
    /// # Errors
    ///
    /// Rejects stale profile acceptance, unavailable pinned loopback ports, or
    /// failure to create strong state and PKCE material.
    pub async fn begin_browser_login(&self) -> Result<CodexBrowserLogin, NativeCodexAuthError> {
        self.validate_profile_admission()?;
        let (listener, port) = bind_loopback_listener(CODEX_OAUTH.loopback_ports)
            .await
            .map_err(|_| NativeCodexAuthError::Binding)?;
        if !CODEX_OAUTH.allows_loopback_port(port) {
            return Err(NativeCodexAuthError::Binding);
        }
        let redirect_uri = format!("http://localhost:{port}{BROWSER_CALLBACK_PATH}");
        Ok(CodexBrowserLogin {
            listener,
            attempt: self.new_browser_login_attempt(redirect_uri)?,
        })
    }

    /// Starts an authorization-code-with-PKCE login whose final redirect URL
    /// can be pasted back from a browser running on another machine.
    ///
    /// This does not bind or listen on a local socket. The provider redirect is
    /// pinned to `http://localhost:1455/auth/callback` for exact manual review.
    ///
    /// # Errors
    ///
    /// Rejects stale profile acceptance or failure to create strong state and
    /// PKCE material.
    pub fn begin_remote_browser_login(
        &self,
    ) -> Result<CodexRemoteBrowserLogin, NativeCodexAuthError> {
        Ok(CodexRemoteBrowserLogin {
            attempt: self.new_browser_login_attempt(REMOTE_BROWSER_REDIRECT_URI.to_owned())?,
        })
    }

    /// Completes one exact browser callback, validates OIDC and `UserInfo`, then
    /// atomically installs the selected account generation.
    ///
    /// # Errors
    ///
    /// Rejects an expired or inexact callback and any transport, token,
    /// identity, account-continuity, or persistence failure.
    pub async fn complete_browser_login(
        &self,
        login: CodexBrowserLogin,
    ) -> Result<CodexCredentialStatus, NativeCodexAuthError> {
        self.validate_profile_admission()?;
        let mut hooks = CodexBrowserLoopbackHooks {
            manager: self,
            deadline: login.attempt.deadline,
            attempt: Some(login.attempt),
        };
        run_browser_loopback_session(login.listener, &mut hooks).await
    }

    /// Completes one exact pasted browser redirect, validates OIDC and
    /// `UserInfo`, then atomically installs the selected account generation.
    ///
    /// # Errors
    ///
    /// Rejects an expired handle; an oversized or inexact redirect URL; an
    /// invalid bare authorization code; invalid callback query grammar or
    /// state; and any transport, token, identity, account-continuity, or
    /// persistence failure.
    pub async fn complete_remote_browser_login(
        &self,
        login: CodexRemoteBrowserLogin,
        authorization_input: &str,
    ) -> Result<CodexCredentialStatus, NativeCodexAuthError> {
        self.validate_profile_admission()?;
        let callback = parse_remote_authorization_input(
            authorization_input,
            login.attempt.state.expose_secret(),
            REMOTE_BROWSER_REDIRECT_URI,
            codex_callback_policy(),
        )
        .map_err(map_oauth_workflow_error)?;
        self.complete_browser_callback(login.attempt, callback)
            .await
    }

    pub(super) async fn complete_browser_callback(
        &self,
        login: BrowserLoginAttempt,
        callback: OAuthCallbackOutcome,
    ) -> Result<CodexCredentialStatus, NativeCodexAuthError> {
        remaining_login_time(login.deadline, NativeCodexAuthError::Callback)?;
        let code = match callback {
            OAuthCallbackOutcome::AuthorizationCode { code, state } => {
                if !constant_time_value_eq(login.state.expose_secret(), state.as_str()) {
                    return Err(NativeCodexAuthError::Callback);
                }
                code
            }
            OAuthCallbackOutcome::ProviderError { state } => {
                if !constant_time_value_eq(login.state.expose_secret(), state.as_str()) {
                    return Err(NativeCodexAuthError::Callback);
                }
                return Err(NativeCodexAuthError::Provider);
            }
        };
        let remaining = remaining_login_time(login.deadline, NativeCodexAuthError::Callback)?;
        let _lifecycle = tokio::time::timeout(remaining, self.lifecycle.lock())
            .await
            .map_err(|_| NativeCodexAuthError::Callback)?;
        self.validate_profile_admission()?;
        let credential = self
            .exchange_authorization_code(
                &code,
                &login.redirect_uri,
                login.pkce.verifier().expose_secret(),
                None,
                login.deadline,
            )
            .await?;
        self.commit_login(credential, login.credential_baseline, login.deadline)
            .await?;
        Ok(CodexCredentialStatus::Ready)
    }

    /// Starts the pinned Codex custom device flow.
    ///
    /// # Errors
    ///
    /// Rejects stale profile acceptance and malformed or failed pinned device
    /// authorization responses.
    pub async fn begin_device_login(&self) -> Result<CodexDeviceLogin, NativeCodexAuthError> {
        self.validate_profile_admission()?;
        let credential_baseline = self.store.load().map_err(|_| NativeCodexAuthError::Store)?;
        let body = serde_json::to_vec(&DeviceUserCodeRequest {
            client_id: OPENAI_CODEX_NATIVE_CLIENT_ID,
        })
        .map_err(|_| NativeCodexAuthError::Validation)?;
        let reply = execute_bounded(
            &self.client,
            &self.endpoints.auth_origin,
            Method::POST,
            DEVICE_USER_CODE_PATH,
            json_headers(None)?,
            body,
            Instant::now() + Duration::from_secs(30),
        )
        .await?;
        require_status(&reply, StatusCode::OK)?;
        require_json_media_type(&reply)?;
        let mut response: DeviceUserCodeResponse = parse_strict_json(&reply.body)?;
        validate_oauth_value(&response.device_auth_id, MAX_OAUTH_VALUE_BYTES)?;
        validate_user_code(&response.user_code)?;
        let interval = response
            .interval
            .parse::<u64>()
            .map_err(|_| NativeCodexAuthError::Validation)?;
        if interval == 0 || interval > MAX_DEVICE_INTERVAL_SECONDS {
            return Err(NativeCodexAuthError::Validation);
        }
        Ok(CodexDeviceLogin {
            verification_url: endpoint_url(&self.endpoints.auth_origin, DEVICE_VERIFICATION_PATH)?
                .into(),
            user_code: AuthSecretString::from(std::mem::take(&mut response.user_code)),
            device_auth_id: AuthSecretString::from(std::mem::take(&mut response.device_auth_id)),
            interval: Duration::from_secs(interval),
            deadline: Instant::now() + DEVICE_DEADLINE,
            credential_baseline,
        })
    }

    /// Completes one pinned custom device flow and atomically installs it.
    ///
    /// # Errors
    ///
    /// Rejects expiration, malformed verifier/challenge material, and any
    /// transport, token, identity, account-continuity, or persistence failure.
    pub async fn complete_device_login(
        &self,
        login: CodexDeviceLogin,
    ) -> Result<CodexCredentialStatus, NativeCodexAuthError> {
        self.validate_profile_admission()?;
        let mut code = loop {
            if Instant::now() >= login.deadline {
                return Err(NativeCodexAuthError::Provider);
            }
            let body = serde_json::to_vec(&DevicePollRequest {
                device_auth_id: login.device_auth_id.expose_secret(),
                user_code: login.user_code.expose_secret(),
            })
            .map_err(|_| NativeCodexAuthError::Validation)?;
            let reply = execute_bounded(
                &self.client,
                &self.endpoints.auth_origin,
                Method::POST,
                DEVICE_TOKEN_PATH,
                json_headers(None)?,
                body,
                capped_login_deadline(
                    login.deadline,
                    AUTH_REQUEST_CAP,
                    NativeCodexAuthError::Provider,
                )?,
            )
            .await?;
            match reply.status {
                StatusCode::OK => {
                    require_json_media_type(&reply)?;
                    break parse_strict_json::<DeviceCodeResponse>(&reply.body)?;
                }
                StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => {
                    // Custom Codex device flow has no RFC 8628 slow_down; shared
                    // schedule still owns remaining-time-clamped sleep math.
                    let schedule = DevicePollSchedule::new(
                        login.interval,
                        Duration::from_secs(MAX_DEVICE_INTERVAL_SECONDS),
                        Duration::ZERO,
                        login.deadline,
                    )
                    .map_err(|_| NativeCodexAuthError::Validation)?;
                    if schedule.is_expired(Instant::now()) {
                        return Err(NativeCodexAuthError::Provider);
                    }
                    tokio::time::sleep(schedule.sleep_for(Instant::now())).await;
                }
                _ => return Err(NativeCodexAuthError::Provider),
            }
        };
        validate_oauth_value(&code.authorization_code, MAX_OAUTH_VALUE_BYTES)?;
        validate_oauth_value(&code.code_verifier, 128)?;
        validate_oauth_value(&code.code_challenge, 128)?;
        let verifier = plakous_auth::PkceVerifier::new(AuthSecretString::from(std::mem::take(
            &mut code.code_verifier,
        )))
        .map_err(|_| NativeCodexAuthError::Validation)?;
        let pkce = plakous_auth::PkceS256::from_verifier(verifier);
        if !constant_time_value_eq(pkce.challenge().as_str(), &code.code_challenge) {
            return Err(NativeCodexAuthError::Validation);
        }
        let redirect_uri = endpoint_url(&self.endpoints.auth_origin, DEVICE_REDIRECT_PATH)?;
        let remaining = remaining_login_time(login.deadline, NativeCodexAuthError::Provider)?;
        let _lifecycle = tokio::time::timeout(remaining, self.lifecycle.lock())
            .await
            .map_err(|_| NativeCodexAuthError::Provider)?;
        self.validate_profile_admission()?;
        let credential = self
            .exchange_authorization_code(
                &code.authorization_code,
                redirect_uri.as_str(),
                pkce.verifier().expose_secret(),
                None,
                login.deadline,
            )
            .await?;
        self.commit_login(credential, login.credential_baseline, login.deadline)
            .await?;
        Ok(CodexCredentialStatus::Ready)
    }

    pub(super) async fn exchange_authorization_code(
        &self,
        code: &str,
        redirect_uri: &str,
        verifier: &str,
        expected_identity: Option<(&str, &str)>,
        deadline: Instant,
    ) -> Result<StoredCredential, NativeCodexAuthError> {
        remaining_login_time(deadline, NativeCodexAuthError::LoginTokenTransport)?;
        validate_oauth_value(code, MAX_OAUTH_VALUE_BYTES)
            .map_err(|_| NativeCodexAuthError::LoginTokenTransport)?;
        let body = form_body(&authorization_code_token_form(
            code,
            redirect_uri,
            OPENAI_CODEX_NATIVE_CLIENT_ID,
            verifier,
        ));
        let reply = execute_bounded(
            &self.client,
            &self.endpoints.auth_origin,
            Method::POST,
            TOKEN_PATH,
            form_headers(),
            body,
            capped_login_deadline(
                deadline,
                AUTH_REQUEST_CAP,
                NativeCodexAuthError::LoginTokenTransport,
            )?,
        )
        .await
        .map_err(|_| NativeCodexAuthError::LoginTokenTransport)?;
        if !reply.status.is_success() {
            return Err(NativeCodexAuthError::LoginTokenRejected);
        }
        // The provider-owned Codex client parses a bounded successful body as
        // JSON without making token validity depend on an optional media-type
        // header. Preserve that compatibility contract here.
        let tokens: OAuthLoginTokenResponse = parse_strict_json(&reply.body)
            .map_err(|_| NativeCodexAuthError::LoginTokenResponseInvalid)?;
        self.validate_token_response(tokens, expected_identity, true, deadline)
            .await
    }

    pub(super) async fn commit_login(
        &self,
        credential: StoredCredential,
        credential_baseline: Option<CredentialFence>,
        deadline: Instant,
    ) -> Result<(), NativeCodexAuthError> {
        remaining_login_time(deadline, NativeCodexAuthError::LoginPersistence)?;
        let store = Arc::clone(&self.store);
        let accepted_at = self.acceptance.accepted_at;
        let (account, committed) = run_blocking(move || {
            validate_profile_at(accepted_at, now_unix_secs())?;
            remaining_login_time(deadline, NativeCodexAuthError::LoginPersistence)?;
            let account = AccountBinding::new(&credential.account_id)
                .map_err(|_| NativeCodexAuthError::LoginPersistence)?;
            remaining_login_time(deadline, NativeCodexAuthError::LoginPersistence)?;
            let committed = if let Some(fence) = credential_baseline {
                let next = fence
                    .generation()
                    .next()
                    .map_err(|_| NativeCodexAuthError::LoginPersistence)?;
                let record = stored_record(&credential, next)
                    .map_err(|_| NativeCodexAuthError::LoginPersistence)?;
                store
                    .replace_selected_login(&fence, record)
                    .map_err(|_| NativeCodexAuthError::LoginPersistence)?
            } else {
                let first = CredentialGeneration::new(1)
                    .map_err(|_| NativeCodexAuthError::LoginPersistence)?;
                let record = stored_record(&credential, first)
                    .map_err(|_| NativeCodexAuthError::LoginPersistence)?;
                let committed = record.fence();
                store
                    .initialize_selected(&record)
                    .map_err(|_| NativeCodexAuthError::LoginPersistence)?;
                committed
            };
            if Instant::now() >= deadline {
                store
                    .logout(committed.generation())
                    .map_err(|_| NativeCodexAuthError::LoginPersistence)?;
                return Err(NativeCodexAuthError::LoginPersistence);
            }
            Ok((account, committed))
        })
        .await
        .map_err(login_persistence_error)?;
        if Instant::now() >= deadline {
            let store = Arc::clone(&self.store);
            run_blocking(move || {
                store
                    .logout(committed.generation())
                    .map_err(|_| NativeCodexAuthError::LoginPersistence)
            })
            .await
            .map_err(login_persistence_error)?;
            return Err(NativeCodexAuthError::LoginPersistence);
        }
        self.install_selected_account(account)
            .map_err(|_| NativeCodexAuthError::LoginPersistence)?;
        if Instant::now() >= deadline {
            let store = Arc::clone(&self.store);
            run_blocking(move || {
                store
                    .logout(committed.generation())
                    .map_err(|_| NativeCodexAuthError::LoginPersistence)
            })
            .await
            .map_err(login_persistence_error)?;
            return Err(NativeCodexAuthError::LoginPersistence);
        }
        Ok(())
    }
}

pub(super) fn login_persistence_error(error: NativeCodexAuthError) -> NativeCodexAuthError {
    match error {
        NativeCodexAuthError::ProfileNotAccepted | NativeCodexAuthError::ProfileNotCurrent => error,
        _ => NativeCodexAuthError::LoginPersistence,
    }
}

#[derive(Serialize)]
pub(super) struct DeviceUserCodeRequest<'a> {
    pub(super) client_id: &'a str,
}

#[derive(Deserialize)]
pub(super) struct DeviceUserCodeResponse {
    pub(super) device_auth_id: String,
    #[serde(alias = "usercode")]
    pub(super) user_code: String,
    pub(super) interval: String,
}

impl Drop for DeviceUserCodeResponse {
    fn drop(&mut self) {
        self.device_auth_id.zeroize();
        self.user_code.zeroize();
        self.interval.zeroize();
    }
}

#[derive(Serialize)]
pub(super) struct DevicePollRequest<'a> {
    pub(super) device_auth_id: &'a str,
    pub(super) user_code: &'a str,
}

#[derive(Deserialize)]
pub(super) struct DeviceCodeResponse {
    pub(super) authorization_code: String,
    pub(super) code_challenge: String,
    pub(super) code_verifier: String,
}

impl Drop for DeviceCodeResponse {
    fn drop(&mut self) {
        self.authorization_code.zeroize();
        self.code_challenge.zeroize();
        self.code_verifier.zeroize();
    }
}

pub(super) fn codex_callback_policy() -> CallbackQueryPolicy {
    CallbackQueryPolicy::codex(OPENAI_CODEX_OAUTH_SCOPES)
}

pub(super) fn parse_browser_callback(
    request: &[u8],
    expected_port: u16,
) -> Result<OAuthCallbackOutcome, NativeCodexAuthError> {
    let text = std::str::from_utf8(request).map_err(|_| NativeCodexAuthError::Callback)?;
    let parsed = parse_http11_get_request(text).map_err(|_| NativeCodexAuthError::Callback)?;
    if !CODEX_OAUTH.loopback_host_matches(parsed.host, Some(expected_port)) {
        return Err(NativeCodexAuthError::Callback);
    }
    let (path, query) = parsed
        .target
        .split_once('?')
        .ok_or(NativeCodexAuthError::Callback)?;
    if path != BROWSER_CALLBACK_PATH || query.is_empty() {
        return Err(NativeCodexAuthError::Callback);
    }
    parse_oauth_callback_query(query, codex_callback_policy()).map_err(map_oauth_workflow_error)
}

pub(super) const CODEX_CALLBACK_PAGE: LoopbackCallbackPage<'static> = LoopbackCallbackPage {
    complete: "Login complete",
    rejected: "Login rejected",
    failed: "Login failed",
    headers: &[("Cache-Control", "no-store")],
};

pub(super) struct CodexBrowserLoopbackHooks<'a> {
    pub(super) manager: &'a CodexSubscriptionCredentialManager,
    /// Session deadline retained after the attempt is consumed by complete.
    pub(super) deadline: Instant,
    pub(super) attempt: Option<BrowserLoginAttempt>,
}

#[allow(
    clippy::unused_async_trait_impl,
    reason = "hook methods implement the orchestrator's async trait; some bodies finish without awaiting, and async keeps them lazy until the orchestrator polls"
)]
impl BrowserLoopbackHooks for CodexBrowserLoopbackHooks<'_> {
    type Error = NativeCodexAuthError;
    type Callback = OAuthCallbackOutcome;
    type Preflight = ();
    type Output = CodexCredentialStatus;

    fn deadline(&self) -> Instant {
        self.deadline
    }

    fn max_request_bytes(&self) -> usize {
        MAX_CALLBACK_BYTES
    }

    fn deadline_error(&self) -> Self::Error {
        NativeCodexAuthError::Callback
    }

    fn callback_error(&self) -> Self::Error {
        NativeCodexAuthError::Callback
    }

    fn parse_request(
        &mut self,
        request: &[u8],
        local_port: u16,
    ) -> Result<LoopbackConnectionEvent<Self::Callback, Self::Preflight>, Self::Error> {
        parse_browser_callback(request, local_port).map(LoopbackConnectionEvent::Callback)
    }

    fn preflight_response_bytes(&self, _preflight: &Self::Preflight) -> Vec<u8> {
        Vec::new()
    }

    fn callback_response_bytes(&self, complete: Result<&Self::Output, &Self::Error>) -> Vec<u8> {
        CODEX_CALLBACK_PAGE.response_bytes(match complete {
            Ok(_) => LoopbackCallbackOutcome::Complete,
            Err(NativeCodexAuthError::Callback) => LoopbackCallbackOutcome::Rejected,
            Err(_) => LoopbackCallbackOutcome::Failed,
        })
    }

    async fn complete_callback(
        &mut self,
        callback: Self::Callback,
    ) -> Result<Self::Output, Self::Error> {
        let attempt = self.attempt.take().ok_or(NativeCodexAuthError::Callback)?;
        self.manager
            .complete_browser_callback(attempt, callback)
            .await
    }
}

#[cfg(test)]
pub(super) async fn write_callback_response<W>(
    stream: &mut W,
    status: StatusCode,
    body: &str,
    deadline: Instant,
) where
    W: tokio::io::AsyncWrite + Unpin,
{
    let Ok(remaining) = remaining_login_time(deadline, NativeCodexAuthError::Callback) else {
        return;
    };
    let remaining = remaining.min(CALLBACK_RESPONSE_CAP);
    let response = format_http11_text_response(
        status.as_u16(),
        status.canonical_reason().unwrap_or("Result"),
        body,
        &[("Cache-Control", "no-store")],
    );
    let _ =
        write_all_before_deadline(stream, response.as_bytes(), Instant::now() + remaining).await;
}
