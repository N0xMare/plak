//! The exact upstream request shapes an OpenAI credential may be relayed to.

use plakous_auth::custody::{PinPath, PinQuery, UpstreamPin};

use crate::config::{
    OPENAI_CHATGPT_ORIGIN, OPENAI_CODEX_MODELS_PATH, OPENAI_CODEX_RESPONSES_PATH,
    OPENAI_PLATFORM_ORIGIN, OPENAI_PLATFORM_RESPONSES_PATH,
};

/// The Platform key's credential header.
pub const OPENAI_API_CREDENTIAL_HEADERS: &[&str] = &["authorization"];
/// The Codex subscription's credential headers, in injection order.
pub const CODEX_CREDENTIAL_HEADERS: &[&str] = &["authorization", "chatgpt-account-id"];

const fn api(method: &'static str, path: &'static str, query: PinQuery) -> UpstreamPin {
    UpstreamPin {
        origin: OPENAI_PLATFORM_ORIGIN,
        method,
        path: PinPath::Exact(path),
        query,
        credential_headers: OPENAI_API_CREDENTIAL_HEADERS,
        pool_inference: false,
    }
}

const fn codex(method: &'static str, path: &'static str, query: PinQuery) -> UpstreamPin {
    UpstreamPin {
        origin: OPENAI_CHATGPT_ORIGIN,
        method,
        path: PinPath::Exact(path),
        query,
        credential_headers: CODEX_CREDENTIAL_HEADERS,
        pool_inference: false,
    }
}

/// Platform API-key rows: Responses, the model list and images.
pub const OPENAI_API_UPSTREAM_PINS: &[UpstreamPin] = &[
    api("POST", OPENAI_PLATFORM_RESPONSES_PATH, PinQuery::None).pooled(),
    api("GET", crate::api_models::OPENAI_MODELS_PATH, PinQuery::None),
    api(
        "POST",
        crate::images::OPENAI_IMAGE_GENERATIONS_PATH,
        PinQuery::None,
    ),
    api(
        "POST",
        crate::images::OPENAI_IMAGE_EDITS_PATH,
        PinQuery::None,
    ),
];

/// Codex subscription rows: Responses, the versioned catalog, usage, search
/// and images. Login, token, revoke, JWKS and userinfo are never relayed.
pub const CODEX_UPSTREAM_PINS: &[UpstreamPin] = &[
    codex("POST", OPENAI_CODEX_RESPONSES_PATH, PinQuery::None).pooled(),
    codex(
        "GET",
        OPENAI_CODEX_MODELS_PATH,
        PinQuery::Keys(&["client_version"]),
    ),
    codex(
        "GET",
        crate::subscription_usage::CODEX_SUBSCRIPTION_USAGE_PATH,
        PinQuery::None,
    ),
    codex(
        "POST",
        crate::codex_search::CODEX_SEARCH_PATH,
        PinQuery::None,
    ),
    codex(
        "POST",
        crate::images::CODEX_IMAGE_GENERATIONS_PATH,
        PinQuery::None,
    ),
    codex(
        "POST",
        crate::images::CODEX_IMAGE_EDITS_PATH,
        PinQuery::None,
    ),
];

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use plakous_auth::custody::pinned;

    use super::*;

    #[test]
    fn the_send_sites_match_their_rows_and_nothing_else_does() {
        let platform = OPENAI_PLATFORM_ORIGIN.trim_end_matches('/');
        let chatgpt = OPENAI_CHATGPT_ORIGIN.trim_end_matches('/');
        assert!(pinned(OPENAI_API_UPSTREAM_PINS, "POST", platform, "/v1/responses").is_some());
        let catalog = format!(
            "{OPENAI_CODEX_MODELS_PATH}?client_version={}",
            crate::OPENAI_CODEX_REFERENCE_CLIENT_VERSION
        );
        let row = pinned(CODEX_UPSTREAM_PINS, "GET", chatgpt, &catalog).unwrap();
        assert_eq!(row.credential_headers, CODEX_CREDENTIAL_HEADERS);
        assert!(
            pinned(
                CODEX_UPSTREAM_PINS,
                "POST",
                chatgpt,
                "/backend-api/codex/responses"
            )
            .is_some()
        );
        for (origin, path) in [
            ("https://auth.openai.com", "/oauth/token"),
            ("https://auth.openai.com", "/.well-known/jwks.json"),
            ("https://auth.openai.com", "/api/accounts/oauth/userinfo"),
            (
                "https://auth.openai.com",
                "/api/accounts/deviceauth/usercode",
            ),
            (platform, "/backend-api/codex/responses"),
            (chatgpt, "/v1/responses"),
        ] {
            assert!(
                pinned(OPENAI_API_UPSTREAM_PINS, "POST", chatgpt, path).is_none(),
                "{path}"
            );
            assert!(
                pinned(CODEX_UPSTREAM_PINS, "POST", origin, path).is_none(),
                "{path}"
            );
        }
    }
}
