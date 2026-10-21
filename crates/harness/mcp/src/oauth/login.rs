//! Logging in to an OAuth MCP server and logging out of it.
//!
//! Only an embedder's login command drives these; nothing in a session does.
//! Discovery, registration and the token exchange follow the MCP
//! authorization specification 2026-07-28 (chapter 01 §8.5).

use std::time::{Duration, Instant};

use http::header::{ACCEPT, CONTENT_TYPE};
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use plakous_auth::oauth_orchestrator::{
    BrowserLoopbackHooks, BuildRequestFailure, ClaimedRevokeHooks, LoopbackConnectionEvent,
    bind_loopback_listener, run_browser_loopback_session, run_claimed_revoke,
};
use plakous_auth::oauth_workflow::{
    AuthorizationServerMetadata, BearerChallenge, CallbackQueryPolicy, DevicePollSchedule,
    OAuthCallbackOutcome, OAuthWorkflowError, ResourceAuthorizeRequest,
    authorization_server_metadata_urls, build_resource_authorization_url, check_callback_issuer,
    device_authorization_form, device_token_form, dynamic_registration_request,
    format_http11_text_response, oauth_error_code, parse_absolute_redirect_query,
    parse_authorization_server_metadata, parse_bearer_challenge, parse_device_authorization,
    parse_dynamic_registration, parse_http11_get_request, parse_loopback_callback_target,
    parse_protected_resource_metadata, parse_resource_token_response,
    protected_resource_metadata_urls, resource_token_form, revocation_form, take_issuer_parameter,
    union_scopes,
};
use plakous_auth::{
    ClaimedHttpError, ClaimedHttpResponse, CredentialFence, CredentialGeneration, CredentialStatus,
    PkceS256, PrivateCredentialStore, PrivateStoreError, RefreshClaim, SecretString,
    generate_oauth_correlation_token,
};
use plakous_http::{Bytes, ExchangeControl, SensitiveBytes};
use plakous_tools::McpServerName;
use tokio::net::TcpListener;
use zeroize::Zeroizing;

use super::McpOAuthSettings;
use super::credential::{
    LoginExpectation, STORED_VERSION, StoredLogin, TokenEndpointAuth, open_store, read_ready,
    status, store_failed, unix_now,
};
use super::exchange::{ClientAuth, EXCHANGE_TIMEOUT, OAuthHttp, redact_query};
use crate::McpOAuthStatus;
use crate::error::{McpError, McpErrorKind};
use crate::http::McpHttpEndpoint;
use crate::pool::McpPool;
use crate::spec::{McpServerSpec, Transport};

const OFFLINE_ACCESS: &str = "offline_access";
pub(super) const MAX_SCOPE_BYTES: usize = 1024;
const DEFAULT_CLIENT_NAME: &str = "Plakous";
const DISCOVER_PROTOCOL_VERSION: &str = "2026-07-28";
const DEVICE_MAX_INTERVAL: Duration = Duration::from_secs(60);
const DEVICE_SLOW_DOWN: Duration = Duration::from_secs(5);
const CALLBACK_PAGE_COMPLETE: &str = "Login complete. You can close this window.";
const CALLBACK_PAGE_REFUSED: &str = "Login failed. Return to the terminal for details.";

/// Choices a login makes beyond the server's definition.
#[derive(Clone, Debug)]
pub struct McpLoginOptions {
    scope: Option<String>,
    metadata_document: Option<String>,
    client_name: String,
}

impl Default for McpLoginOptions {
    fn default() -> Self {
        Self {
            scope: None,
            metadata_document: None,
            client_name: DEFAULT_CLIENT_NAME.to_owned(),
        }
    }
}

impl McpLoginOptions {
    /// Options with no explicit scope, no Client ID Metadata Document and
    /// the default client name.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests `scope` (space-separated) instead of the discovered one.
    ///
    /// # Errors
    ///
    /// Refuses an empty scope, one over 1 KiB, or one with characters
    /// outside the conservative scope-token alphabet.
    pub fn with_scope(mut self, scope: &str) -> Result<Self, McpError> {
        let scope = scope.split_whitespace().collect::<Vec<_>>().join(" ");
        if scope.is_empty()
            || scope.len() > MAX_SCOPE_BYTES
            || !plakous_auth::oauth_workflow::valid_scope_list(&scope)
        {
            return Err(authorization(
                "an oauth scope is 1 to 1024 bytes of space-separated tokens using letters, digits and ._:/+-",
            ));
        }
        self.scope = Some(scope);
        Ok(self)
    }

    /// Identifies the client by the Client ID Metadata Document at `url`
    /// to an authorization server that supports one, when no client is
    /// pre-registered.
    ///
    /// # Errors
    ///
    /// Refuses a URL that is not `https://` with a path and without query or
    /// fragment.
    pub fn with_client_id_metadata_document(mut self, url: &str) -> Result<Self, McpError> {
        let valid = url::Url::parse(url).is_ok_and(|parsed| {
            parsed.scheme() == "https"
                && parsed.host_str().is_some()
                && parsed.path() != "/"
                && parsed.query().is_none()
                && parsed.fragment().is_none()
                && parsed.username().is_empty()
                && parsed.password().is_none()
        });
        if !valid {
            return Err(authorization(
                "a client id metadata document url is https:// with a path and no query or fragment",
            ));
        }
        self.metadata_document = Some(url.to_owned());
        Ok(self)
    }

