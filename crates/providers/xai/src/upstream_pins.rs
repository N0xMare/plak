//! The exact upstream request shapes an xAI credential may be relayed to.

use plakous_auth::custody::{PinPath, PinQuery, UpstreamPin};

use crate::platform::{XAI_PLATFORM_API_KEY_ORIGIN, XAI_PLATFORM_RESPONSES_PATH};
use crate::responses_dialect::{
    SUPERGROK_MODELS_PATH, SUPERGROK_RESPONSES_ORIGIN, SUPERGROK_RESPONSES_PATH,
};

/// The Platform key's credential header.
pub const XAI_API_CREDENTIAL_HEADERS: &[&str] = &["authorization"];
/// Every SuperGrok credential header, in injection order: the bearer and the
/// account subject the catalog and billing reads carry.
pub const SUPERGROK_CREDENTIAL_HEADERS: &[&str] = &["authorization", "x-userid"];

const fn api(method: &'static str, path: &'static str) -> UpstreamPin {
    UpstreamPin {
        origin: XAI_PLATFORM_API_KEY_ORIGIN,
        method,
        path: PinPath::Exact(path),
        query: PinQuery::None,
        credential_headers: XAI_API_CREDENTIAL_HEADERS,
        pool_inference: false,
    }
}

/// Platform API-key rows: Responses, the model list and images.
pub const XAI_API_UPSTREAM_PINS: &[UpstreamPin] = &[
    api("POST", XAI_PLATFORM_RESPONSES_PATH).pooled(),
    api("GET", crate::api_models::XAI_MODELS_PATH),
    api("POST", crate::images::XAI_IMAGE_GENERATIONS_PATH),
    api("POST", crate::images::XAI_IMAGE_EDITS_PATH),
];

/// SuperGrok rows: Responses with the bearer alone; the catalog and the
/// credits billing read with the subject as well. OAuth, device, JWKS and
/// userinfo are never relayed.
pub const SUPERGROK_UPSTREAM_PINS: &[UpstreamPin] = &[
    UpstreamPin {
        origin: SUPERGROK_RESPONSES_ORIGIN,
        method: "POST",
        path: PinPath::Exact(SUPERGROK_RESPONSES_PATH),
        query: PinQuery::None,
        credential_headers: &["authorization"],
        pool_inference: true,
    },
    UpstreamPin {
        origin: SUPERGROK_RESPONSES_ORIGIN,
        method: "GET",
        path: PinPath::Exact(SUPERGROK_MODELS_PATH),
        query: PinQuery::None,
        credential_headers: SUPERGROK_CREDENTIAL_HEADERS,
        pool_inference: false,
    },
    UpstreamPin {
        origin: SUPERGROK_RESPONSES_ORIGIN,
        method: "GET",
        path: PinPath::Exact("/v1/billing"),
        query: PinQuery::Exact("format=credits"),
        credential_headers: SUPERGROK_CREDENTIAL_HEADERS,
        pool_inference: false,
    },
];

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use plakous_auth::custody::pinned;

    use super::*;

    #[test]
    fn the_send_sites_match_their_rows_and_nothing_else_does() {
        let billing = "/v1/billing?format=credits";
        assert!(
            pinned(
                SUPERGROK_UPSTREAM_PINS,
                "GET",
                SUPERGROK_RESPONSES_ORIGIN,
                billing
            )
            .is_some()
        );
        assert!(
            pinned(
                SUPERGROK_UPSTREAM_PINS,
                "GET",
                SUPERGROK_RESPONSES_ORIGIN,
                "/v1/billing"
            )
            .is_none()
        );
        let responses = pinned(
            SUPERGROK_UPSTREAM_PINS,
            "POST",
            SUPERGROK_RESPONSES_ORIGIN,
            SUPERGROK_RESPONSES_PATH,
        )
        .unwrap();
        assert_eq!(responses.credential_headers, ["authorization"]);
        assert!(
            pinned(
                XAI_API_UPSTREAM_PINS,
                "POST",
                XAI_PLATFORM_API_KEY_ORIGIN,
                "/v1/responses"
            )
            .is_some()
        );
        assert!(
            pinned(
                XAI_API_UPSTREAM_PINS,
                "POST",
                SUPERGROK_RESPONSES_ORIGIN,
                "/v1/responses"
            )
            .is_none()
        );
        assert!(
            pinned(
                SUPERGROK_UPSTREAM_PINS,
                "POST",
                "https://auth.x.ai",
                "/oauth2/token"
            )
            .is_none()
        );
    }
}
