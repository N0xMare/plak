use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use bytes::Bytes;
use http::header::{
    CONNECTION, CONTENT_LENGTH, HOST, PROXY_AUTHENTICATE, PROXY_AUTHORIZATION, TE, TRAILER,
    TRANSFER_ENCODING, UPGRADE,
};
use http::uri::{Authority, PathAndQuery, Scheme};
use http::{HeaderMap, HeaderName, HeaderValue, Method, Uri};

use crate::{LimitKind, Limits, RequestError};

/// An absolute origin used for both connection and request routing: HTTPS,
/// or plain HTTP to exactly `127.0.0.1` or `[::1]` ([`Origin::loopback_http`]).
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct Origin {
    authority: Authority,
    host: String,
    port: u16,
    plain_loopback: bool,
}

impl Origin {
    /// Parses an HTTPS origin with no credentials, path, query, or fragment.
    ///
    /// # Errors
    ///
    /// Returns [`RequestError::InvalidOrigin`] when `value` is not exactly an
    /// absolute HTTPS origin.
    pub fn parse(value: &str) -> Result<Self, RequestError> {
        let uri = value
            .parse::<Uri>()
            .map_err(|_| RequestError::InvalidOrigin)?;
        if uri.scheme() != Some(&Scheme::HTTPS)
            || uri.query().is_some()
            || uri.path() != "/"
            || uri.authority().is_none()
        {
            return Err(RequestError::InvalidOrigin);
        }
        let authority = uri.authority().ok_or(RequestError::InvalidOrigin)?;
        if authority.as_str().contains('@') {
            return Err(RequestError::InvalidOrigin);
        }
        let host = uri.host().ok_or(RequestError::InvalidOrigin)?.to_owned();
        if host.is_empty() {
            return Err(RequestError::InvalidOrigin);
        }
        let port = uri.port_u16().unwrap_or(443);
        Ok(Self {
            authority: authority.clone(),
            host,
            port,
            plain_loopback: false,
        })
    }

    /// A plain-HTTP origin on the loopback interface: `http://127.0.0.1:port`
    /// or `http://[::1]:port`. No other address and no host name is accepted,
    /// so plain HTTP never leaves the machine. Connections carry HTTP/1.1.
    ///
    /// # Errors
    ///
    /// Returns [`RequestError::InvalidOrigin`] for any other address or port 0.
    pub fn loopback_http(address: IpAddr, port: u16) -> Result<Self, RequestError> {
        let host = match address {
            IpAddr::V4(Ipv4Addr::LOCALHOST) => "127.0.0.1",
            IpAddr::V6(Ipv6Addr::LOCALHOST) => "[::1]",
            IpAddr::V4(_) | IpAddr::V6(_) => return Err(RequestError::InvalidOrigin),
        };
        if port == 0 {
            return Err(RequestError::InvalidOrigin);
        }
        let authority = format!("{host}:{port}")
            .parse::<Authority>()
            .map_err(|_| RequestError::InvalidOrigin)?;
        Ok(Self {
            authority,
            host: host.to_owned(),
            port,
            plain_loopback: true,
        })
    }

    /// Whether this is a plain-HTTP [`Origin::loopback_http`] origin.
    #[must_use]
    pub const fn is_loopback_http(&self) -> bool {
        self.plain_loopback
    }

    /// Returns the normalized non-secret connection authority for exact
    /// request binding.
    ///
    /// This is host and optional explicit port only; credentials are rejected
    /// by [`Origin::parse`].
    #[must_use]
    pub fn binding_authority(&self) -> &str {
        self.authority.as_str()
    }

    pub(crate) fn authority(&self) -> &Authority {
        &self.authority
    }

    /// Returns the origin hostname without scheme, port or credentials.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The host as name resolution takes it: an IPv6 literal without brackets.
    pub(crate) fn resolution_host(&self) -> &str {
        self.host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(&self.host)
    }

    pub(crate) const fn port(&self) -> u16 {
        self.port
    }
}

impl fmt::Debug for Origin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("Origin")
            .field(&"<redacted>")
            .finish()
    }
}

/// Header storage whose `Debug` implementation never prints values.
#[derive(Clone)]
pub struct Headers(HeaderMap<HeaderValue>);

impl Headers {
    pub(crate) fn new(mut headers: HeaderMap<HeaderValue>) -> Self {
        for value in headers.values_mut() {
            value.set_sensitive(true);
        }
        Self(headers)
    }

    /// Returns the first value for a header name.
    #[must_use]
    pub fn get(&self, name: &HeaderName) -> Option<&HeaderValue> {
        self.0.get(name)
    }

    /// Returns the number of header values.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns whether there are no headers.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Iterates over header names and redaction-marked values.
    pub fn iter(&self) -> impl Iterator<Item = (&HeaderName, &HeaderValue)> {
        self.0.iter()
    }

    pub(crate) fn into_inner(self) -> HeaderMap<HeaderValue> {
        self.0
    }
}

impl fmt::Debug for Headers {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Headers")
            .field("count", &self.0.len())
            .finish()
    }
}

/// An owned, immutable, origin-bound HTTP request.
#[derive(Clone)]
pub struct Request {
    origin: Origin,
    method: Method,
    path_and_query: PathAndQuery,
    headers: Headers,
    body: Bytes,
}