    /// Names the client in a dynamic registration.
    #[must_use]
    pub fn with_client_name(mut self, name: impl Into<String>) -> Self {
        self.client_name = name.into();
        self
    }
}

/// A discovered login, ready to start: the authorization server and scope
/// are known and can be shown before anything is written.
pub struct McpLogin {
    plan: Plan,
}

struct Plan {
    server: McpServerName,
    settings: McpOAuthSettings,
    http: OAuthHttp,
    resource: String,
    resource_indicator: String,
    metadata: AuthorizationServerMetadata,
    scope: Option<String>,
    metadata_document: Option<String>,
    client_name: String,
    baseline: Option<CredentialFence>,
}

impl std::fmt::Debug for McpLogin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpLogin")
            .field("server", &self.plan.server)
            .field("issuer", &self.plan.metadata.issuer)
            .field("scope", &self.plan.scope)
            .finish_non_exhaustive()
    }
}

impl McpPool {
    /// Discovers `spec`'s authorization server and the scope a login would
    /// request. Nothing is written until the login completes.
    ///
    /// # Errors
    ///
    /// Refuses a definition without OAuth, a login a newer build stored,
    /// and every discovery failure the MCP authorization specification
    /// names: metadata for another resource, no usable authorization-server
    /// metadata, no PKCE S256.
    pub async fn login(
        &self,
        spec: &McpServerSpec,
        options: McpLoginOptions,
    ) -> Result<McpLogin, McpError> {
        let (endpoint, settings) = oauth_endpoint(spec)?;
        let server = spec.name().clone();
        let resource = endpoint.canonical_resource()?;
        let http = OAuthHttp::new(&self.http_client()?, endpoint.is_loopback())?;
        let (baseline, replaced_scope) = baseline(settings, &resource)?;
        let challenge = probe(&http, endpoint).await?;
        let protected = protected_resource(&http, &server, endpoint.url(), &challenge).await?;
        let issuer = match settings.issuer() {
            Some(pinned) => {
                if !protected
                    .authorization_servers
                    .iter()
                    .any(|listed| listed == pinned)
                {
                    return Err(authorization(&format!(
                        "mcp server {server} does not list the pinned authorization server {pinned}"
                    )));
                }
                pinned.to_owned()
            }
            None => protected.authorization_servers[0].clone(),
        };
        let metadata = authorization_server(&http, &server, &issuer).await?;
        if !http.admits(&metadata.token_endpoint) || !http.admits(&metadata.authorization_endpoint)
        {
            return Err(authorization(&format!(
                "authorization server {issuer} names an endpoint outside the transport policy"
            )));
        }
        let discovered = options
            .scope
            .clone()
            .or_else(|| challenge.scope.clone())
            .or_else(|| {
                (!protected.scopes_supported.is_empty())
                    .then(|| protected.scopes_supported.join(" "))
            });
        let offline = metadata
            .scopes_supported
            .iter()
            .any(|scope| scope == OFFLINE_ACCESS)
            .then_some(OFFLINE_ACCESS);
        let scope = union_scopes([discovered.as_deref(), replaced_scope.as_deref(), offline]);
        Ok(McpLogin {
            plan: Plan {
                server,
                settings: settings.clone(),
                http,
                resource,
                resource_indicator: protected.resource,
                metadata,
                scope,
                metadata_document: options.metadata_document,
                client_name: options.client_name,
                baseline,
            },
        })
    }

    /// Logs out of `spec`'s server: revokes at its authorization server
    /// when it offers revocation, else (and when `local`) retires the login
    /// here only. An unusable login is retired here.
    ///
    /// # Errors
    ///
    /// Refuses a definition without OAuth and an unreadable store, and
    /// fails without retiring anything when the revocation fails.
    pub async fn logout(
        &self,
        spec: &McpServerSpec,
        local: bool,
    ) -> Result<McpLogoutOutcome, McpError> {
        let (endpoint, settings) = oauth_endpoint(spec)?;
        let Some(store) = open_store(settings.store())? else {
            return Ok(McpLogoutOutcome::NotLoggedIn);
        };
        let fence = match store.load() {
            Ok(Some(fence)) if fence.status() != CredentialStatus::Revoked => fence,
            Ok(_) => return Ok(McpLogoutOutcome::NotLoggedIn),
            Err(PrivateStoreError::WrittenByNewerBuild | PrivateStoreError::ForeignProfile) => {
                return retire(&store, McpLocalRetirement::Unusable);
            }
            Err(error) => return Err(store_failed("mcp login could not be read", error)),
        };
        if local {
            return retire(&store, McpLocalRetirement::Requested);
        }
        if fence.status() != CredentialStatus::Ready {
            return retire(&store, McpLocalRetirement::Unusable);
        }
        let Ok(stored) = read_ready(&store, &fence) else {
            return retire(&store, McpLocalRetirement::Unusable);
        };
        if !stored.belongs_to(&LoginExpectation::of(
            settings,
            &endpoint.canonical_resource()?,
        )) {
            return retire(&store, McpLocalRetirement::DefinitionChanged);
        }
        let Some(endpoint_url) = stored.revocation_endpoint.clone() else {
            return retire(&store, McpLocalRetirement::NoRevocationEndpoint);
        };
        let http = OAuthHttp::new(&self.http_client()?, endpoint.is_loopback())?;
        let secret = client_secret(settings, &stored)?;
        let mut hooks = RevokeHooks {
            http: &http,
            fence,
            endpoint: endpoint_url,
            secret,
        };
        // A failed revocation never falls back to local retirement: the
        // grant would stay live without the operator choosing that.
        run_claimed_revoke(&store, http.client(), &mut hooks)
            .await
            .map_err(|error| {
                McpError::with_source(
                    McpErrorKind::Authorization,
                    format!("{error}; the login was not retired"),
                    error,
                )
            })?;
        Ok(McpLogoutOutcome::Revoked)
    }
}

