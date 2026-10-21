//! The streamable HTTP transport over `plakous-http`.
//!
//! One TLS, redirect and proxy policy serves providers and MCP alike: HTTPS
//! origins, or plain HTTP to a loopback literal, never a redirect and never a
//! proxy. Configured server URLs are operator input, so no destination
//! address policy applies to them. Secret headers live only in the client
//! built at each server start; rmcp never sees them.

use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::stream::{self, BoxStream, Stream, StreamExt as _};
use http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, WWW_AUTHENTICATE};
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use plakous_auth::oauth_workflow::parse_bearer_challenge;
use plakous_http::{
    Bytes, CancellationToken, Client, ErrorKind, ExchangeControl, Failure, Limits, Origin, Request,
    ResponseBody, SensitiveBytes, SseDecoder, SseLimits, sensitive_header_value,
};
use plakous_tools::McpServerName;
use rmcp::ServiceError;
use rmcp::model::{ClientJsonRpcMessage, ClientRequest, ErrorData, ServerJsonRpcMessage};
use rmcp::service::ClientInitializeError;
use rmcp::transport::streamable_http_client::{
    SseError, StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
};
use sse_stream::Sse;

use crate::error::{McpError, McpErrorKind};
use crate::oauth::{Bearer, McpOAuthSettings, OAuthHttp, OAuthSession};

/// Largest server-sent event the client accepts.
pub const MAX_SSE_EVENT_BYTES: usize = 4 * 1024 * 1024;
/// Largest response body one HTTP exchange may carry.
const MAX_RESPONSE_BODY_BYTES: usize = 64 * 1024 * 1024;

const SESSION_ID: HeaderName = HeaderName::from_static("mcp-session-id");
const LAST_EVENT_ID: HeaderName = HeaderName::from_static("last-event-id");
const EVENT_STREAM: &str = "text/event-stream";
const JSON: &str = "application/json";

/// Headers the transport owns; a definition may not set them.
const RESERVED_HEADERS: [&str; 6] = [
    "accept",
    "content-type",
    "mcp-session-id",
    "mcp-protocol-version",
    "last-event-id",
    "host",
];

/// Which request header a resolved secret fills.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum McpHttpSecret {
    /// `Authorization: Bearer <secret>`.
    Bearer,
    /// The named header carries the secret as its whole value.
    Header(HeaderName),
}

/// Resolves an HTTP server's secret headers at each start.
///
/// Called again on every restart, so secret values are not retained by the
/// pool between starts.
pub type McpHttpSecretResolver = Arc<
    dyn Fn() -> Result<Vec<(McpHttpSecret, SensitiveBytes)>, Box<dyn Error + Send + Sync>>
        + Send
        + Sync,
>;

/// One frozen streamable HTTP endpoint: its URL, literal headers and the
/// slots of its secret headers.
#[derive(Clone)]
pub struct McpHttpEndpoint {
    url: Arc<str>,
    origin: Origin,
    path_and_query: String,
    headers: Vec<(HeaderName, HeaderValue)>,
    secret_slots: Vec<McpHttpSecret>,
    secrets: Option<McpHttpSecretResolver>,
    oauth: Option<McpOAuthSettings>,
}

