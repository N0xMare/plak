//! Browser, remote-browser and device login flows and their loopback hooks.

use super::{
    Arc, BrowserLoopbackHooks, CallbackBinding, CallbackQueryPolicy, CredentialFence, DEVICE_GRANT,
    DEVICE_SLOW_DOWN_INCREMENT, DevicePollSchedule, Duration, Instant, LoopbackCallbackOutcome,
    LoopbackCallbackPage, LoopbackConnectionEvent, MAX_AUTH_BODY_BYTES, MAX_CALLBACK_BYTES,
    MAX_DEVICE_INTERVAL_SECONDS, MAX_LOOPBACK_CONNECTIONS, MAX_TOKEN_BYTES, ManagerCore,
    OAuthCallbackOutcome, OAuthErrorResponse, PkceS256, SecretString, TcpListener, TokenResponse,
    XAI_LOOPBACK_REDIRECT, XAI_OAUTH, XAI_OAUTH_SCOPE, XAI_PUBLIC_CLIENT_ID, XaiNativeAuthError,
    XaiSubscriptionStatus, Zeroizing, decode_json, fmt, is_http_token_byte,
    map_oauth_workflow_error, parse_absolute_redirect_query, parse_http11_get_request,
    parse_remote_authorization_input, run_browser_loopback_session, until_flow_deadline,
    urlencoded_form,
};

/// Non-cloneable redacted browser-login capability.
///
/// ```compile_fail
/// # use plakous_xai::XaiBrowserLogin;
/// fn duplicate(handle: XaiBrowserLogin) {
///     let _ = handle.clone();
/// }
/// ```
pub struct XaiBrowserLogin {
    pub(super) flow: BrowserLoginFlow,
    pub(super) listener: TcpListener,
}

pub(super) struct BrowserLoginFlow {
    pub(super) core: Arc<ManagerCore>,
    pub(super) baseline: Option<CredentialFence>,
    pub(super) pkce: PkceS256,
    pub(super) binding: CallbackBinding,
    pub(super) authorization_url: Zeroizing<String>,
    pub(super) deadline: Instant,
}

impl XaiBrowserLogin {
    /// Returns the exact xAI authorization URL the app should open.
    #[must_use]
    pub fn authorization_url(&self) -> &str {
        self.flow.authorization_url()
    }

    /// Accepts bounded xAI browser preflights followed by one fixed-loopback
    /// callback, validates state/nonce/ID token and `UserInfo`, and atomically
    /// installs the selected account.
    ///
    /// # Errors
    ///
    /// Fails closed for callback, token, identity, scope, account, or storage errors.
    pub async fn complete(self) -> Result<XaiSubscriptionStatus, XaiNativeAuthError> {
        let Self { flow, listener } = self;
        flow.ensure_active()?;
        let mut hooks = XaiBrowserLoopbackHooks {
            deadline: flow.deadline,
            flow: Some(flow),
        };
        run_browser_loopback_session(listener, &mut hooks).await
    }
}

impl BrowserLoginFlow {
    pub(super) fn authorization_url(&self) -> &str {
        &self.authorization_url
    }

    pub(super) fn ensure_active(&self) -> Result<(), XaiNativeAuthError> {
        self.core.ensure_flow_active(self.deadline)
    }

    pub(super) async fn complete_callback(
        self,
        callback: BrowserCallback,
    ) -> Result<XaiSubscriptionStatus, XaiNativeAuthError> {
        self.ensure_active()?;
        let code = match callback {
            BrowserCallback::AuthorizationCode { code, state } => {
                self.binding
                    .verify(&state, self.binding.expose_nonce())
                    .map_err(|_| XaiNativeAuthError::InvalidCallback)?;
                code
            }
            BrowserCallback::ProviderError { state } => {
                self.binding
                    .verify(&state, self.binding.expose_nonce())
                    .map_err(|_| XaiNativeAuthError::InvalidCallback)?;
                return Err(XaiNativeAuthError::AuthorizationDenied);
            }
        };
        until_flow_deadline(
            self.deadline,
            self.core.exchange_code(
                code,
                &self.pkce,
                Some(self.binding.expose_nonce()),
                self.baseline.as_ref(),
                self.deadline,
            ),
        )
        .await
    }
}

impl fmt::Debug for XaiBrowserLogin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("XaiBrowserLogin(<redacted non-cloneable login>)")
    }
}