/// How a logout ended.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum McpLogoutOutcome {
    /// No login was stored.
    NotLoggedIn,
    /// The authorization server revoked the grant and the login is retired.
    Revoked,
    /// The login is retired here; the grant may remain active upstream.
    RetiredLocally(McpLocalRetirement),
}

/// Why a logout retired a login without revoking it.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum McpLocalRetirement {
    /// The logout asked for local retirement only.
    Requested,
    /// The authorization server offers no revocation endpoint.
    NoRevocationEndpoint,
    /// The definition no longer names this login's URL, issuer or client.
    DefinitionChanged,
    /// The stored login was not usable, so nothing could be revoked.
    Unusable,
}

fn retire(
    store: &PrivateCredentialStore,
    reason: McpLocalRetirement,
) -> Result<McpLogoutOutcome, McpError> {
    store
        .logout_local()
        .map_err(|error| store_failed("mcp login could not be retired", error))?;
    Ok(McpLogoutOutcome::RetiredLocally(reason))
}

impl McpLogin {
    /// Returns the authorization server's issuer.
    #[must_use]
    pub fn issuer(&self) -> &str {
        &self.plan.metadata.issuer
    }

    /// Returns the scope the login requests, when any.
    #[must_use]
    pub fn scope(&self) -> Option<&str> {
        self.plan.scope.as_deref()
    }

    /// Whether the authorization server offers the device grant, which a
    /// remote login uses.
    #[must_use]
    pub const fn offers_device_grant(&self) -> bool {
        self.plan.metadata.device_authorization_endpoint.is_some()
    }

    /// Starts a login on this machine: binds the loopback callback (the
    /// configured port, else an ephemeral one), registers the client when
    /// needed, and returns the authorization URL to open.
    ///
    /// # Errors
    ///
    /// Returns a bind failure, a registration failure, or a refusal when no
    /// registration path is available.
    pub async fn local(self) -> Result<McpLocalLogin, McpError> {
        let port = self.plan.settings.callback_port().unwrap_or(0);
        let (listener, port) = bind_loopback_listener(&[port]).await.map_err(|error| {
            McpError::with_source(
                McpErrorKind::Authorization,
                format!("the login callback port {port} could not be bound"),
                error,
            )
        })?;
        let attempt = Attempt::start(&self.plan, port).await?;
        Ok(McpLocalLogin {
            plan: self.plan,
            listener,
            attempt,
        })
    }

    /// Starts a login whose browser is elsewhere; nothing stays bound. The
    /// device grant is used when the authorization server offers it and
    /// accepts the request; otherwise the callback URL is pasted back.
    ///
    /// # Errors
    ///
    /// As [`Self::local`], and a device request that fails other than by
    /// refusal.
    pub async fn remote(self) -> Result<McpRemoteLogin, McpError> {
        let port = match self.plan.settings.callback_port() {
            Some(port) => port,
            None => ephemeral_port().await?,
        };
        let attempt = Attempt::start(&self.plan, port).await?;
        if let Some(endpoint) = self.plan.metadata.device_authorization_endpoint.clone()
            && self.plan.http.admits(&endpoint)
        {
            let fields = device_authorization_form(
                &attempt.client.id,
                self.plan.scope.as_deref(),
                &self.plan.resource_indicator,
            );
            let answer = self
                .plan
                .http
                .send(
                    self.plan
                        .http
                        .form_request(&endpoint, &fields, &attempt.client.auth())?,
                )
                .await?;
            if answer.status.is_success() {
                let device = parse_device_authorization(&answer.body)
                    .map_err(|_| authorization("the device authorization response is invalid"))?;
                return Ok(McpRemoteLogin::Device(McpDeviceLogin {
                    plan: self.plan,
                    attempt,
                    device,
                }));
            }
            if !answer.status.is_client_error() {
                return Err(authorization(&format!(
                    "the device authorization request failed with HTTP {}",
                    answer.status.as_u16()
                )));
            }
        }
        Ok(McpRemoteLogin::Paste(McpPasteLogin {
            plan: self.plan,
            attempt,
        }))
    }
}

/// A login completing on this machine's loopback callback.
pub struct McpLocalLogin {
    plan: Plan,
    listener: TcpListener,
    attempt: Attempt,
}

impl std::fmt::Debug for McpLocalLogin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpLocalLogin")
            .field("redirect_uri", &self.attempt.redirect_uri)
            .finish_non_exhaustive()
    }
}

impl McpLocalLogin {
    /// Returns the authorization URL to open in a browser.
    #[must_use]
    pub fn authorization_url(&self) -> &str {
        &self.attempt.authorization_url
    }

    /// Returns the redirect URI the callback listens on.
    #[must_use]
    pub fn redirect_uri(&self) -> &str {
        &self.attempt.redirect_uri
    }

