//! HTTP exchanges with a protected resource's authorization server.
//!
//! The transport policy of the MCP endpoint applies: `https://`, or plain
//! HTTP to a loopback literal only when the endpoint itself is loopback; no
//! proxy; no redirect, so a 3xx is just a status; bodies within 256 KiB and
//! each exchange within 30 s. These URLs come from operator-configured
//! servers, so no destination address policy applies.

use std::net::IpAddr;
use std::time::{Duration, Instant};

use base64::Engine as _;
use http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use plakous_auth::oauth_workflow::{MAX_RESOURCE_OAUTH_RESPONSE_BYTES, urlencoded_form_bytes};
use plakous_http::{Bytes, Client, ExchangeControl, Limits, Origin, Request};
use url::Url;
use zeroize::Zeroizing;

use super::credential::TokenEndpointAuth;
use crate::error::{McpError, McpErrorKind};

/// Bound of one authorization-server exchange.
pub(crate) const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);
const JSON: &str = "application/json";
const FORM: &str = "application/x-www-form-urlencoded";

/// One response, its body read within the bound.
pub(crate) struct Answer {
    pub(crate) status: StatusCode,
    pub(crate) headers: HeaderMap,
    pub(crate) body: Zeroizing<Vec<u8>>,
}

/// How the token endpoint authenticates the client, with the secret when
/// the client has one.
pub(crate) struct ClientAuth<'a> {
    pub(crate) id: &'a str,
    pub(crate) method: TokenEndpointAuth,
    pub(crate) secret: Option<&'a [u8]>,
}

/// The HTTP client of one endpoint's authorization exchanges.
#[derive(Clone)]
pub(crate) struct OAuthHttp {
    client: Client,
    loopback: bool,
}

impl OAuthHttp {
    /// Derives the exchange client from the pool's transport client;
    /// `loopback` admits plain HTTP to loopback literals.
    pub(crate) fn new(base: &Client, loopback: bool) -> Result<Self, McpError> {
        let limits = Limits {
            max_response_body_bytes: MAX_RESOURCE_OAUTH_RESPONSE_BYTES,
            response_head_timeout: EXCHANGE_TIMEOUT,
            response_body_timeout: EXCHANGE_TIMEOUT,
            ..Limits::default()
        };
        let client = base
            .with_limits(limits)
            .map_err(|error| {
                McpError::with_source(
                    McpErrorKind::Authorization,
                    "mcp oauth transport limits are invalid",
                    error,
                )
            })?
            .with_address_policy(None);
        Ok(Self { client, loopback })
    }

    pub(crate) const fn client(&self) -> &Client {
        &self.client
    }

    /// Whether `url` is one this client may reach.
    pub(crate) fn admits(&self, url: &str) -> bool {
        self.target(url).is_ok()
    }