impl McpHttpEndpoint {
    /// An endpoint at `url`: `https://`, or `http://` only to `127.0.0.1` or
    /// `[::1]`, with no credentials or fragment.
    ///
    /// # Errors
    ///
    /// Returns a definition error for any other URL.
    pub fn new(url: &str) -> Result<Self, McpError> {
        let invalid = |reason: &str| {
            McpError::new(McpErrorKind::Definition, format!("mcp server url {reason}"))
        };
        if url.contains('#') {
            return Err(invalid("cannot carry a fragment"));
        }
        let uri = url
            .parse::<Uri>()
            .map_err(|_| invalid("is not an absolute URL"))?;
        let authority = uri
            .authority()
            .ok_or_else(|| invalid("is not an absolute URL"))?;
        if authority.as_str().contains('@') {
            return Err(invalid("cannot carry credentials"));
        }
        let origin = match uri.scheme_str() {
            Some("https") => Origin::parse(&format!("https://{authority}")),
            Some("http") => {
                let host = authority.host();
                let address = host
                    .strip_prefix('[')
                    .and_then(|host| host.strip_suffix(']'))
                    .unwrap_or(host)
                    .parse::<IpAddr>()
                    .map_err(|_| invalid("must be https://, or http:// to 127.0.0.1 or [::1]"))?;
                Origin::loopback_http(address, authority.port_u16().unwrap_or(80))
            }
            _ => {
                return Err(invalid(
                    "must be https://, or http:// to 127.0.0.1 or [::1]",
                ));
            }
        }
        .map_err(|_| invalid("must be https://, or http:// to 127.0.0.1 or [::1]"))?;
        let path_and_query = uri
            .path_and_query()
            .map_or_else(|| "/".to_owned(), ToString::to_string);
        Ok(Self {
            url: url.into(),
            origin,
            path_and_query,
            headers: Vec::new(),
            secret_slots: Vec::new(),
            secrets: None,
            oauth: None,
        })
    }

    /// Declares literal, non-secret request headers.
    ///
    /// # Errors
    ///
    /// Refuses `Authorization`, a transport-owned header, or a name given
    /// twice.
    pub fn with_headers(
        mut self,
        headers: impl IntoIterator<Item = (HeaderName, HeaderValue)>,
    ) -> Result<Self, McpError> {
        self.headers = headers.into_iter().collect();
        for (name, _) in &self.headers {
            if *name == AUTHORIZATION {
                return Err(definition(
                    "headers cannot carry Authorization; declare it as a secret",
                ));
            }
        }
        self.check_names()?;
        Ok(self)
    }

    /// Declares the secret headers and how to resolve their values.
    ///
    /// The slots are part of the definition digest; the values are not.
    ///
    /// # Errors
    ///
    /// Refuses a slot given twice, an `Authorization` header beside
    /// [`McpHttpSecret::Bearer`], a transport-owned header, or a header that
    /// is also a literal one.
    pub fn with_secret_headers(
        mut self,
        slots: impl IntoIterator<Item = McpHttpSecret>,
        resolver: McpHttpSecretResolver,
    ) -> Result<Self, McpError> {
        self.secret_slots = slots.into_iter().collect();
        self.secrets = Some(resolver);
        self.check_names()?;
        Ok(self)
    }

    /// Authenticates with OAuth under `settings` (MCP authorization)
    /// instead of a bearer secret.
    ///
    /// # Errors
    ///
    /// Refuses a URL with a query, a [`McpHttpSecret::Bearer`] slot, and a
    /// pre-registered client without an issuer pin.
    pub fn with_oauth(mut self, settings: McpOAuthSettings) -> Result<Self, McpError> {
        if self.path_and_query.contains('?') {
            return Err(definition("url cannot carry a query with oauth"));
        }
        settings.check()?;
        self.oauth = Some(settings);
        self.check_names()?;
        Ok(self)
    }

    /// Returns the configured URL.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The literal headers, which an OAuth discovery probe carries too.
    pub(crate) fn literal_headers(&self) -> HeaderMap {
        self.headers.iter().cloned().collect()
    }

    /// Whether the endpoint is plain HTTP to a loopback literal.
    pub(crate) const fn is_loopback(&self) -> bool {
        self.origin.is_loopback_http()
    }

    pub(crate) const fn oauth(&self) -> Option<&McpOAuthSettings> {
        self.oauth.as_ref()
    }

    /// The canonical server URI an OAuth login binds to.
    pub(crate) fn canonical_resource(&self) -> Result<String, McpError> {
        plakous_auth::oauth_workflow::canonical_resource_uri(&self.url)
            .map_err(|_| definition("url is not a valid oauth resource"))
    }