    /// Waits up to `timeout` for the callback, checks it, exchanges the code
    /// and stores the login, replacing the previous one only now.
    ///
    /// # Errors
    ///
    /// Returns a timeout, a refused or mismatched callback (state, redirect,
    /// issuer), a token failure or a store failure.
    pub async fn complete(self, timeout: Duration) -> Result<McpOAuthStatus, McpError> {
        let mut hooks = CallbackHooks {
            plan: &self.plan,
            attempt: &self.attempt,
            deadline: Instant::now() + timeout,
        };
        run_browser_loopback_session(self.listener, &mut hooks).await
    }
}

/// A login completing away from this machine.
#[derive(Debug)]
pub enum McpRemoteLogin {
    /// The device grant: the user enters a code elsewhere.
    Device(McpDeviceLogin),
    /// Paste-back: the user pastes the callback URL their browser was sent to.
    Paste(McpPasteLogin),
}

/// A device-grant login.
pub struct McpDeviceLogin {
    plan: Plan,
    attempt: Attempt,
    device: plakous_auth::oauth_workflow::DeviceAuthorization,
}

impl std::fmt::Debug for McpDeviceLogin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpDeviceLogin")
            .field("verification_uri", &self.device.verification_uri)
            .finish_non_exhaustive()
    }
}

impl McpDeviceLogin {
    /// Returns where the user enters the code.
    #[must_use]
    pub fn verification_uri(&self) -> &str {
        &self.device.verification_uri
    }

    /// Returns the code the user enters.
    #[must_use]
    pub fn user_code(&self) -> &str {
        &self.device.user_code
    }

    /// Returns how long the code stays valid.
    #[must_use]
    pub const fn expires_in(&self) -> Duration {
        Duration::from_secs(self.device.expires_in)
    }

    /// Polls the token endpoint until the user approves, refuses or the code
    /// expires, then stores the login.
    ///
    /// # Errors
    ///
    /// Returns an expired, refused or failed grant, or a store failure.
    pub async fn complete(self) -> Result<McpOAuthStatus, McpError> {
        let plan = &self.plan;
        let client = self.attempt.client.auth();
        let deadline = Instant::now() + self.expires_in();
        let interval = self.device.interval.unwrap_or(5).clamp(1, 60);
        let mut schedule = DevicePollSchedule::new(
            Duration::from_secs(interval),
            DEVICE_MAX_INTERVAL,
            DEVICE_SLOW_DOWN,
            deadline,
        )
        .map_err(|_| authorization("the device grant interval is invalid"))?;
        loop {
            let now = Instant::now();
            if schedule.is_expired(now) {
                return Err(authorization(
                    "the device code expired before it was approved",
                ));
            }
            tokio::time::sleep(schedule.sleep_for(now)).await;
            let fields = device_token_form(
                self.device.device_code.expose_secret(),
                &self.attempt.client.id,
                &plan.resource_indicator,
            );
            let answer = plan
                .http
                .send(
                    plan.http
                        .form_request(&plan.metadata.token_endpoint, &fields, &client)?,
                )
                .await?;
            if answer.status.is_success() {
                return persist(plan, &self.attempt.client, &answer.body);
            }
            match oauth_error_code(&answer.body).as_deref() {
                Some("authorization_pending") => {}
                Some("slow_down") => schedule.apply_slow_down(),
                Some("access_denied") => return Err(authorization("the device login was refused")),
                Some("expired_token") => {
                    return Err(authorization(
                        "the device code expired before it was approved",
                    ));
                }
                code => return Err(token_refused(answer.status, code)),
            }
        }
    }
}

/// A paste-back login.
pub struct McpPasteLogin {
    plan: Plan,
    attempt: Attempt,
}

impl std::fmt::Debug for McpPasteLogin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpPasteLogin")
            .field("redirect_uri", &self.attempt.redirect_uri)
            .finish_non_exhaustive()
    }
}

impl McpPasteLogin {
    /// Returns the authorization URL to open in a browser.
    #[must_use]
    pub fn authorization_url(&self) -> &str {
        &self.attempt.authorization_url
    }

    /// Returns the redirect URI the pasted URL must start with.
    #[must_use]
    pub fn redirect_uri(&self) -> &str {
        &self.attempt.redirect_uri
    }

    /// Checks the full callback URL the browser was sent to, exchanges its
    /// code and stores the login.
    ///
    /// # Errors
    ///
    /// Returns a refused or mismatched callback, a token failure or a store
    /// failure.
    pub async fn complete(self, pasted: &str) -> Result<McpOAuthStatus, McpError> {
        let code = accept_callback(&self.plan, &self.attempt, pasted.trim(), None)?;
        exchange(&self.plan, &self.attempt, &code).await
    }
}

/// One authorization attempt: its registered client, PKCE pair, state and
/// URL.
struct Attempt {
    client: RegisteredClient,
    pkce: PkceS256,
    state: SecretString,
    redirect_uri: String,
    authorization_url: Zeroizing<String>,
}

struct RegisteredClient {
    id: String,
    secret: Option<Zeroizing<Vec<u8>>>,
    /// Whether the secret came from a dynamic registration, so it is stored.
    dynamic: bool,
    method: TokenEndpointAuth,
}

impl RegisteredClient {
    fn auth(&self) -> ClientAuth<'_> {
        ClientAuth {
            id: &self.id,
            method: self.method,
            secret: self.secret.as_deref().map(Vec::as_slice),
        }
    }
}