/// Non-cloneable redacted browser-login capability for pasted callbacks.
///
/// This handle owns the same PKCE verifier, state, nonce, selected-slot
/// baseline, and deadline as [`XaiBrowserLogin`], but owns no socket.
///
/// ```compile_fail
/// # use plakous_xai::XaiRemoteBrowserLogin;
/// fn duplicate(handle: XaiRemoteBrowserLogin) {
///     let _ = handle.clone();
/// }
/// ```
pub struct XaiRemoteBrowserLogin {
    pub(super) flow: BrowserLoginFlow,
}

impl XaiRemoteBrowserLogin {
    /// Returns the exact xAI authorization URL the app should open.
    #[must_use]
    pub fn authorization_url(&self) -> &str {
        self.flow.authorization_url()
    }

    /// Validates and consumes one pasted authorization input, then performs the
    /// shared token, identity, and selected-slot installation.
    ///
    /// Accepts either:
    /// - the complete loopback redirect URL (`http://127.0.0.1:<port>/callback?code=…&state=…`), or
    /// - a bare authorization `code` (paired with this attempt's owned `state`).
    ///
    /// The input is never included in an error or response. Full redirect URLs
    /// must still match the pinned callback grammar and attempt-owned state.
    /// Bare codes are validated as URL-safe tokens and bound to this attempt's
    /// stored state (same CSRF protection as a state-bearing redirect).
    ///
    /// # Errors
    ///
    /// Fails closed for callback, correlation, deadline, token, identity,
    /// scope, account, or storage errors.
    pub async fn complete(
        self,
        authorization_input: &str,
    ) -> Result<XaiSubscriptionStatus, XaiNativeAuthError> {
        self.flow.ensure_active()?;
        let callback = parse_remote_authorization_input(
            authorization_input,
            self.flow.binding.expose_state(),
            XAI_LOOPBACK_REDIRECT,
            xai_callback_policy(),
        )
        .map(oauth_callback_to_browser)
        .map_err(map_oauth_workflow_error)?;
        self.flow.complete_callback(callback).await
    }
}

impl fmt::Debug for XaiRemoteBrowserLogin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("XaiRemoteBrowserLogin(<redacted non-cloneable login>)")
    }
}

/// Non-cloneable redacted RFC 8628 device-login capability.
pub struct XaiDeviceLogin {
    pub(super) core: Arc<ManagerCore>,
    pub(super) baseline: Option<CredentialFence>,
    pub(super) device_code: SecretString,
    pub(super) user_code: String,
    pub(super) verification_uri: String,
    pub(super) verification_uri_complete: Option<String>,
    pub(super) interval: Duration,
    pub(super) deadline: Instant,
}

impl XaiDeviceLogin {
    /// Returns the short code intended for display to the user.
    #[must_use]
    pub fn user_code(&self) -> &str {
        &self.user_code
    }

    /// Returns the provider verification URL.
    #[must_use]
    pub fn verification_uri(&self) -> &str {
        self.verification_uri_complete
            .as_deref()
            .unwrap_or(&self.verification_uri)
    }

    /// Polls under RFC 8628 `pending/slow_down` semantics and installs the login.
    ///
    /// # Errors
    ///
    /// Fails on denial, expiry, malformed responses, identity mismatch, or storage failure.
    pub async fn complete(mut self) -> Result<XaiSubscriptionStatus, XaiNativeAuthError> {
        let mut schedule = DevicePollSchedule::new(
            self.interval,
            Duration::from_secs(MAX_DEVICE_INTERVAL_SECONDS),
            DEVICE_SLOW_DOWN_INCREMENT,
            self.deadline,
        )
        .map_err(|_| XaiNativeAuthError::LoginTokenExchange)?;
        loop {
            self.core.ensure_flow_active(self.deadline)?;
            let form = urlencoded_form(&[
                ("grant_type", DEVICE_GRANT),
                ("client_id", XAI_PUBLIC_CLIENT_ID),
                ("device_code", self.device_code.expose_secret()),
            ]);
            match until_flow_deadline(
                self.deadline,
                self.core
                    .send_login_token_form_raw(self.core.endpoints.token(), form),
            )
            .await
            {
                Ok((status, response)) if status.is_success() => {
                    return until_flow_deadline(self.deadline, self.complete_success(response))
                        .await;
                }
                Ok((_status, response)) => {
                    self.core.ensure_flow_active(self.deadline)?;
                    let error: OAuthErrorResponse = decode_json(&response, MAX_AUTH_BODY_BYTES)
                        .map_err(|_| XaiNativeAuthError::LoginTokenExchange)?;
                    match error.error.as_str() {
                        "authorization_pending" => {}
                        "slow_down" => {
                            schedule.apply_slow_down();
                            self.interval = schedule.interval();
                        }
                        "access_denied" | "authorization_denied" => {
                            return Err(XaiNativeAuthError::AuthorizationDenied);
                        }
                        "expired_token" => return Err(XaiNativeAuthError::LoginExpired),
                        _ => return Err(XaiNativeAuthError::LoginTokenExchange),
                    }
                }
                Err(error) => return Err(error),
            }
            self.core.ensure_flow_active(self.deadline)?;
            tokio::time::sleep(schedule.sleep_for(Instant::now())).await;
        }
    }

