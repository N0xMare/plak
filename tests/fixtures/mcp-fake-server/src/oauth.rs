//! A scripted OAuth authorization server and protected-resource gate,
//! served by the HTTP fixture beside the MCP endpoint (MCP authorization
//! 2026-07-28), and a scripted browser.
//!
//! The authorization server's issuer is the fixture's origin plus
//! [`OAuthConfig::issuer_path`]. It issues `at-<n>` access and `rt-<n>`
//! refresh tokens, checks PKCE S256 and records every request.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io;
use std::sync::{Mutex, OnceLock, PoisonError};

use base64::Engine as _;
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

/// Where the protected-resource metadata is found.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ResourceMetadataAt {
    /// Named by the 401 challenge's `resource_metadata`.
    #[default]
    Challenge,
    /// Only at the path-aware well-known URL.
    PathWellKnown,
    /// Only at the origin's well-known URL.
    RootWellKnown,
}

/// Where the authorization-server metadata is served.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ServerMetadataAt {
    /// RFC 8414, path inserted.
    #[default]
    OAuth,
    /// OIDC discovery, path inserted.
    OidcInserted,
    /// OIDC discovery, path appended.
    OidcAppended,
}

/// What the callback's `iss` carries.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CallbackIssuer {
    /// The issuer.
    #[default]
    Correct,
    /// Another issuer.
    Wrong,
    /// No `iss` at all.
    Absent,
}

/// The scripted behavior.
#[derive(Clone, Debug)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "each flag scripts one independent server capability"
)]
pub struct OAuthConfig {
    /// Where the resource metadata lives.
    pub resource_metadata: ResourceMetadataAt,
    /// The resource metadata's `resource`, an absolute URL or path on the fixture's origin;
    /// the MCP URL when `None`.
    pub resource: Option<String>,
    /// The issuer's path after the origin, such as `/tenant`.
    pub issuer_path: String,
    /// Where the server metadata lives.
    pub server_metadata: ServerMetadataAt,
    /// Whether `S256` is advertised.
    pub pkce: bool,
    /// Whether `authorization_response_iss_parameter_supported` is set.
    pub advertise_iss: bool,
    /// What the callback's `iss` carries.
    pub callback_issuer: CallbackIssuer,
    /// Whether dynamic registration is offered.
    pub registration: bool,
    /// Whether the device grant is offered.
    pub device: bool,
    /// Whether revocation is offered.
    pub revocation: bool,
    /// `scopes_supported` of both documents.
    pub scopes_supported: Vec<String>,
    /// A scope every MCP request's token must carry, else 403.
    pub required_scope: Option<String>,
    /// Seconds an access token lives; zero omits `expires_in`.
    pub access_lifetime: u64,
    /// Whether a refresh returns a new refresh token.
    pub rotate_refresh: bool,
    /// A pre-registered confidential client: id and secret, sent by HTTP
    /// Basic.
    pub confidential_client: Option<(String, String)>,
}

impl Default for OAuthConfig {
    fn default() -> Self {
        Self {
            resource_metadata: ResourceMetadataAt::Challenge,
            resource: None,
            issuer_path: String::new(),
            server_metadata: ServerMetadataAt::OAuth,
            pkce: true,
            advertise_iss: true,
            callback_issuer: CallbackIssuer::Correct,
            registration: true,
            device: false,
            revocation: true,
            scopes_supported: vec!["mcp:tools".to_owned(), "offline_access".to_owned()],
            required_scope: None,
            access_lifetime: 3600,
            rotate_refresh: true,
            confidential_client: None,
        }
    }
}

/// One request the authorization side saw.
#[derive(Clone, Debug)]
pub struct AuthRequest {
    /// The method.
    pub method: String,
    /// The path without query.
    pub path: String,
    /// Query and form fields, decoded, in order.
    pub fields: Vec<(String, String)>,
    /// The `Authorization` header, if any.
    pub authorization: Option<String>,
}