impl Attempt {
    async fn start(plan: &Plan, port: u16) -> Result<Self, McpError> {
        let redirect_uri = format!("http://127.0.0.1:{port}/callback");
        let client = register(plan, &redirect_uri).await?;
        let pkce = PkceS256::generate().map_err(|_| authorization("randomness is unavailable"))?;
        let state = generate_oauth_correlation_token()
            .map_err(|_| authorization("randomness is unavailable"))?;
        let authorization_url = build_resource_authorization_url(ResourceAuthorizeRequest {
            authorization_endpoint: &plan.metadata.authorization_endpoint,
            client_id: &client.id,
            redirect_uri: &redirect_uri,
            scope: plan.scope.as_deref(),
            code_challenge: pkce.challenge().as_str(),
            state: state.expose_secret(),
            resource: &plan.resource_indicator,
        })
        .map_err(|_| authorization("the authorization url could not be built"))?;
        Ok(Self {
            client,
            pkce,
            state,
            redirect_uri,
            authorization_url,
        })
    }
}

/// The client a login uses: the pre-registered one, else a Client ID
/// Metadata Document client the server supports, else a dynamic
/// registration for exactly `redirect_uri`.
async fn register(plan: &Plan, redirect_uri: &str) -> Result<RegisteredClient, McpError> {
    if let Some(client) = plan.settings.client() {
        let secret = client
            .secret
            .as_ref()
            .map(|resolve| resolve_secret(resolve, &plan.server))
            .transpose()?;
        return Ok(RegisteredClient {
            id: client.id.clone(),
            method: auth_method(&plan.metadata, secret.is_some()),
            secret,
            dynamic: false,
        });
    }
    if let Some(document) = &plan.metadata_document
        && plan.metadata.client_id_metadata_document_supported
    {
        return Ok(RegisteredClient {
            id: document.clone(),
            secret: None,
            dynamic: false,
            method: TokenEndpointAuth::None,
        });
    }
    let Some(endpoint) = &plan.metadata.registration_endpoint else {
        return Err(authorization(&format!(
            "authorization server {} offers no client registration this client can use; configure a client registered with it in advance",
            plan.metadata.issuer
        )));
    };
    let answer = plan
        .http
        .post_json(
            endpoint,
            dynamic_registration_request(&plan.client_name, redirect_uri),
            HeaderMap::new(),
        )
        .await?;
    if !answer.status.is_success() {
        return Err(authorization(&format!(
            "dynamic client registration at {} failed with HTTP {}",
            redact_query(endpoint),
            answer.status.as_u16()
        )));
    }
    let registration = parse_dynamic_registration(&answer.body)
        .map_err(|_| authorization("the dynamic client registration response is invalid"))?;
    let secret = registration
        .client_secret
        .map(|secret| Zeroizing::new(secret.expose_secret().as_bytes().to_vec()));
    Ok(RegisteredClient {
        id: registration.client_id,
        method: auth_method(&plan.metadata, secret.is_some()),
        dynamic: secret.is_some(),
        secret,
    })
}

/// HTTP Basic unless the server lists only `client_secret_post`.
fn auth_method(metadata: &AuthorizationServerMetadata, has_secret: bool) -> TokenEndpointAuth {
    let methods = &metadata.token_endpoint_auth_methods_supported;
    if !has_secret {
        TokenEndpointAuth::None
    } else if methods.iter().any(|method| method == "client_secret_post")
        && !methods.iter().any(|method| method == "client_secret_basic")
    {
        TokenEndpointAuth::ClientSecretPost
    } else {
        TokenEndpointAuth::ClientSecretBasic
    }
}

fn resolve_secret(
    resolve: &super::McpOAuthSecretResolver,
    server: &McpServerName,
) -> Result<Zeroizing<Vec<u8>>, McpError> {
    resolve()
        .map(|secret: SensitiveBytes| Zeroizing::new(secret.into_bytes().to_vec()))
        .map_err(|error| {
            authorization(&format!(
                "mcp server {server} oauth client secret could not be resolved: {error}"
            ))
        })
}

/// The client secret a stored login authenticates with: its dynamic
/// registration's, else the definition's pre-registered one for the same
/// client.
pub(crate) fn client_secret(
    settings: &McpOAuthSettings,
    stored: &StoredLogin,
) -> Result<Option<Zeroizing<Vec<u8>>>, McpError> {
    if let Some(secret) = &stored.client_secret {
        return Ok(Some(Zeroizing::new(secret.as_bytes().to_vec())));
    }
    match settings.client() {
        Some(client)
            if client.id == stored.client_id
                && settings.issuer() == Some(stored.issuer.as_str()) =>
        {
            client
                .secret
                .as_ref()
                .map(|resolve| {
                    resolve()
                        .map(|secret| Zeroizing::new(secret.into_bytes().to_vec()))
                        .map_err(|error| {
                            authorization(&format!(
                                "oauth client secret could not be resolved: {error}"
                            ))
                        })
                })
                .transpose()
        }
        _ => Ok(None),
    }
}

/// The store's current login fence and the scope a login for this endpoint
/// requested, which a new login keeps.
fn baseline(
    settings: &McpOAuthSettings,
    resource: &str,
) -> Result<(Option<CredentialFence>, Option<String>), McpError> {
    let Some(store) = open_store(settings.store())? else {
        return Ok((None, None));
    };
    match store.load() {
        Ok(None) => Ok((None, None)),
        Ok(Some(fence)) => {
            let scope = (fence.status() == CredentialStatus::Ready)
                .then(|| read_ready(&store, &fence).ok())
                .flatten()
                .filter(|stored| stored.belongs_to(&LoginExpectation::of(settings, resource)))
                .and_then(|stored| stored.scope.clone());
            Ok((Some(fence), scope))
        }
        Err(PrivateStoreError::WrittenByNewerBuild | PrivateStoreError::ForeignProfile) => {
            Err(authorization(
                "the stored login was written by a newer build; use that build, or log out locally first",
            ))
        }
        Err(error) => Err(store_failed("mcp login could not be read", error)),
    }
}