    fn check_names(&self) -> Result<(), McpError> {
        if self.oauth.is_some() && self.secret_slots.contains(&McpHttpSecret::Bearer) {
            return Err(definition("oauth cannot be combined with a bearer secret"));
        }
        let names = self
            .headers
            .iter()
            .map(|(name, _)| name)
            .chain(self.secret_slots.iter().map(|slot| match slot {
                McpHttpSecret::Bearer => &AUTHORIZATION,
                McpHttpSecret::Header(name) => name,
            }))
            .collect::<Vec<_>>();
        for (index, name) in names.iter().enumerate() {
            if RESERVED_HEADERS.contains(&name.as_str()) {
                return Err(definition(&format!(
                    "header {name} is set by the transport"
                )));
            }
            if names[..index].contains(name) {
                return Err(definition(&format!("header {name} is declared twice")));
            }
        }
        Ok(())
    }

    pub(crate) fn digest_parts(&self, parts: &mut Vec<Vec<u8>>) {
        parts.push(self.url.as_bytes().to_vec());
        parts.push(self.headers.len().to_be_bytes().to_vec());
        for (name, value) in &self.headers {
            parts.push(name.as_str().as_bytes().to_vec());
            parts.push(value.as_bytes().to_vec());
        }
        parts.push(self.secret_slots.len().to_be_bytes().to_vec());
        for slot in &self.secret_slots {
            parts.push(match slot {
                McpHttpSecret::Bearer => b"bearer".to_vec(),
                McpHttpSecret::Header(name) => [b"header:", name.as_str().as_bytes()].concat(),
            });
        }
        if let Some(oauth) = &self.oauth {
            oauth.digest_parts(parts);
        }
    }

    /// Builds the client for one server start, resolving secrets now; an
    /// OAuth endpoint reads its login at each request.
    pub(crate) fn client(
        &self,
        server: &McpServerName,
        base: &Client,
        exchange_timeout: Duration,
    ) -> Result<PlakousStreamableClient, McpError> {
        let mut headers = HeaderMap::new();
        for (name, value) in &self.headers {
            headers.insert(name.clone(), value.clone());
        }
        let resolved = match &self.secrets {
            Some(resolve) => resolve().map_err(|error| {
                McpError::new(
                    McpErrorKind::Definition,
                    format!("mcp server {server} secrets could not be resolved: {error}"),
                )
            })?,
            None => Vec::new(),
        };
        let exact = resolved.len() == self.secret_slots.len()
            && self
                .secret_slots
                .iter()
                .all(|slot| resolved.iter().filter(|(found, _)| found == slot).count() == 1);
        if !exact {
            return Err(McpError::new(
                McpErrorKind::Definition,
                format!(
                    "mcp server {server} secret resolution did not return exactly its declared headers"
                ),
            ));
        }
        for (slot, value) in resolved {
            let (name, value) = match slot {
                McpHttpSecret::Bearer => {
                    let secret = value.into_bytes();
                    let mut bearer = Vec::with_capacity(7 + secret.len());
                    bearer.extend_from_slice(b"Bearer ");
                    bearer.extend_from_slice(&secret);
                    (AUTHORIZATION, SensitiveBytes::new(bearer))
                }
                McpHttpSecret::Header(name) => (name, value),
            };
            let value = sensitive_header_value(value).map_err(|_| {
                McpError::new(
                    McpErrorKind::Definition,
                    format!(
                        "mcp server {server} secret for header {name} is not a valid header value"
                    ),
                )
            })?;
            headers.insert(name, value);
        }
        let limits = Limits {
            max_response_body_bytes: MAX_RESPONSE_BODY_BYTES,
            response_head_timeout: exchange_timeout,
            response_body_timeout: exchange_timeout,
            ..Limits::default()
        };
        let http = base
            .with_limits(limits)
            .map_err(|error| {
                McpError::with_source(
                    McpErrorKind::Definition,
                    format!("mcp server {server} transport limits are invalid"),
                    error,
                )
            })?
            .with_address_policy(None)
            .pooled();
        let oauth = match &self.oauth {
            Some(settings) => Some(Arc::new(OAuthSession::new(
                server.clone(),
                settings.clone(),
                self.canonical_resource()?,
                OAuthHttp::new(base, self.is_loopback())?,
            ))),
            None => None,
        };
        Ok(PlakousStreamableClient {
            inner: Arc::new(ClientState {
                http,
                origin: self.origin.clone(),
                path_and_query: self.path_and_query.clone(),
                headers,
                exchange_timeout,
                oauth,
            }),
        })
    }
}