    pub(super) async fn complete_success(
        &self,
        response: Zeroizing<Vec<u8>>,
    ) -> Result<XaiSubscriptionStatus, XaiNativeAuthError> {
        self.core.ensure_flow_active(self.deadline)?;
        let tokens: TokenResponse = decode_json(&response, MAX_TOKEN_BYTES)
            .map_err(|_| XaiNativeAuthError::LoginTokenExchange)?;
        let stored = self
            .core
            .validate_tokens(tokens, None, None, None, None, Some(self.deadline))
            .await?;
        self.core.ensure_flow_active(self.deadline)?;
        self.core
            .persist_login(self.baseline.as_ref(), &stored, Some(self.deadline))
    }
}

impl fmt::Debug for XaiDeviceLogin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("XaiDeviceLogin(<redacted non-cloneable login>)")
    }
}

pub(super) enum LoopbackRequest {
    Callback(BrowserCallback),
    Preflight { origin: AllowedBrowserOrigin },
}

pub(super) const XAI_CALLBACK_PAGE: LoopbackCallbackPage<'static> = LoopbackCallbackPage {
    complete: "Authorization complete. Return to your application.",
    rejected: "Authorization failed. Return to your application.",
    failed: "Authorization failed. Return to your application.",
    headers: &[
        ("Cache-Control", "no-store"),
        ("Pragma", "no-cache"),
        ("X-Content-Type-Options", "nosniff"),
    ],
};

pub(super) struct XaiBrowserLoopbackHooks {
    /// Session deadline retained after the flow is consumed by complete.
    pub(super) deadline: Instant,
    pub(super) flow: Option<BrowserLoginFlow>,
}

#[allow(
    clippy::unused_async_trait_impl,
    reason = "hook methods implement the orchestrator's async trait; some bodies finish without awaiting, and async keeps them lazy until the orchestrator polls"
)]
impl BrowserLoopbackHooks for XaiBrowserLoopbackHooks {
    type Error = XaiNativeAuthError;
    type Callback = BrowserCallback;
    type Preflight = AllowedBrowserOrigin;
    type Output = XaiSubscriptionStatus;

    fn deadline(&self) -> Instant {
        self.deadline
    }

    fn max_connections(&self) -> usize {
        MAX_LOOPBACK_CONNECTIONS
    }

    fn max_request_bytes(&self) -> usize {
        MAX_CALLBACK_BYTES
    }

    fn deadline_error(&self) -> Self::Error {
        XaiNativeAuthError::LoginExpired
    }

    fn callback_error(&self) -> Self::Error {
        XaiNativeAuthError::InvalidCallback
    }

    fn parse_request(
        &mut self,
        request: &[u8],
        _local_port: u16,
    ) -> Result<LoopbackConnectionEvent<Self::Callback, Self::Preflight>, Self::Error> {
        let text = std::str::from_utf8(request).map_err(|_| XaiNativeAuthError::InvalidCallback)?;
        match parse_loopback_request(text)? {
            LoopbackRequest::Callback(callback) => Ok(LoopbackConnectionEvent::Callback(callback)),
            LoopbackRequest::Preflight { origin } => Ok(LoopbackConnectionEvent::Preflight(origin)),
        }
    }

    fn preflight_response_bytes(&self, preflight: &Self::Preflight) -> Vec<u8> {
        format!(
            "HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: {}\r\nAccess-Control-Allow-Methods: GET, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type\r\nAccess-Control-Allow-Private-Network: true\r\nVary: Origin\r\nCache-Control: no-store\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            preflight.as_str()
        )
        .into_bytes()
    }

    fn callback_response_bytes(&self, complete: Result<&Self::Output, &Self::Error>) -> Vec<u8> {
        XAI_CALLBACK_PAGE.response_bytes(if complete.is_ok() {
            LoopbackCallbackOutcome::Complete
        } else {
            LoopbackCallbackOutcome::Failed
        })
    }

    async fn complete_callback(
        &mut self,
        callback: Self::Callback,
    ) -> Result<Self::Output, Self::Error> {
        let flow = self
            .flow
            .take()
            .ok_or(XaiNativeAuthError::InvalidCallback)?;
        flow.complete_callback(callback).await
    }
}