impl Request {
    /// Validates and constructs an immutable request against strict limits.
    ///
    /// # Errors
    ///
    /// Returns [`RequestError`] when routing or framing is caller-controlled,
    /// the target is invalid, or a configured bound is exceeded.
    pub fn new(
        origin: Origin,
        method: Method,
        path_and_query: &str,
        headers: HeaderMap<HeaderValue>,
        body: Bytes,
        limits: &Limits,
    ) -> Result<Self, RequestError> {
        if method == Method::CONNECT {
            return Err(RequestError::ConnectMethod);
        }
        if !path_and_query.starts_with('/') || path_and_query.starts_with("//") {
            return Err(RequestError::InvalidPathAndQuery);
        }
        let path_and_query = path_and_query
            .parse::<PathAndQuery>()
            .map_err(|_| RequestError::InvalidPathAndQuery)?;
        validate_headers(&headers, &origin, body.len(), limits)?;
        if body.len() > limits.max_request_body_bytes {
            return Err(RequestError::LimitExceeded(LimitKind::RequestBodyBytes));
        }
        Ok(Self {
            origin,
            method,
            path_and_query,
            headers: Headers::new(headers),
            body,
        })
    }

    /// Returns the bound origin.
    #[must_use]
    pub const fn origin(&self) -> &Origin {
        &self.origin
    }

    /// Returns the HTTP method.
    #[must_use]
    pub const fn method(&self) -> &Method {
        &self.method
    }

    /// Returns the origin-form path and query.
    #[must_use]
    pub const fn path_and_query(&self) -> &PathAndQuery {
        &self.path_and_query
    }

    /// Returns redaction-marked request headers.
    #[must_use]
    pub const fn headers(&self) -> &Headers {
        &self.headers
    }

    /// Returns the request body bytes.
    #[must_use]
    pub const fn body(&self) -> &Bytes {
        &self.body
    }

    pub(crate) fn validate(&self, limits: &Limits) -> Result<(), RequestError> {
        validate_headers(&self.headers.0, &self.origin, self.body.len(), limits)?;
        if self.body.len() > limits.max_request_body_bytes {
            return Err(RequestError::LimitExceeded(LimitKind::RequestBodyBytes));
        }
        Ok(())
    }

    pub(crate) fn into_parts(self) -> (Origin, Method, PathAndQuery, Headers, Bytes) {
        (
            self.origin,
            self.method,
            self.path_and_query,
            self.headers,
            self.body,
        )
    }
}

impl fmt::Debug for Request {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Request")
            .field("header_count", &self.headers.len())
            .field("body_bytes", &self.body.len())
            .finish_non_exhaustive()
    }
}

pub(crate) fn header_bytes(headers: &HeaderMap<HeaderValue>) -> Option<usize> {
    headers.iter().try_fold(0_usize, |total, (name, value)| {
        total
            .checked_add(name.as_str().len())?
            .checked_add(value.as_bytes().len())?
            .checked_add(4)
    })
}

fn validate_headers(
    headers: &HeaderMap<HeaderValue>,
    origin: &Origin,
    body_bytes: usize,
    limits: &Limits,
) -> Result<(), RequestError> {
    // Reserve for the transport-owned Host and worst-case Content-Length.
    if request_header_count_exceeds(headers.len(), limits.max_request_headers) {
        return Err(RequestError::LimitExceeded(LimitKind::RequestHeaderCount));
    }
    let framing_bytes = HOST
        .as_str()
        .len()
        .checked_add(origin.authority.as_str().len())
        .and_then(|bytes| bytes.checked_add(CONTENT_LENGTH.as_str().len()))
        .and_then(|bytes| bytes.checked_add(body_bytes.to_string().len()))
        .and_then(|bytes| bytes.checked_add(8));
    let total_bytes = header_bytes(headers)
        .zip(framing_bytes)
        .and_then(|(headers, framing)| headers.checked_add(framing));
    if total_bytes.is_none_or(|bytes| bytes > limits.max_request_header_bytes) {
        return Err(RequestError::LimitExceeded(LimitKind::RequestHeaderBytes));
    }
    for name in headers.keys() {
        if controlled_header(name) {
            return Err(RequestError::ControlledHeader(name.clone()));
        }
    }
    Ok(())
}

fn request_header_count_exceeds(caller_count: usize, limit: usize) -> bool {
    caller_count
        .checked_add(2)
        .is_none_or(|total| total > limit)
}

fn controlled_header(name: &HeaderName) -> bool {
    name == HOST
        || name == CONTENT_LENGTH
        || name == CONNECTION
        || name == TRANSFER_ENCODING
        || name == PROXY_AUTHENTICATE
        || name == PROXY_AUTHORIZATION
        || name == UPGRADE
        || name == TE
        || name == TRAILER
        || name.as_str() == "proxy-connection"
        || name.as_str() == "keep-alive"
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use super::request_header_count_exceeds;

    #[test]
    fn request_header_count_overflow_fails_closed() {
        assert!(request_header_count_exceeds(usize::MAX, usize::MAX));
        assert!(request_header_count_exceeds(usize::MAX - 1, usize::MAX));
        assert!(!request_header_count_exceeds(usize::MAX - 2, usize::MAX));
    }
}