impl AuthRequest {
    /// The first value of field `name`.
    #[must_use]
    pub fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

#[derive(Clone)]
struct Grant {
    client_id: String,
    redirect_uri: String,
    challenge: String,
    scope: String,
}

#[derive(Default)]
struct AuthState {
    counter: u32,
    codes: HashMap<String, Grant>,
    access: HashMap<String, String>,
    refresh: HashMap<String, String>,
    device_polls: u32,
    requests: Vec<AuthRequest>,
    required_scope: Option<String>,
    resource_requests: u32,
    token_failures: u32,
    token_failure_status: Option<u16>,
    revocation_failure: bool,
    rejection_barrier: Option<(std::sync::Arc<tokio::sync::Barrier>, usize)>,
}

/// The authorization server and resource gate of one fixture.
pub struct OAuthGate {
    config: OAuthConfig,
    origin: OnceLock<String>,
    state: Mutex<AuthState>,
}

impl std::fmt::Debug for OAuthGate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("OAuthGate").finish_non_exhaustive()
    }
}

/// A response the gate answers itself.
pub struct Reply {
    /// The status.
    pub status: u16,
    /// Extra headers.
    pub headers: Vec<(&'static str, String)>,
    /// The body.
    pub body: String,
}

impl Reply {
    fn json(status: u16, body: &serde_json::Value) -> Self {
        Self {
            status,
            headers: vec![("content-type", "application/json".to_owned())],
            body: body.to_string(),
        }
    }

    fn status(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: String::new(),
        }
    }
}

impl OAuthGate {
    /// A gate scripted by `config`.
    #[must_use]
    pub fn new(config: OAuthConfig) -> Self {
        let required_scope = config.required_scope.clone();
        Self {
            config,
            origin: OnceLock::new(),
            state: Mutex::new(AuthState {
                required_scope,
                ..AuthState::default()
            }),
        }
    }

    pub(crate) fn set_origin(&self, origin: String) {
        let _ = self.origin.set(origin);
    }

    fn origin(&self) -> &str {
        self.origin.get().map_or("", String::as_str)
    }

    /// The issuer.
    #[must_use]
    pub fn issuer(&self) -> String {
        format!("{}{}", self.origin(), self.config.issuer_path)
    }

    /// Every authorization-side request, in order.
    #[must_use]
    pub fn requests(&self) -> Vec<AuthRequest> {
        self.lock().requests.clone()
    }

    /// Requests to `path`.
    #[must_use]
    pub fn requests_to(&self, path: &str) -> Vec<AuthRequest> {
        self.requests()
            .into_iter()
            .filter(|request| request.path == path)
            .collect()
    }

    /// MCP requests the gate admitted with a valid token.
    #[must_use]
    pub fn admitted(&self) -> u32 {
        self.lock().resource_requests
    }

    /// Invalidates every access token, as a server-side revocation would.
    pub fn reject_access_tokens(&self) {
        self.lock().access.clear();
    }

    /// Rejects the current tokens, withholding the first responses until all
    /// `parties` requests have presented the rejected generation.
    pub fn reject_access_tokens_together(&self, parties: usize) {
        let mut state = self.lock();
        state.access.clear();
        state.rejection_barrier = Some((
            std::sync::Arc::new(tokio::sync::Barrier::new(parties)),
            parties,
        ));
    }

    pub(crate) async fn wait_for_rejection(&self) {
        let barrier = {
            let mut state = self.lock();
            if let Some((barrier, remaining)) = state.rejection_barrier.as_mut() {
                let barrier = std::sync::Arc::clone(barrier);
                *remaining -= 1;
                if *remaining == 0 {
                    state.rejection_barrier = None;
                }
                Some(barrier)
            } else {
                None
            }
        };
        if let Some(barrier) = barrier {
            barrier.wait().await;
        }
    }

    /// Invalidates every refresh token, as a revoked grant would.
    pub fn reject_refresh_tokens(&self) {
        self.lock().refresh.clear();
    }

    /// Answers the next `count` token requests with HTTP 503.
    pub fn fail_token_requests(&self, count: u32) {
        self.fail_token_requests_with_status(count, 503);
    }

