//! The exact upstream request shapes an Anthropic credential may be relayed to.

use plakous_auth::custody::{PinPath, PinQuery, UpstreamPin};

use crate::oauth::CLAUDE_OAUTH_MESSAGES_ORIGIN;
use crate::route::{ANTHROPIC_MESSAGES_PATH, ANTHROPIC_PLATFORM_ORIGIN};

/// The Platform key's credential header.
pub const ANTHROPIC_API_CREDENTIAL_HEADERS: &[&str] = &["x-api-key"];
/// The Claude subscription's credential header.
pub const CLAUDE_CREDENTIAL_HEADERS: &[&str] = &["authorization"];

/// Platform API-key rows: Messages and the paginated model list.
pub const ANTHROPIC_API_UPSTREAM_PINS: &[UpstreamPin] = &[
    UpstreamPin {
        origin: ANTHROPIC_PLATFORM_ORIGIN,
        method: "POST",
        path: PinPath::Exact(ANTHROPIC_MESSAGES_PATH),
        query: PinQuery::None,
        credential_headers: ANTHROPIC_API_CREDENTIAL_HEADERS,
        pool_inference: true,
    },
    UpstreamPin {
        origin: ANTHROPIC_PLATFORM_ORIGIN,
        method: "GET",
        path: PinPath::Exact(crate::api_models::ANTHROPIC_MODELS_PATH),
        query: PinQuery::Keys(&["limit", "after_id"]),
        credential_headers: ANTHROPIC_API_CREDENTIAL_HEADERS,
        pool_inference: false,
    },
];

/// Claude subscription rows: OAuth Messages (`?beta=true`), usage and
/// account profile reads. The token endpoint is never relayed.
pub const CLAUDE_UPSTREAM_PINS: &[UpstreamPin] = &[
    UpstreamPin {
        origin: CLAUDE_OAUTH_MESSAGES_ORIGIN,
        method: "POST",
        path: PinPath::Exact(ANTHROPIC_MESSAGES_PATH),
        query: PinQuery::Exact("beta=true"),
        credential_headers: CLAUDE_CREDENTIAL_HEADERS,
        pool_inference: true,
    },
    UpstreamPin {
        origin: CLAUDE_OAUTH_MESSAGES_ORIGIN,
        method: "GET",
        path: PinPath::Exact(crate::subscription_usage::CLAUDE_SUBSCRIPTION_USAGE_PATH),
        query: PinQuery::None,
        credential_headers: CLAUDE_CREDENTIAL_HEADERS,
        pool_inference: false,
    },
    UpstreamPin {
        origin: CLAUDE_OAUTH_MESSAGES_ORIGIN,
        method: "GET",
        path: PinPath::Exact(crate::subscription_usage::CLAUDE_SUBSCRIPTION_PROFILE_PATH),
        query: PinQuery::None,
        credential_headers: CLAUDE_CREDENTIAL_HEADERS,
        pool_inference: false,
    },
];

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use plakous_auth::custody::pinned;

    use super::*;

    #[test]
    fn only_messages_pins_are_pooled() {
        for (pins, path) in [
            (ANTHROPIC_API_UPSTREAM_PINS, "/v1/messages"),
            (CLAUDE_UPSTREAM_PINS, "/v1/messages?beta=true"),
        ] {
            assert_eq!(
                pins.iter()
                    .filter(|pin| pin.pool_inference)
                    .collect::<Vec<_>>(),
                [pinned(pins, "POST", ANTHROPIC_PLATFORM_ORIGIN, path).unwrap()],
            );
        }
    }

    #[test]
    fn the_send_sites_match_their_rows_and_nothing_else_does() {
        assert_eq!(
            format!("{ANTHROPIC_MESSAGES_PATH}?beta=true"),
            crate::oauth::CLAUDE_OAUTH_MESSAGES_PATH
        );
        let origin = ANTHROPIC_PLATFORM_ORIGIN;
        assert!(
            pinned(
                CLAUDE_UPSTREAM_PINS,
                "POST",
                origin,
                crate::oauth::CLAUDE_OAUTH_MESSAGES_PATH
            )
            .is_some()
        );
        assert!(
            pinned(
                CLAUDE_UPSTREAM_PINS,
                "POST",
                origin,
                ANTHROPIC_MESSAGES_PATH
            )
            .is_none()
        );
        assert!(
            pinned(
                ANTHROPIC_API_UPSTREAM_PINS,
                "POST",
                origin,
                ANTHROPIC_MESSAGES_PATH
            )
            .is_some()
        );
        assert!(
            pinned(
                ANTHROPIC_API_UPSTREAM_PINS,
                "GET",
                origin,
                "/v1/models?limit=1000&after_id=x"
            )
            .is_some()
        );
        let profile = "/api/oauth/profile";
        assert!(pinned(CLAUDE_UPSTREAM_PINS, "GET", origin, profile).is_some());
        for (pins, method, origin, path) in [
            (CLAUDE_UPSTREAM_PINS, "POST", origin, profile),
            (
                CLAUDE_UPSTREAM_PINS,
                "GET",
                origin,
                "/api/oauth/profile?x=1",
            ),
            (
                CLAUDE_UPSTREAM_PINS,
                "GET",
                "https://platform.claude.com",
                profile,
            ),
            (ANTHROPIC_API_UPSTREAM_PINS, "GET", origin, profile),
            (
                CLAUDE_UPSTREAM_PINS,
                "POST",
                "https://platform.claude.com",
                "/v1/oauth/token",
            ),
        ] {
            assert!(
                pinned(pins, method, origin, path).is_none(),
                "{method} {origin}{path}"
            );
        }
    }
}