impl fmt::Debug for McpHttpEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Http")
            .field("origin", &self.origin.host())
            .field(
                "headers",
                &self
                    .headers
                    .iter()
                    .map(|(name, _)| name)
                    .collect::<Vec<_>>(),
            )
            .field("secret_slots", &self.secret_slots)
            .field("oauth", &self.oauth)
            .finish_non_exhaustive()
    }
}

fn definition(reason: &str) -> McpError {
    McpError::new(McpErrorKind::Definition, format!("mcp server {reason}"))
}

/// rmcp's [`StreamableHttpClient`] over `plakous-http`.
#[derive(Clone)]
pub(crate) struct PlakousStreamableClient {
    inner: Arc<ClientState>,
}

struct ClientState {
    http: Client,
    origin: Origin,
    path_and_query: String,
    /// Literal and resolved secret headers; secret values are marked
    /// sensitive and zeroized with their last clone.
    headers: HeaderMap,
    exchange_timeout: Duration,
    oauth: Option<Arc<OAuthSession>>,
}

/// One failure of the MCP HTTP client. Never carries header values or
/// response bodies.
#[derive(Debug)]
pub(crate) enum McpHttpError {
    /// The request could not be built; nothing was sent.
    Request,
    /// The transport failed.
    Transport(Failure),
    /// The server answered with a status the transport does not accept.
    Status(StatusCode),
    /// A server-sent event broke the framing or its size bound.
    Sse(plakous_http::SseError),
    /// The message could not be encoded; nothing was sent.
    Encode,
    /// The OAuth credentials were missing or refused; the text names the
    /// remedy, and `renewed` says a 401 renewed them.
    Authorization { text: String, renewed: bool },
}

impl fmt::Display for McpHttpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Request => formatter.write_str("the mcp http request is invalid"),
            Self::Transport(failure) => failure.fmt(formatter),
            Self::Status(status) => write!(formatter, "server answered HTTP {}", status.as_u16()),
            Self::Sse(error) => error.fmt(formatter),
            Self::Encode => formatter.write_str("the mcp message could not be encoded"),
            Self::Authorization { text, .. } => formatter.write_str(text),
        }
    }
}

impl Error for McpHttpError {}

type HttpResult<T> = Result<T, StreamableHttpError<McpHttpError>>;

impl PlakousStreamableClient {
    fn request(
        &self,
        method: Method,
        mut headers: HeaderMap,
        protocol_headers: HashMap<HeaderName, HeaderValue>,
        session_id: Option<&str>,
        body: Bytes,
        bearer: Option<&Bearer>,
    ) -> HttpResult<Request> {
        for (name, value) in protocol_headers {
            // rmcp supplies the protocol version and standard request
            // headers; the session and stream headers are this client's.
            if name != "mcp-protocol-version" && RESERVED_HEADERS.contains(&name.as_str()) {
                return Err(StreamableHttpError::ReservedHeaderConflict(
                    name.to_string(),
                ));
            }
            headers.insert(name, value);
        }
        if let Some(session_id) = session_id {
            headers.insert(
                SESSION_ID,
                HeaderValue::from_str(session_id)
                    .map_err(|_| StreamableHttpError::Client(McpHttpError::Request))?,
            );
        }
        for (name, value) in &self.inner.headers {
            headers.insert(name.clone(), value.clone());
        }
        if let Some(bearer) = bearer {
            headers.insert(AUTHORIZATION, bearer.header.clone());
        }
        Request::new(
            self.inner.origin.clone(),
            method,
            &self.inner.path_and_query,
            headers,
            body,
            self.inner.http.limits(),
        )
        .map_err(|_| StreamableHttpError::Client(McpHttpError::Request))
    }