    /// Answers token requests with a chosen failure status.
    pub fn fail_token_requests_with_status(&self, count: u32, status: u16) {
        let mut state = self.lock();
        state.token_failures = count;
        state.token_failure_status = Some(status);
    }

    /// Makes revocation fail before it changes any grant.
    pub fn fail_revocation(&self, fail: bool) {
        self.lock().revocation_failure = fail;
    }

    /// Changes the scope MCP requests require.
    pub fn require_scope(&self, scope: Option<&str>) {
        self.lock().required_scope = scope.map(ToOwned::to_owned);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, AuthState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn record(
        &self,
        method: &str,
        path: &str,
        fields: &[(String, String)],
        authorization: Option<&str>,
    ) {
        self.lock().requests.push(AuthRequest {
            method: method.to_owned(),
            path: path.to_owned(),
            fields: fields.to_vec(),
            authorization: authorization.map(ToOwned::to_owned),
        });
    }

    fn resource_metadata_url(&self, mcp_path: &str) -> String {
        match self.config.resource_metadata {
            ResourceMetadataAt::Challenge => {
                format!("{}/custom/resource-metadata", self.origin())
            }
            ResourceMetadataAt::PathWellKnown => format!(
                "{}/.well-known/oauth-protected-resource{mcp_path}",
                self.origin()
            ),
            ResourceMetadataAt::RootWellKnown => {
                format!("{}/.well-known/oauth-protected-resource", self.origin())
            }
        }
    }

    fn server_metadata_path(&self) -> String {
        let path = &self.config.issuer_path;
        match self.config.server_metadata {
            ServerMetadataAt::OAuth => format!("/.well-known/oauth-authorization-server{path}"),
            ServerMetadataAt::OidcInserted => format!("/.well-known/openid-configuration{path}"),
            ServerMetadataAt::OidcAppended => format!("{path}/.well-known/openid-configuration"),
        }
    }

    /// Checks one MCP request's bearer: `Err` is the 401 or 403 to answer.
    pub(crate) fn check(&self, mcp_path: &str, authorization: Option<&str>) -> Result<(), Reply> {
        let token = authorization.and_then(|value| value.strip_prefix("Bearer "));
        let mut state = self.lock();
        let Some(scope) = token.and_then(|token| state.access.get(token)).cloned() else {
            drop(state);
            let mut challenge = String::from("Bearer");
            if self.config.resource_metadata == ResourceMetadataAt::Challenge {
                let _ = write!(
                    challenge,
                    " resource_metadata=\"{}\"",
                    self.resource_metadata_url(mcp_path)
                );
            }
            if token.is_some() {
                challenge.push_str(", error=\"invalid_token\"");
            }
            return Err(Reply {
                status: 401,
                headers: vec![("www-authenticate", challenge)],
                body: String::new(),
            });
        };
        if let Some(required) = state.required_scope.clone()
            && !scope.split(' ').any(|granted| granted == required)
        {
            let quoted = required.replace('\\', "\\\\").replace('"', "\\\"");
            return Err(Reply {
                status: 403,
                headers: vec![(
                    "www-authenticate",
                    format!("Bearer error=\"insufficient_scope\", scope=\"{quoted}\""),
                )],
                body: String::new(),
            });
        }
        state.resource_requests += 1;
        Ok(())
    }

    /// Answers an authorization-side request, or `None` for the MCP path.
    pub(crate) fn handle(
        &self,
        method: &str,
        path: &str,
        query: Option<&str>,
        body: &[u8],
        authorization: Option<&str>,
        mcp_path: &str,
    ) -> Option<Reply> {
        if path == mcp_path {
            return None;
        }
        let mut fields = decode(query.unwrap_or(""));
        if method == "POST" && !body.is_empty() && !body.starts_with(b"{") {
            fields.extend(decode(&String::from_utf8_lossy(body)));
        }
        self.record(method, path, &fields, authorization);
        let field = |name: &str| {
            fields
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        if path
            == self
                .resource_metadata_url(mcp_path)
                .trim_start_matches(self.origin())
        {
            return Some(Reply::json(
                200,
                &serde_json::json!({
                    "resource": match self.config.resource.as_deref() {
                        Some(resource) if resource.contains("://") => resource.to_owned(),
                        resource => format!("{}{}", self.origin(), resource.unwrap_or(mcp_path)),
                    },
                    "authorization_servers": [self.issuer()],
                    "scopes_supported": self.config.scopes_supported,
                }),
            ));
        }
        if path == self.server_metadata_path() {
            return Some(Reply::json(200, &self.server_metadata()));
        }
        let issuer_path = &self.config.issuer_path;
        let endpoint = path.strip_prefix(issuer_path.as_str()).unwrap_or(path);
        Some(match (method, endpoint) {
            ("GET", "/authorize") => self.authorize(&field),
            ("POST", "/register") => {
                let request: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
                self.record(
                    "POST",
                    "/register-body",
                    &[(
                        "redirect_uris".to_owned(),
                        request["redirect_uris"].to_string(),
                    )],
                    None,
                );
                let id = {
                    let mut state = self.lock();
                    state.counter += 1;
                    format!("dyn-{}", state.counter)
                };
                Reply::json(201, &serde_json::json!({"client_id": id}))
            }
            ("POST", "/token") => self.token(&field, authorization),
            ("POST", "/device") => Reply::json(
                200,
                &serde_json::json!({
                    "device_code": "device-1",
                    "user_code": "WDJB-MJHT",
                    "verification_uri": format!("{}/device-verify", self.issuer()),
                    "expires_in": 600,
                    "interval": 1,
                }),
            ),
            ("POST", "/revoke") => {
                if self.lock().revocation_failure {
                    return Some(Reply::status(503));
                }
                if let Some(token) = field("token") {
                    let mut state = self.lock();
                    state.refresh.remove(&token);
                    state.access.remove(&token);
                }
                Reply::status(200)
            }
            _ => Reply::status(404),
        })
    }

    fn server_metadata(&self) -> serde_json::Value {
        let issuer = self.issuer();
        let mut document = serde_json::json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{issuer}/authorize"),
            "token_endpoint": format!("{issuer}/token"),
            "scopes_supported": self.config.scopes_supported,
            "authorization_response_iss_parameter_supported": self.config.advertise_iss,
            "token_endpoint_auth_methods_supported": ["none", "client_secret_basic"],
        });
        if self.config.pkce {
            document["code_challenge_methods_supported"] = serde_json::json!(["S256"]);
        }
        if self.config.registration {
            document["registration_endpoint"] = format!("{issuer}/register").into();
        }
        if self.config.device {
            document["device_authorization_endpoint"] = format!("{issuer}/device").into();
        }
        if self.config.revocation {
            document["revocation_endpoint"] = format!("{issuer}/revoke").into();
        }
        document
    }