    fn target(&self, url: &str) -> Result<(Origin, String), McpError> {
        let refused = || {
            McpError::new(
                McpErrorKind::Authorization,
                format!(
                    "oauth url {} must be https://{}",
                    redact_query(url),
                    if self.loopback {
                        ", or http:// to 127.0.0.1 or [::1]"
                    } else {
                        ""
                    }
                ),
            )
        };
        let parsed = Url::parse(url).map_err(|_| refused())?;
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(refused());
        }
        let host = parsed.host_str().ok_or_else(refused)?;
        let origin = match parsed.scheme() {
            "https" => {
                let port = parsed
                    .port()
                    .map(|port| format!(":{port}"))
                    .unwrap_or_default();
                Origin::parse(&format!("https://{host}{port}")).map_err(|_| refused())?
            }
            "http" if self.loopback => {
                let address = host
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .parse::<IpAddr>()
                    .map_err(|_| refused())?;
                Origin::loopback_http(address, parsed.port().unwrap_or(80))
                    .map_err(|_| refused())?
            }
            _ => return Err(refused()),
        };
        let mut path = parsed.path().to_owned();
        if let Some(query) = parsed.query() {
            path.push('?');
            path.push_str(query);
        }
        Ok((origin, path))
    }

    /// Builds one request to `url`.
    pub(crate) fn request(
        &self,
        method: Method,
        url: &str,
        mut headers: HeaderMap,
        body: Bytes,
    ) -> Result<Request, McpError> {
        let (origin, path) = self.target(url)?;
        headers
            .entry(ACCEPT)
            .or_insert(HeaderValue::from_static(JSON));
        Request::new(origin, method, &path, headers, body, self.client.limits()).map_err(|error| {
            McpError::with_source(
                McpErrorKind::Authorization,
                format!("oauth request to {} is invalid", redact_query(url)),
                error,
            )
        })
    }

    /// A form POST with the client's token-endpoint authentication.
    pub(crate) fn form_request(
        &self,
        url: &str,
        fields: &[(&str, &str)],
        client: &ClientAuth<'_>,
    ) -> Result<Request, McpError> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static(FORM));
        let secret = client
            .secret
            .map(|secret| Zeroizing::new(String::from_utf8_lossy(secret).into_owned()));
        let mut all = fields.to_vec();
        match (client.method, secret.as_deref()) {
            (TokenEndpointAuth::ClientSecretBasic, Some(secret)) => {
                let encode = |value: &str| {
                    url::form_urlencoded::byte_serialize(value.as_bytes()).collect::<String>()
                };
                let pair = Zeroizing::new(format!("{}:{}", encode(client.id), encode(secret)));
                let value = Zeroizing::new(format!(
                    "Basic {}",
                    base64::engine::general_purpose::STANDARD.encode(pair.as_bytes())
                ));
                let mut value = HeaderValue::from_str(&value).map_err(|_| {
                    McpError::new(
                        McpErrorKind::Authorization,
                        "oauth client secret is invalid",
                    )
                })?;
                value.set_sensitive(true);
                headers.insert(AUTHORIZATION, value);
            }
            (TokenEndpointAuth::ClientSecretPost, Some(secret)) => {
                all.push(("client_secret", secret));
            }
            _ => {}
        }
        let body = urlencoded_form_bytes(&all);
        self.request(Method::POST, url, headers, Bytes::copy_from_slice(&body))
    }

    /// Sends `request` and reads the body within the bound.
    pub(crate) async fn send(&self, request: Request) -> Result<Answer, McpError> {
        let control = ExchangeControl::new(Instant::now() + EXCHANGE_TIMEOUT);
        let response = self
            .client
            .execute(request, control)
            .await
            .map_err(|failure| {
                McpError::with_source(
                    McpErrorKind::Authorization,
                    format!("oauth exchange failed: {failure}"),
                    failure,
                )
            })?;
        let status = response.status();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect::<HeaderMap>();
        let mut body = response.into_body();
        let mut bytes = Zeroizing::new(Vec::new());
        while let Some(chunk) = body.next_chunk().await {
            let chunk = chunk.map_err(|failure| {
                McpError::with_source(
                    McpErrorKind::Authorization,
                    format!("oauth exchange failed: {failure}"),
                    failure,
                )
            })?;
            if bytes.len() + chunk.len() > MAX_RESOURCE_OAUTH_RESPONSE_BYTES {
                let _ = body.close().await;
                return Err(McpError::new(
                    McpErrorKind::Authorization,
                    "oauth response is over 256 KiB",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(Answer {
            status,
            headers,
            body: bytes,
        })
    }

    pub(crate) async fn get(&self, url: &str) -> Result<Answer, McpError> {
        self.send(self.request(Method::GET, url, HeaderMap::new(), Bytes::new())?)
            .await
    }

    pub(crate) async fn post_json(
        &self,
        url: &str,
        body: Vec<u8>,
        extra: HeaderMap,
    ) -> Result<Answer, McpError> {
        let mut headers = extra;
        headers.insert(CONTENT_TYPE, HeaderValue::from_static(JSON));
        self.send(self.request(Method::POST, url, headers, Bytes::from(body))?)
            .await
    }
}

/// A URL for operator text: its query, which may carry correlation values,
/// is dropped.
pub(crate) fn redact_query(url: &str) -> &str {
    url.split(['?', '#']).next().unwrap_or(url)
}
