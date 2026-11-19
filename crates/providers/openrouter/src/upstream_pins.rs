//! The exact upstream request shapes an OpenRouter key may be relayed to.

use plakous_auth::custody::{PathSegment, PinPath, PinQuery, UpstreamPin};

use crate::route::OPENROUTER_PLATFORM_ORIGIN;

/// The key's credential header.
pub const OPENROUTER_CREDENTIAL_HEADERS: &[&str] = &["authorization"];

const fn row(method: &'static str, path: PinPath) -> UpstreamPin {
    UpstreamPin {
        origin: OPENROUTER_PLATFORM_ORIGIN,
        method,
        path,
        query: PinQuery::None,
        credential_headers: OPENROUTER_CREDENTIAL_HEADERS,
        pool_inference: false,
    }
}

/// Chat, Messages, Responses, the catalog, each model's endpoint list
/// (`/api/v1/models/{author}/{slug}/endpoints`) and the key read.
pub const OPENROUTER_UPSTREAM_PINS: &[UpstreamPin] = &[
    row(
        "POST",
        PinPath::Exact(crate::chat::OPENROUTER_CHAT_COMPLETIONS_PATH),
    )
    .pooled(),
    row(
        "POST",
        PinPath::Exact(crate::messages::OPENROUTER_MESSAGES_PATH),
    )
    .pooled(),
    row(
        "POST",
        PinPath::Exact(crate::dialect::OPENROUTER_RESPONSES_PATH),
    )
    .pooled(),
    row("GET", PinPath::Exact(crate::discovery::MODELS_PATH)),
    row(
        "GET",
        PinPath::Segments(&[
            PathSegment::Literal("api"),
            PathSegment::Literal("v1"),
            PathSegment::Literal("models"),
            PathSegment::Any,
            PathSegment::Any,
            PathSegment::Literal("endpoints"),
        ]),
    ),
    row(
        "GET",
        PinPath::Exact(crate::key_usage::OPENROUTER_KEY_USAGE_PATH),
    ),
];

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use plakous_auth::custody::pinned;

    use super::*;

    #[test]
    fn the_send_sites_match_their_rows_and_nothing_else_does() {
        let origin = OPENROUTER_PLATFORM_ORIGIN;
        for (method, path) in [
            ("POST", "/api/v1/chat/completions"),
            ("POST", "/api/v1/messages"),
            ("POST", "/api/v1/responses"),
            ("GET", "/api/v1/models"),
            (
                "GET",
                "/api/v1/models/anthropic/claude-sonnet-5.1/endpoints",
            ),
            ("GET", "/api/v1/models/vendor/model-v1%3Afree/endpoints"),
            ("GET", "/api/v1/key"),
        ] {
            assert!(
                pinned(OPENROUTER_UPSTREAM_PINS, method, origin, path).is_some(),
                "{path}"
            );
        }
        for (method, path) in [
            ("GET", "/api/v1/credits"),
            ("GET", "/api/v1/models/a/endpoints"),
            ("POST", "/api/v1/models"),
            ("GET", "/api/v1/models?x=1"),
            ("GET", "/api/v1/models/a/%2E%2E/endpoints"),
            ("GET", "/api/v1/models/a%2Fb/c/endpoints"),
            ("GET", "/api/v1/models/../c/endpoints"),
        ] {
            assert!(
                pinned(OPENROUTER_UPSTREAM_PINS, method, origin, path).is_none(),
                "{path}"
            );
        }
    }
}