#[derive(Clone, Copy)]
pub(super) enum AllowedBrowserOrigin {
    AccountsXai,
    AuthXai,
}

impl AllowedBrowserOrigin {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::AccountsXai => "https://accounts.x.ai",
            Self::AuthXai => "https://auth.x.ai",
        }
    }
}

pub(super) enum BrowserCallback {
    AuthorizationCode {
        code: Zeroizing<String>,
        state: Zeroizing<String>,
    },
    ProviderError {
        state: Zeroizing<String>,
    },
}

pub(super) fn parse_loopback_request(request: &str) -> Result<LoopbackRequest, XaiNativeAuthError> {
    if request.starts_with("OPTIONS ") {
        return parse_preflight_request(request)
            .map(|origin| LoopbackRequest::Preflight { origin });
    }
    parse_callback_request(request).map(LoopbackRequest::Callback)
}

pub(super) fn parse_preflight_request(
    request: &str,
) -> Result<AllowedBrowserOrigin, XaiNativeAuthError> {
    let head = request
        .strip_suffix("\r\n\r\n")
        .ok_or(XaiNativeAuthError::InvalidCallback)?;
    let mut lines = head.split("\r\n");
    if lines.next() != Some("OPTIONS /callback HTTP/1.1") {
        return Err(XaiNativeAuthError::InvalidCallback);
    }
    let mut host = None;
    let mut origin = None;
    let mut requested_method = None;
    for line in lines {
        if line.is_empty() || line.starts_with([' ', '\t']) {
            return Err(XaiNativeAuthError::InvalidCallback);
        }
        let (name, value) = line
            .split_once(':')
            .ok_or(XaiNativeAuthError::InvalidCallback)?;
        if name.is_empty() || !name.bytes().all(is_http_token_byte) {
            return Err(XaiNativeAuthError::InvalidCallback);
        }
        let value = value.trim_matches(|character| character == ' ' || character == '\t');
        if !value
            .bytes()
            .all(|byte| byte == b'\t' || (0x20..=0x7e).contains(&byte))
        {
            return Err(XaiNativeAuthError::InvalidCallback);
        }
        if name.eq_ignore_ascii_case("host") && host.replace(value).is_some() {
            return Err(XaiNativeAuthError::InvalidCallback);
        }
        if name.eq_ignore_ascii_case("origin") && origin.replace(value).is_some() {
            return Err(XaiNativeAuthError::InvalidCallback);
        }
        if name.eq_ignore_ascii_case("access-control-request-method")
            && requested_method.replace(value).is_some()
        {
            return Err(XaiNativeAuthError::InvalidCallback);
        }
    }
    if host.is_none_or(|value| !XAI_OAUTH.loopback_host_matches(value, None))
        || requested_method != Some("GET")
    {
        return Err(XaiNativeAuthError::InvalidCallback);
    }
    match origin {
        Some("https://accounts.x.ai") => Ok(AllowedBrowserOrigin::AccountsXai),
        Some("https://auth.x.ai") => Ok(AllowedBrowserOrigin::AuthXai),
        _ => Err(XaiNativeAuthError::InvalidCallback),
    }
}

pub(super) fn xai_callback_policy() -> CallbackQueryPolicy {
    CallbackQueryPolicy::xai(XAI_OAUTH_SCOPE)
}

pub(super) fn oauth_callback_to_browser(outcome: OAuthCallbackOutcome) -> BrowserCallback {
    match outcome {
        OAuthCallbackOutcome::AuthorizationCode { code, state } => {
            BrowserCallback::AuthorizationCode { code, state }
        }
        OAuthCallbackOutcome::ProviderError { state } => BrowserCallback::ProviderError { state },
    }
}

pub(super) fn parse_callback_request(request: &str) -> Result<BrowserCallback, XaiNativeAuthError> {
    let parsed =
        parse_http11_get_request(request).map_err(|_| XaiNativeAuthError::InvalidCallback)?;
    if !XAI_OAUTH.loopback_host_matches(parsed.host, None) {
        return Err(XaiNativeAuthError::InvalidCallback);
    }
    let full_redirect_url = Zeroizing::new(format!(
        "http://{}{}",
        XAI_OAUTH.loopback_authority, parsed.target
    ));
    parse_callback_url(&full_redirect_url)
}

pub(super) fn parse_callback_url(
    full_redirect_url: &str,
) -> Result<BrowserCallback, XaiNativeAuthError> {
    parse_absolute_redirect_query(
        full_redirect_url,
        XAI_LOOPBACK_REDIRECT,
        xai_callback_policy(),
    )
    .map(oauth_callback_to_browser)
    .map_err(|_| XaiNativeAuthError::InvalidCallback)
}