    fn authorize(&self, field: &dyn Fn(&str) -> Option<String>) -> Reply {
        let (Some(client_id), Some(redirect_uri), Some(challenge), Some(state_value)) = (
            field("client_id"),
            field("redirect_uri"),
            field("code_challenge"),
            field("state"),
        ) else {
            return Reply::status(400);
        };
        let code = {
            let mut state = self.lock();
            state.counter += 1;
            let code = format!("code-{}", state.counter);
            state.codes.insert(
                code.clone(),
                Grant {
                    client_id,
                    redirect_uri: redirect_uri.clone(),
                    challenge,
                    scope: field("scope").unwrap_or_default(),
                },
            );
            code
        };
        let mut location = format!("{redirect_uri}?code={code}&state={}", encode(&state_value));
        match self.config.callback_issuer {
            CallbackIssuer::Correct => {
                let _ = write!(location, "&iss={}", encode(&self.issuer()));
            }
            CallbackIssuer::Wrong => {
                let _ = write!(location, "&iss={}", encode("https://evil.example.com"));
            }
            CallbackIssuer::Absent => {}
        }
        Reply {
            status: 302,
            headers: vec![("location", location)],
            body: String::new(),
        }
    }

    fn token(&self, field: &dyn Fn(&str) -> Option<String>, authorization: Option<&str>) -> Reply {
        let error = |code: &str| Reply::json(400, &serde_json::json!({"error": code}));
        {
            let mut state = self.lock();
            if state.token_failures > 0 {
                state.token_failures -= 1;
                return Reply::status(state.token_failure_status.unwrap_or(503));
            }
        }
        if let Some((id, secret)) = &self.config.confidential_client {
            let expected = format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("{id}:{secret}"))
            );
            if authorization != Some(expected.as_str()) {
                return Reply::json(401, &serde_json::json!({"error": "invalid_client"}));
            }
        }
        let scope = match field("grant_type").as_deref() {
            Some("authorization_code") => {
                let Some(grant) = field("code").and_then(|code| self.lock().codes.remove(&code))
                else {
                    return error("invalid_grant");
                };
                let verifier = field("code_verifier").unwrap_or_default();
                let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .encode(Sha256::digest(verifier.as_bytes()));
                if challenge != grant.challenge
                    || field("redirect_uri").as_deref() != Some(grant.redirect_uri.as_str())
                    || field("client_id").as_deref() != Some(grant.client_id.as_str())
                {
                    return error("invalid_grant");
                }
                grant.scope
            }
            Some("refresh_token") => {
                let token = field("refresh_token").unwrap_or_default();
                let found = if self.config.rotate_refresh {
                    self.lock().refresh.remove(&token)
                } else {
                    self.lock().refresh.get(&token).cloned()
                };
                let Some(scope) = found else {
                    return error("invalid_grant");
                };
                if !self.config.rotate_refresh {
                    return self.issue(scope, false);
                }
                scope
            }
            Some("urn:ietf:params:oauth:grant-type:device_code") => {
                let mut state = self.lock();
                state.device_polls += 1;
                if state.device_polls < 2 {
                    return error("authorization_pending");
                }
                drop(state);
                self.config.scopes_supported.join(" ")
            }
            _ => return error("unsupported_grant_type"),
        };
        self.issue(scope, true)
    }

    fn issue(&self, scope: String, new_refresh: bool) -> Reply {
        let mut state = self.lock();
        state.counter += 1;
        let access = format!("at-{}", state.counter);
        state.access.insert(access.clone(), scope.clone());
        let mut body = serde_json::json!({
            "access_token": access,
            "token_type": "Bearer",
            "expires_in": self.config.access_lifetime,
            "scope": scope,
        });
        if self.config.access_lifetime == 0
            && let Some(body) = body.as_object_mut()
        {
            body.remove("expires_in");
        }
        if new_refresh {
            let refresh = format!("rt-{}", state.counter);
            state.refresh.insert(refresh.clone(), scope);
            body["refresh_token"] = refresh.into();
        }
        Reply::json(200, &body)
    }
}

fn decode(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|field| !field.is_empty())
        .map(|field| {
            let (key, value) = field.split_once('=').unwrap_or((field, ""));
            (percent_decode(key), percent_decode(value))
        })
        .collect()
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => decoded.push(b' '),
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        decoded.push(byte);
                        index += 2;
                    }
                    Err(_) => decoded.push(b'%'),
                }
            }
            byte => decoded.push(byte),
        }
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

/// A scripted browser: one HTTP/1.1 GET to a loopback `http://` URL.
/// Returns the status and the `Location` header, if any.
///
/// # Errors
///
/// Returns a malformed URL or the connection error.
pub async fn browse(url: &str) -> io::Result<(u16, Option<String>)> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "not a loopback http url");
    let rest = url.strip_prefix("http://").ok_or_else(invalid)?;
    let (authority, target) = rest.split_at(rest.find('/').ok_or_else(invalid)?);
    let mut stream = TcpStream::connect(authority).await?;
    stream
        .write_all(
            format!("GET {target} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await?;
    let text = String::from_utf8_lossy(&response);
    let status = text
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no status"))?;
    let location = text
        .lines()
        .take_while(|line| !line.is_empty())
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("location")
                .then(|| value.trim().to_owned())
        });
    Ok((status, location))
}