/// One unauthenticated `server/discover` POST: its 401 challenge, when any.
async fn probe(http: &OAuthHttp, endpoint: &McpHttpEndpoint) -> Result<BearerChallenge, McpError> {
    let mut headers = endpoint.literal_headers();
    headers.insert(
        HeaderName::from_static("mcp-protocol-version"),
        HeaderValue::from_static(DISCOVER_PROTOCOL_VERSION),
    );
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/json, text/event-stream"),
    );
    let request = http.request(
        http::Method::POST,
        endpoint.url(),
        headers,
        Bytes::from_static(br#"{"jsonrpc":"2.0","id":0,"method":"server/discover","params":{}}"#),
    )?;
    let answer = http.send(request).await?;
    Ok(if answer.status == StatusCode::UNAUTHORIZED {
        answer
            .headers
            .get_all(http::header::WWW_AUTHENTICATE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find_map(parse_bearer_challenge)
            .unwrap_or_default()
    } else {
        BearerChallenge::default()
    })
}

/// Show only a bounded URL without user information, query or fragment.
fn published_resource(body: &[u8]) -> String {
    let value = serde_json::from_slice::<serde_json::Value>(body).ok();
    let resource = value
        .as_ref()
        .and_then(|value| value.get("resource"))
        .and_then(serde_json::Value::as_str);
    let Some(mut url) = resource.and_then(|resource| url::Url::parse(resource).ok()) else {
        return "<invalid>".to_owned();
    };
    if !matches!(url.scheme(), "https" | "http") {
        return "<invalid>".to_owned();
    }
    let has_user_info = !url.username().is_empty() || url.password().is_some();
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    let display = resource.filter(|_| !has_user_info);
    display
        .map_or(url.as_str(), redact_query)
        .chars()
        .filter(|c| !c.is_control())
        .take(512)
        .collect()
}

async fn protected_resource(
    http: &OAuthHttp,
    server: &McpServerName,
    resource: &str,
    challenge: &BearerChallenge,
) -> Result<plakous_auth::oauth_workflow::ProtectedResourceMetadata, McpError> {
    let parse = |body: &[u8], url: &str| {
        parse_protected_resource_metadata(body, resource).map_err(|error| {
            authorization(&match error {
                OAuthWorkflowError::ResourceMismatch => format!(
                    "the resource metadata at {} publishes resource {} instead of {resource}; set url exactly as published",
                    redact_query(url),
                    published_resource(body)
                ),
                OAuthWorkflowError::MissingMetadata => format!(
                    "the resource metadata at {} names no authorization server",
                    redact_query(url)
                ),
                _ => format!("the resource metadata at {} is invalid", redact_query(url)),
            })
        })
    };
    if let Some(url) = &challenge.resource_metadata {
        if !http.admits(url) {
            return Err(authorization(&format!(
                "mcp server {server} names resource metadata outside the transport policy"
            )));
        }
        let answer = http.get(url).await?;
        if answer.status != StatusCode::OK {
            return Err(authorization(&format!(
                "the resource metadata at {} answered HTTP {}",
                redact_query(url),
                answer.status.as_u16()
            )));
        }
        return parse(&answer.body, url);
    }
    let urls = protected_resource_metadata_urls(resource)
        .map_err(|_| authorization("the server url is not a valid oauth resource"))?;
    for url in &urls {
        let answer = http.get(url).await?;
        if answer.status == StatusCode::OK {
            return parse(&answer.body, url);
        }
    }
    Err(authorization(&format!(
        "mcp server {server} publishes no protected-resource metadata"
    )))
}

async fn authorization_server(
    http: &OAuthHttp,
    server: &McpServerName,
    issuer: &str,
) -> Result<AuthorizationServerMetadata, McpError> {
    let urls = authorization_server_metadata_urls(issuer).map_err(|_| {
        authorization(&format!(
            "mcp server {server} names an invalid authorization server {issuer}"
        ))
    })?;
    if !http.admits(issuer) {
        return Err(authorization(&format!(
            "authorization server {issuer} is outside the transport policy"
        )));
    }
    for url in &urls {
        let answer = http.get(url).await?;
        if answer.status != StatusCode::OK {
            continue;
        }
        match parse_authorization_server_metadata(&answer.body, issuer) {
            Ok(metadata) => return Ok(metadata),
            Err(OAuthWorkflowError::PkceUnsupported) => {
                return Err(authorization(&format!(
                    "authorization server {issuer} does not advertise PKCE S256; refusing to log in"
                )));
            }
            Err(_) => {}
        }
    }
    Err(authorization(&format!(
        "authorization server {issuer} publishes no usable metadata"
    )))
}

/// Checks a callback against the attempt: its issuer first (a mismatch
/// discards the response, error fields included), then the redirect and
/// state. Returns the authorization code.
fn accept_callback(
    plan: &Plan,
    attempt: &Attempt,
    callback: &str,
    host: Option<&str>,
) -> Result<Zeroizing<String>, McpError> {
    let refused = || authorization("the login callback is invalid");
    let (rest, issuer) = take_issuer_parameter(callback).map_err(|_| refused())?;
    check_callback_issuer(
        issuer.as_deref().map(String::as_str),
        &plan.metadata.issuer,
        plan.metadata.authorization_response_iss_parameter_supported,
    )
    .map_err(|_| {
        authorization(
            "the authorization response did not come from the expected authorization server; nothing was sent to a token endpoint",
        )
    })?;
    let policy = CallbackQueryPolicy::standard();
    let outcome = match host {
        Some(host) => parse_loopback_callback_target(host, &rest, &attempt.redirect_uri, policy),
        None => parse_absolute_redirect_query(&rest, &attempt.redirect_uri, policy),
    }
    .map_err(|_| refused())?;
    match outcome {
        OAuthCallbackOutcome::AuthorizationCode { code, state }
            if plakous_auth::verify_callback_state(
                attempt.state.expose_secret(),
                state.as_str(),
            )
            .is_ok() =>
        {
            Ok(code)
        }
        OAuthCallbackOutcome::ProviderError { state }
            if plakous_auth::verify_callback_state(
                attempt.state.expose_secret(),
                state.as_str(),
            )
            .is_ok() =>
        {
            Err(authorization("the authorization server refused the login"))
        }
        _ => Err(refused()),
    }
}

async fn exchange(plan: &Plan, attempt: &Attempt, code: &str) -> Result<McpOAuthStatus, McpError> {
    let fields = resource_token_form(
        code,
        &attempt.redirect_uri,
        &attempt.client.id,
        attempt.pkce.verifier().expose_secret(),
        &plan.resource_indicator,
    );
    let request = plan.http.form_request(
        &plan.metadata.token_endpoint,
        &fields,
        &attempt.client.auth(),
    )?;
    let answer = plan.http.send(request).await?;
    if !answer.status.is_success() {
        return Err(token_refused(
            answer.status,
            oauth_error_code(&answer.body).as_deref(),
        ));
    }
    persist(plan, &attempt.client, &answer.body)
}

/// Stores a token response as the new login, replacing the previous one only
/// if it is still the one the login started from.
fn persist(
    plan: &Plan,
    client: &RegisteredClient,
    body: &[u8],
) -> Result<McpOAuthStatus, McpError> {
    let tokens = parse_resource_token_response(body)
        .map_err(|_| authorization("the token response is invalid"))?;
    let stored = StoredLogin {
        version: STORED_VERSION,
        resource: plan.resource.clone(),
        resource_indicator: plan.resource_indicator.clone(),
        issuer: plan.metadata.issuer.clone(),
        client_id: client.id.clone(),
        client_secret: client
            .dynamic
            .then_some(client.secret.as_deref())
            .flatten()
            .map(|secret| String::from_utf8_lossy(secret).into_owned()),
        token_endpoint_auth: client.method,
        access_token: tokens.access_token.expose_secret().to_owned(),
        lifetime: tokens.expires_in,
        renewed_after_rejection: false,
        expires_at: tokens
            .expires_in
            .map(|seconds| unix_now().saturating_add(seconds)),
        refresh_token: tokens
            .refresh_token
            .as_ref()
            .map(|token| token.expose_secret().to_owned()),
        scope: plan.scope.clone(),
        token_endpoint: plan.metadata.token_endpoint.clone(),
        revocation_endpoint: plan
            .metadata
            .revocation_endpoint
            .clone()
            .filter(|endpoint| plan.http.admits(endpoint)),
    };
    let store = open_store(plan.settings.store())?.ok_or_else(|| {
        authorization("the login directory does not exist; the embedder creates it before a login")
    })?;
    let generation = match &plan.baseline {
        Some(fence) => fence.generation().next(),
        None => CredentialGeneration::new(1),
    }
    .map_err(|error| store_failed("mcp login generation is exhausted", error))?;
    let record = stored.record(generation)?;
    let written = match &plan.baseline {
        Some(fence) => store.replace_selected_login(fence, record).map(|_| ()),
        None => store.initialize_selected(&record),
    };
    written.map_err(|error| match error {
        PrivateStoreError::FenceMismatch
        | PrivateStoreError::AlreadyInitialized
        | PrivateStoreError::RecordMissing => {
            authorization("the stored login changed while this login ran; nothing was replaced")
        }
        error => store_failed("mcp login could not be stored", error),
    })?;
    status(&plan.settings, &plan.resource)
}

fn token_refused(status: StatusCode, code: Option<&str>) -> McpError {
    authorization(&match code {
        Some(code) => format!(
            "the token endpoint refused the login: {code} (HTTP {})",
            status.as_u16()
        ),
        None => format!(
            "the token endpoint refused the login with HTTP {}",
            status.as_u16()
        ),
    })
}

/// A free loopback port, chosen and released, for a remote login's redirect.
async fn ephemeral_port() -> Result<u16, McpError> {
    let (listener, port) = bind_loopback_listener(&[0]).await.map_err(|error| {
        McpError::with_source(
            McpErrorKind::Authorization,
            "no loopback port is available for the login redirect",
            error,
        )
    })?;
    drop(listener);
    Ok(port)
}

fn oauth_endpoint(spec: &McpServerSpec) -> Result<(&McpHttpEndpoint, &McpOAuthSettings), McpError> {
    if let Transport::Http(endpoint) = spec.transport()
        && let Some(settings) = endpoint.oauth()
    {
        return Ok((endpoint, settings));
    }
    Err(authorization(&format!(
        "mcp server {} does not use oauth",
        spec.name()
    )))
}

fn authorization(message: &str) -> McpError {
    McpError::new(McpErrorKind::Authorization, message)
}

struct CallbackHooks<'a> {
    plan: &'a Plan,
    attempt: &'a Attempt,
    deadline: Instant,
}

