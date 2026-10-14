//! Pure HTTP/1.1 GET request-head parsing for OAuth loopback callbacks.
//!
//! Managers retain TCP accept/read/write ownership. This helper only extracts
//! the origin-form target and single Host header from a complete request head.

use super::OAuthWorkflowError;

/// Origin-form target and Host extracted from an HTTP/1.1 GET request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Http11GetRequest<'a> {
    /// Request-target in origin-form (`/path` or `/path?query`).
    pub target: &'a str,
    /// Single `Host` header value (space/tab trimmed).
    pub host: &'a str,
}

/// Parses a complete HTTP/1.1 GET request for OAuth loopback use.
///
/// Accepts only:
/// - a request that ends at the header terminator (`\r\n\r\n`) with no body
/// - request-line `GET <origin-form-target> HTTP/1.1`
/// - a single `Host` header
/// - no folded header lines
///
/// Other headers are ignored. Query grammar and redirect URI binding remain in
/// [`super::parse_oauth_callback_query`] / [`super::parse_absolute_redirect_query`].
///
/// # Errors
///
/// Returns [`OAuthWorkflowError::Callback`] for any framing or grammar failure.
pub fn parse_http11_get_request(request: &str) -> Result<Http11GetRequest<'_>, OAuthWorkflowError> {
    let head = request
        .strip_suffix("\r\n\r\n")
        .filter(|head| !head.is_empty() && !head.contains("\r\n\r\n"))
        .ok_or(OAuthWorkflowError::Callback)?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next().ok_or(OAuthWorkflowError::Callback)?;
    let mut parts = request_line.split(' ');
    if parts.next() != Some("GET") {
        return Err(OAuthWorkflowError::Callback);
    }
    let target = parts.next().ok_or(OAuthWorkflowError::Callback)?;
    if parts.next() != Some("HTTP/1.1")
        || parts.next().is_some()
        || target.is_empty()
        || !target.starts_with('/')
        || target.starts_with("//")
        || target.contains('#')
    {
        return Err(OAuthWorkflowError::Callback);
    }

    let mut host = None;
    for line in lines {
        if line.is_empty() || line.starts_with([' ', '\t']) {
            return Err(OAuthWorkflowError::Callback);
        }
        let (name, value) = line.split_once(':').ok_or(OAuthWorkflowError::Callback)?;
        if name.is_empty() || !name.bytes().all(is_http_token_byte) {
            return Err(OAuthWorkflowError::Callback);
        }
        let value = value.trim_matches(|character| character == ' ' || character == '\t');
        if !value
            .bytes()
            .all(|byte| byte == b'\t' || (0x20..=0x7e).contains(&byte))
        {
            return Err(OAuthWorkflowError::Callback);
        }
        if name.eq_ignore_ascii_case("host") && host.replace(value).is_some() {
            return Err(OAuthWorkflowError::Callback);
        }
    }
    let host = host
        .filter(|value| !value.is_empty())
        .ok_or(OAuthWorkflowError::Callback)?;
    Ok(Http11GetRequest { target, host })
}

/// Returns whether `byte` is a valid HTTP token character (RFC 9110).
#[must_use]
pub const fn is_http_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use super::{Http11GetRequest, parse_http11_get_request};
    use crate::oauth_workflow::OAuthWorkflowError;

    #[test]
    fn accepts_origin_form_get_with_single_host() {
        let request = "GET /auth/callback?code=x&state=y HTTP/1.1\r\nHost: localhost:1455\r\nConnection: close\r\n\r\n";
        assert_eq!(
            parse_http11_get_request(request),
            Ok(Http11GetRequest {
                target: "/auth/callback?code=x&state=y",
                host: "localhost:1455",
            })
        );
    }

    #[test]
    fn rejects_body_absolute_form_duplicate_host_and_folding() {
        assert_eq!(
            parse_http11_get_request(
                "GET /callback?code=c&state=s HTTP/1.1\r\nHost: 127.0.0.1:56121\r\n\r\nbody"
            ),
            Err(OAuthWorkflowError::Callback)
        );
        assert_eq!(
            parse_http11_get_request(
                "GET http://127.0.0.1:56121/callback?code=c&state=s HTTP/1.1\r\nHost: 127.0.0.1:56121\r\n\r\n"
            ),
            Err(OAuthWorkflowError::Callback)
        );
        assert_eq!(
            parse_http11_get_request(
                "GET /callback?code=c&state=s HTTP/1.1\r\nHost: 127.0.0.1:56121\r\nHost: 127.0.0.1:56121\r\n\r\n"
            ),
            Err(OAuthWorkflowError::Callback)
        );
        assert_eq!(
            parse_http11_get_request(
                "GET /callback?code=c&state=s HTTP/1.1\r\nHost: 127.0.0.1:56121\r\n folded: value\r\n\r\n"
            ),
            Err(OAuthWorkflowError::Callback)
        );
        assert_eq!(
            parse_http11_get_request(
                "GET /callback?code=c&state=s HTTP/1.0\r\nHost: 127.0.0.1:56121\r\n\r\n"
            ),
            Err(OAuthWorkflowError::Callback)
        );
    }
}
