//! Exact upstream pins: the request shapes one credential may be relayed to.
//!
//! A gateway relays a custodied credential only to a row of its provider's
//! table, compared exactly on the logical URL: origin, method, path and the
//! query rule. Token, device, JWKS, userinfo, discovery and profile
//! endpoints are never rows: their exchanges stay inside the custodian.

/// One path segment of a templated path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathSegment {
    /// Exactly this segment.
    Literal(&'static str),
    /// One nonempty segment of unreserved or percent-encoded characters.
    Any,
}

/// How a pin matches the path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PinPath {
    /// Exactly this path.
    Exact(&'static str),
    /// These segments after the leading `/`, one for one.
    Segments(&'static [PathSegment]),
}

/// How a pin matches the query.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PinQuery {
    /// No query.
    None,
    /// Exactly this query.
    Exact(&'static str),
    /// Optionally a query of these keys, each at most once, values of
    /// unreserved or percent-encoded characters.
    Keys(&'static [&'static str]),
}

/// One relayable upstream request shape and the credential header names it
/// carries, in injection order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UpstreamPin {
    /// `https://host[:port]`, lowercase, no trailing slash.
    pub origin: &'static str,
    /// The method.
    pub method: &'static str,
    /// The path rule.
    pub path: PinPath,
    /// The query rule.
    pub query: PinQuery,
    /// The custodied header names this request carries.
    pub credential_headers: &'static [&'static str],
}

impl UpstreamPin {
    /// Whether a request with this method, origin and path-and-query matches.
    #[must_use]
    pub fn matches(&self, method: &str, origin: &str, path_and_query: &str) -> bool {
        let (path, query) = match path_and_query.split_once('?') {
            Some((path, query)) => (path, Some(query)),
            None => (path_and_query, None),
        };
        method == self.method
            && origin == self.origin.trim_end_matches('/')
            && path_matches(self.path, path)
            && query_matches(self.query, query)
    }
}

/// The row of `pins` a request matches, when one does.
#[must_use]
pub fn pinned<'a>(
    pins: &'a [UpstreamPin],
    method: &str,
    origin: &str,
    path_and_query: &str,
) -> Option<&'a UpstreamPin> {
    pins.iter()
        .find(|pin| pin.matches(method, origin, path_and_query))
}

fn path_matches(rule: PinPath, path: &str) -> bool {
    match rule {
        PinPath::Exact(exact) => path == exact,
        PinPath::Segments(segments) => {
            let Some(rest) = path.strip_prefix('/') else {
                return false;
            };
            let parts: Vec<&str> = rest.split('/').collect();
            parts.len() == segments.len()
                && parts
                    .iter()
                    .zip(segments)
                    .all(|(part, segment)| match segment {
                        PathSegment::Literal(literal) => part == literal,
                        PathSegment::Any => {
                            !matches!(*part, "" | "." | "..") && part.bytes().all(unreserved)
                        }
                    })
        }
    }
}

fn query_matches(rule: PinQuery, query: Option<&str>) -> bool {
    match (rule, query) {
        (PinQuery::None | PinQuery::Keys(_), None) => true,
        (PinQuery::Exact(exact), Some(query)) => query == exact,
        (PinQuery::Keys(keys), Some(query)) => {
            let mut seen: Vec<&str> = Vec::new();
            query.split('&').all(|pair| {
                let Some((key, value)) = pair.split_once('=') else {
                    return false;
                };
                let fresh = keys.contains(&key) && !seen.contains(&key);
                seen.push(key);
                fresh && value.bytes().all(unreserved)
            })
        }
        _ => false,
    }
}

const fn unreserved(byte: u8) -> bool {
    matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'%')
}

#[cfg(test)]
mod tests {
    use super::*;

    const PINS: &[UpstreamPin] = &[
        UpstreamPin {
            origin: "https://api.example.test",
            method: "POST",
            path: PinPath::Exact("/v1/messages"),
            query: PinQuery::Exact("beta=true"),
            credential_headers: &["authorization"],
        },
        UpstreamPin {
            origin: "https://api.example.test/",
            method: "GET",
            path: PinPath::Segments(&[
                PathSegment::Literal("v1"),
                PathSegment::Literal("models"),
                PathSegment::Any,
                PathSegment::Literal("endpoints"),
            ]),
            query: PinQuery::Keys(&["limit", "after_id"]),
            credential_headers: &["authorization"],
        },
    ];

    #[test]
    fn rows_match_exactly_on_origin_method_path_and_query() {
        let origin = "https://api.example.test";
        assert!(pinned(PINS, "POST", origin, "/v1/messages?beta=true").is_some());
        for refused in [
            ("POST", origin, "/v1/messages"),
            ("POST", origin, "/v1/messages?beta=false"),
            ("GET", origin, "/v1/messages?beta=true"),
            (
                "POST",
                "https://api.example.test:8443",
                "/v1/messages?beta=true",
            ),
            (
                "POST",
                "https://other.example.test",
                "/v1/messages?beta=true",
            ),
            ("POST", origin, "/v1/messages/?beta=true"),
        ] {
            assert!(
                pinned(PINS, refused.0, refused.1, refused.2).is_none(),
                "{refused:?}"
            );
        }
        assert!(pinned(PINS, "GET", origin, "/v1/models/a-b/endpoints").is_some());
        assert!(
            pinned(
                PINS,
                "GET",
                origin,
                "/v1/models/a/endpoints?limit=5&after_id=x"
            )
            .is_some()
        );
        for refused in [
            "/v1/models//endpoints",
            "/v1/models/a/b/endpoints",
            "/v1/models/../endpoints?limit=1",
            "/v1/models/a/endpoints?limit=1&limit=2",
            "/v1/models/a/endpoints?other=1",
            "/v1/models/a/endpoints?limit",
            "/v1/models/a%2Fb/endpoints?limit=a/b",
        ] {
            assert!(pinned(PINS, "GET", origin, refused).is_none(), "{refused}");
        }
    }
}