impl BrowserLoopbackHooks for CallbackHooks<'_> {
    type Error = McpError;
    type Callback = Zeroizing<String>;
    type Preflight = ();
    type Output = McpOAuthStatus;

    fn deadline(&self) -> Instant {
        self.deadline
    }

    fn max_connections(&self) -> usize {
        // A browser may ask for a favicon first.
        4
    }

    fn deadline_error(&self) -> Self::Error {
        authorization("the login timed out waiting for the browser callback")
    }

    fn callback_error(&self) -> Self::Error {
        authorization("the login callback is invalid")
    }

    fn parse_request(
        &mut self,
        request: &[u8],
        _local_port: u16,
    ) -> Result<LoopbackConnectionEvent<Self::Callback, Self::Preflight>, Self::Error> {
        let text = std::str::from_utf8(request).map_err(|_| self.callback_error())?;
        let head = parse_http11_get_request(text).map_err(|_| self.callback_error())?;
        if head.target.split('?').next() != Some("/callback") {
            return Ok(LoopbackConnectionEvent::Preflight(()));
        }
        accept_callback(self.plan, self.attempt, head.target, Some(head.host))
            .map(LoopbackConnectionEvent::Callback)
    }

    fn preflight_response_bytes(&self, _preflight: &Self::Preflight) -> Vec<u8> {
        format_http11_text_response(404, "Not Found", "", &[]).into_bytes()
    }

    fn callback_response_bytes(&self, complete: Result<&Self::Output, &Self::Error>) -> Vec<u8> {
        match complete {
            Ok(_) => format_http11_text_response(200, "OK", CALLBACK_PAGE_COMPLETE, &[]),
            Err(_) => format_http11_text_response(400, "Bad Request", CALLBACK_PAGE_REFUSED, &[]),
        }
        .into_bytes()
    }

    async fn complete_callback(
        &mut self,
        code: Self::Callback,
    ) -> Result<Self::Output, Self::Error> {
        exchange(self.plan, self.attempt, &code).await
    }
}