    async fn execute(&self, request: Request) -> HttpResult<plakous_http::Response> {
        // The exchange deadline bounds the response head; each body pull is
        // bounded by the client's idle limit. rmcp's own request timeout
        // drops the exchange when a call runs out of time.
        let control = ExchangeControl::streaming(
            Instant::now() + self.inner.exchange_timeout,
            CancellationToken::new(),
        );
        self.inner
            .http
            .execute(request, control)
            .await
            .map_err(|failure| StreamableHttpError::Client(McpHttpError::Transport(failure)))
    }

    /// The OAuth bearer of the next request, `None` without OAuth.
    async fn bearer(&self) -> HttpResult<Option<Bearer>> {
        match &self.inner.oauth {
            Some(session) => session.bearer().await.map(Some).map_err(|text| {
                StreamableHttpError::Client(McpHttpError::Authorization {
                    text,
                    renewed: false,
                })
            }),
            None => Ok(None),
        }
    }

    /// Settles an OAuth endpoint's 401 or 403 and closes the response: a
    /// 401 renews the credentials once, a 403 names the step-up login.
    /// Nothing is re-sent. Any other response passes through.
    async fn oauth_refusal(
        &self,
        response: plakous_http::Response,
        bearer: Option<&Bearer>,
    ) -> HttpResult<plakous_http::Response> {
        let status = response.status();
        let (Some(session), Some(bearer)) = (&self.inner.oauth, bearer) else {
            return Ok(response);
        };
        if status.is_success()
            && let Err(error) = session.accepted(bearer.generation).await
        {
            // The request succeeded; a local store failure must not report
            // that a potentially mutating call did not run.
            tracing::warn!(%error, "mcp login confirmation was not stored");
        }
        if !matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            return Ok(response);
        }
        let challenge = response
            .headers()
            .iter()
            .filter(|(name, _)| **name == WWW_AUTHENTICATE)
            .filter_map(|(_, value)| value.to_str().ok())
            .find_map(parse_bearer_challenge);
        let _ = response.into_body().close().await;
        let (text, renewed) = if status == StatusCode::UNAUTHORIZED {
            session.rejected(bearer.generation).await
        } else {
            (session.forbidden(challenge.as_ref()), false)
        };
        Err(StreamableHttpError::Client(McpHttpError::Authorization {
            text,
            renewed,
        }))
    }
}

impl StreamableHttpClient for PlakousStreamableClient {
    type Error = McpHttpError;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> HttpResult<StreamableHttpPostResponse> {
        self.post_message_with_max_sse_event_size(
            uri,
            message,
            session_id,
            auth_header,
            custom_headers,
            MAX_SSE_EVENT_BYTES,
        )
        .await
    }

    async fn post_message_with_max_sse_event_size(
        &self,
        _uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> HttpResult<StreamableHttpPostResponse> {
        let body = serde_json::to_vec(&message)
            .map_err(|_| StreamableHttpError::Client(McpHttpError::Encode))?;
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static(JSON));
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("text/event-stream, application/json"),
        );
        let bearer = self.bearer().await?;
        let request = self.request(
            Method::POST,
            headers,
            custom_headers,
            session_id.as_deref(),
            Bytes::from(body),
            bearer.as_ref(),
        )?;
        let response = self
            .oauth_refusal(self.execute(request).await?, bearer.as_ref())
            .await?;
        let status = response.status();
        if matches!(status, StatusCode::ACCEPTED | StatusCode::NO_CONTENT) {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        let content_type = response
            .headers()
            .get(&CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let new_session = response
            .headers()
            .get(&SESSION_ID)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let mut body = response.into_body();
        if !status.is_success() {
            if (status.is_client_error() || status.is_server_error())
                && !matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
            {
                let bytes = collect(&mut body).await?;
                if content_type.as_deref().is_some_and(|value| {
                    value
                        .split(';')
                        .next()
                        .is_some_and(|kind| kind.trim().eq_ignore_ascii_case(JSON))
                }) && let Some(error) = correlated_error(&message, &bytes)
                {
                    return Ok(StreamableHttpPostResponse::Json(
                        ServerJsonRpcMessage::Error(error),
                        new_session,
                    ));
                }
                if status.is_client_error()
                    && session_id.is_none()
                    && let Some(rejection) = discover_rejection(&message, status, &bytes)
                {
                    return Ok(rejection);
                }
            }
            if status == StatusCode::NOT_FOUND && session_id.is_some() {
                return Err(StreamableHttpError::SessionExpired);
            }
            return Err(StreamableHttpError::Client(McpHttpError::Status(status)));
        }
        let answers_nothing = matches!(
            message,
            ClientJsonRpcMessage::Notification(_)
                | ClientJsonRpcMessage::Response(_)
                | ClientJsonRpcMessage::Error(_)
        );
        match content_type.as_deref() {
            Some(value) if value.starts_with(EVENT_STREAM) => Ok(StreamableHttpPostResponse::Sse(
                sse_stream(body_chunks(body), max_sse_event_size),
                new_session,
            )),
            Some(value) if value.starts_with(JSON) => {
                let bytes = collect(&mut body).await?;
                match serde_json::from_slice::<ServerJsonRpcMessage>(&bytes) {
                    Ok(parsed) => Ok(StreamableHttpPostResponse::Json(parsed, new_session)),
                    Err(_) if answers_nothing => Ok(StreamableHttpPostResponse::Accepted),
                    Err(error) => Err(StreamableHttpError::Deserialize(error)),
                }
            }
            _ if answers_nothing => {
                let _ = body.close().await;
                Ok(StreamableHttpPostResponse::Accepted)
            }
            other => Err(StreamableHttpError::UnexpectedContentType(
                other.map(ToOwned::to_owned),
            )),
        }
    }

    async fn delete_session(
        &self,
        _uri: Arc<str>,
        session_id: Arc<str>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> HttpResult<()> {
        let bearer = self.bearer().await?;
        let request = self.request(
            Method::DELETE,
            HeaderMap::new(),
            custom_headers,
            Some(&session_id),
            Bytes::new(),
            bearer.as_ref(),
        )?;
        let response = self
            .oauth_refusal(self.execute(request).await?, bearer.as_ref())
            .await?;
        let status = response.status();
        let _ = response.into_body().close().await;
        if status.is_success() || status == StatusCode::METHOD_NOT_ALLOWED {
            Ok(())
        } else {
            Err(StreamableHttpError::Client(McpHttpError::Status(status)))
        }
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> HttpResult<BoxStream<'static, Result<Sse, SseError>>> {
        self.get_stream_with_max_sse_event_size(
            uri,
            session_id,
            last_event_id,
            auth_header,
            custom_headers,
            MAX_SSE_EVENT_BYTES,
        )
        .await
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        _uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> HttpResult<BoxStream<'static, Result<Sse, SseError>>> {
        let mut headers = HeaderMap::new();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("text/event-stream, application/json"),
        );
        if let Some(last_event_id) = last_event_id {
            headers.insert(
                LAST_EVENT_ID,
                HeaderValue::from_str(&last_event_id)
                    .map_err(|_| StreamableHttpError::Client(McpHttpError::Request))?,
            );
        }
        let bearer = self.bearer().await?;
        let request = self.request(
            Method::GET,
            headers,
            custom_headers,
            session_id.as_deref(),
            Bytes::new(),
            bearer.as_ref(),
        )?;
        let response = self
            .oauth_refusal(self.execute(request).await?, bearer.as_ref())
            .await?;
        let status = response.status();
        if status == StatusCode::METHOD_NOT_ALLOWED {
            let _ = response.into_body().close().await;
            return Err(StreamableHttpError::ServerDoesNotSupportSse);
        }
        if !status.is_success() {
            let _ = response.into_body().close().await;
            return Err(StreamableHttpError::Client(McpHttpError::Status(status)));
        }
        let content_type = response
            .headers()
            .get(&CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        if !content_type
            .as_deref()
            .is_some_and(|value| value.starts_with(EVENT_STREAM) || value.starts_with(JSON))
        {
            let _ = response.into_body().close().await;
            return Err(StreamableHttpError::UnexpectedContentType(content_type));
        }
        Ok(sse_stream(
            body_chunks(response.into_body()),
            max_sse_event_size,
        ))
    }
}

// HTTP status alone cannot settle a mutating request. Only its own terminal
// JSON-RPC error can do so; a success-shaped body on an error status cannot.
fn correlated_error(
    message: &ClientJsonRpcMessage,
    bytes: &[u8],
) -> Option<rmcp::model::JsonRpcError> {
    let ClientJsonRpcMessage::Request(request) = message else {
        return None;
    };
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    if value.get("result").is_some() {
        return None;
    }
    let ServerJsonRpcMessage::Error(error) = serde_json::from_value(value).ok()? else {
        return None;
    };
    (error.id.as_ref() == Some(&request.id)).then_some(error)
}

/// A sessionless `server/discover` that a legacy server rejects with a 4xx
/// becomes the JSON-RPC error the lifecycle classifies, so discovery falls
/// back to `initialize`.
fn discover_rejection(
    message: &ClientJsonRpcMessage,
    status: StatusCode,
    body: &[u8],
) -> Option<StreamableHttpPostResponse> {
    let ClientJsonRpcMessage::Request(request) = message else {
        return None;
    };
    if !matches!(request.request, ClientRequest::DiscoverRequest(_)) {
        return None;
    }
    let error = match serde_json::from_slice::<ServerJsonRpcMessage>(body) {
        Ok(ServerJsonRpcMessage::Error(error)) if error.id.as_ref() == Some(&request.id) => {
            error.error
        }
        _ => ErrorData::invalid_request(
            format!("server/discover rejected with HTTP {}", status.as_u16()),
            None,
        ),
    };
    Some(StreamableHttpPostResponse::Json(
        ServerJsonRpcMessage::error(error, Some(request.id.clone())),
        None,
    ))
}

async fn collect(body: &mut ResponseBody) -> HttpResult<Vec<u8>> {
    let mut bytes = Vec::new();
    while let Some(chunk) = body.next_chunk().await {
        let chunk = chunk
            .map_err(|failure| StreamableHttpError::Client(McpHttpError::Transport(failure)))?;
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn body_chunks(body: ResponseBody) -> impl Stream<Item = Result<Bytes, Failure>> + Send + 'static {
    stream::unfold(body, |mut body| async move {
        let chunk = body.next_chunk().await?;
        Some((chunk, body))
    })
}

/// Frames `chunks` as server-sent events with [`plakous_http::sse`].
///
/// Event ids and retry hints are not surfaced, so rmcp never resumes a
/// request stream: a broken stream fails its call instead of re-sending
/// anything.
fn sse_stream<S>(chunks: S, max_event_bytes: usize) -> BoxStream<'static, Result<Sse, SseError>>
where
    S: Stream<Item = Result<Bytes, Failure>> + Send + 'static,
{
    let decoder = SseDecoder::new(SseLimits {
        max_line_bytes: max_event_bytes,
        max_event_bytes,
    });
    let state = (Box::pin(chunks), decoder, VecDeque::new(), false);
    stream::unfold(
        state,
        |(mut chunks, mut decoder, mut ready, mut done)| async move {
            loop {
                if let Some(message) = ready.pop_front() {
                    let message: plakous_http::SseMessage = message;
                    let event = Sse {
                        event: message.event().map(ToOwned::to_owned),
                        data: Some(message.data().to_owned()),
                        id: None,
                        retry: None,
                    };
                    return Some((Ok(event), (chunks, decoder, ready, done)));
                }
                if done {
                    return None;
                }
                let framed = match chunks.next().await {
                    Some(Ok(bytes)) => decoder.push(&bytes),
                    Some(Err(failure)) => {
                        done = true;
                        let error = SseError::Body(Box::new(McpHttpError::Transport(failure)));
                        return Some((Err(error), (chunks, decoder, ready, done)));
                    }
                    None => {
                        done = true;
                        decoder.finish()
                    }
                };
                match framed {
                    Ok(messages) => ready.extend(messages),
                    Err(error) => {
                        done = true;
                        let error = SseError::Body(Box::new(McpHttpError::Sse(error)));
                        return Some((Err(error), (chunks, decoder, ready, done)));
                    }
                }
            }
        },
    )
    .boxed()
}

/// How an HTTP failure settles a call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum HttpRefusal {
    /// The server refused the credentials before running anything.
    Credentials(StatusCode),
    /// OAuth credentials were missing or refused; nothing ran. `renewed`
    /// says a 401 renewed them, so the same request would now be admitted.
    Authorization { text: String, renewed: bool },
    /// The request never reached the server.
    NotSent,
    /// Any other HTTP failure: the call may have reached the server.
    Unknown,
}

/// Classifies `error` when it is an HTTP transport failure.
pub(crate) fn classify(error: &(dyn Error + 'static)) -> Option<HttpRefusal> {
    let found = find_http_error(error)?;
    Some(match found {
        McpHttpError::Status(status @ (StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)) => {
            HttpRefusal::Credentials(*status)
        }
        McpHttpError::Authorization { text, renewed } => HttpRefusal::Authorization {
            text: text.clone(),
            renewed: *renewed,
        },
        McpHttpError::Request | McpHttpError::Encode => HttpRefusal::NotSent,
        McpHttpError::Transport(failure)
            if failure.kind() == ErrorKind::AddressRefused
                || failure.transmission().peer_cannot_have_acted() =>
        {
            HttpRefusal::NotSent
        }
        McpHttpError::Transport(_) | McpHttpError::Status(_) | McpHttpError::Sse(_) => {
            HttpRefusal::Unknown
        }
    })
}

/// Operator text for refused credentials, naming the remedy.
pub(crate) fn credentials_text(server: &McpServerName, status: StatusCode) -> String {
    format!(
        "mcp server {server} refused its credentials (HTTP {}); check the server's bearer token and secret headers and the stored secrets they name",
        status.as_u16()
    )
}

fn find_http_error<'a>(error: &'a (dyn Error + 'static)) -> Option<&'a McpHttpError> {
    let mut current = Some(error);
    while let Some(error) = current {
        if let Some(found) = error.downcast_ref::<McpHttpError>() {
            return Some(found);
        }
        if let Some(StreamableHttpError::Client(found)) =
            error.downcast_ref::<StreamableHttpError<McpHttpError>>()
        {
            return Some(found);
        }
        let transport = match error.downcast_ref::<ServiceError>() {
            Some(ServiceError::TransportSend(transport)) => Some(transport),
            _ => match error.downcast_ref::<ClientInitializeError>() {
                Some(ClientInitializeError::TransportError { error, .. }) => Some(error),
                _ => None,
            },
        };
        if let Some(found) =
            transport.and_then(|transport| find_http_error(transport.error.as_ref()))
        {
            return Some(found);
        }
        current = error.source();
    }
    None
}

#[cfg(test)]
mod tests;