struct RevokeHooks<'a> {
    http: &'a OAuthHttp,
    fence: CredentialFence,
    endpoint: String,
    secret: Option<Zeroizing<Vec<u8>>>,
}

#[allow(
    clippy::unused_async_trait_impl,
    reason = "hook methods implement the orchestrator's async trait; some bodies finish without awaiting"
)]
impl ClaimedRevokeHooks for RevokeHooks<'_> {
    type Error = McpError;
    type Output = ();

    async fn check_profile(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn fence_to_revoke(&mut self) -> Result<CredentialFence, Self::Error> {
        Ok(self.fence.clone())
    }

    fn build_revoke_request(
        &mut self,
        claim: &RefreshClaim,
    ) -> Result<plakous_http::Request, BuildRequestFailure<Self::Error>> {
        let stored = StoredLogin::decode(claim.payload().expose_secret(), &claim.fence())
            .map_err(BuildRequestFailure::Restore)?;
        let (token, hint) = match &stored.refresh_token {
            Some(token) => (token.as_str(), "refresh_token"),
            None => (stored.access_token.as_str(), "access_token"),
        };
        let fields = revocation_form(token, hint, &stored.client_id);
        let method = if self.secret.is_some() {
            stored.token_endpoint_auth
        } else {
            TokenEndpointAuth::None
        };
        self.http
            .form_request(
                &self.endpoint,
                &fields,
                &ClientAuth {
                    id: &stored.client_id,
                    method,
                    secret: self.secret.as_deref().map(Vec::as_slice),
                },
            )
            .map_err(BuildRequestFailure::Restore)
    }

    fn exchange_control(&mut self) -> ExchangeControl {
        ExchangeControl::new(Instant::now() + EXCHANGE_TIMEOUT)
    }

    async fn accept_revoke_response(
        &mut self,
        response: &mut ClaimedHttpResponse,
    ) -> Result<(), Self::Error> {
        if response.status().is_success() {
            Ok(())
        } else {
            Err(authorization(&format!(
                "the revocation endpoint answered HTTP {}",
                response.status().as_u16()
            )))
        }
    }

    async fn map_claimed_http_error(
        &mut self,
        _store: &PrivateCredentialStore,
        error: ClaimedHttpError,
    ) -> Self::Error {
        McpError::with_source(
            McpErrorKind::Authorization,
            format!("the revocation request failed: {error}"),
            error,
        )
    }

    fn map_store_error(&mut self, error: PrivateStoreError) -> Self::Error {
        store_failed("mcp login could not be revoked", error)
    }

    fn output_from_revoked(
        &mut self,
        _generation: CredentialGeneration,
    ) -> Result<Self::Output, Self::Error> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::published_resource;

    #[test]
    fn mismatch_resource_preserves_published_spelling_without_sensitive_parts() {
        assert_eq!(
            published_resource(br#"{"resource":"https://EXAMPLE.com/Path/?private=yes#secret"}"#),
            "https://EXAMPLE.com/Path/"
        );
        assert_eq!(
            published_resource(
                br#"{"resource":"https://user:secret@example.com/path?private=yes"}"#
            ),
            "https://example.com/path"
        );
        assert_eq!(
            published_resource(br#"{"resource":"invalid"}"#),
            "<invalid>"
        );
    }
}
