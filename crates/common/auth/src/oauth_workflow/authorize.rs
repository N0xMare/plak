//! Pure authorization URL construction from subscription OAuth pins.

use url::Url;
use zeroize::Zeroizing;

use crate::oauth_pins::SubscriptionOAuthPins;

use super::OAuthWorkflowError;

/// Inputs for a PKCE authorization-code authorize URL.
///
/// Dialect-specific query pairs (for example `originator`, `plan`, `referrer`)
/// belong in [`Self::extra_pairs`]. Secrets other than correlation material must
/// not appear here; state may be sensitive and the resulting URL is zeroized on
/// drop.
#[derive(Clone, Copy, Debug)]
pub struct AuthorizeUrlRequest<'a> {
    /// Dialect pin table providing client id, default origin, path, and scopes.
    pub pins: SubscriptionOAuthPins,
    /// Optional override of [`SubscriptionOAuthPins::auth_origin`].
    ///
    /// Managers pass their resolved endpoint origin so test doubles and custom
    /// profiles stay authoritative. When `None`, the pin-table origin is used.
    pub auth_origin: Option<&'a str>,
    /// Exact redirect URI registered for this login attempt.
    pub redirect_uri: &'a str,
    /// Public PKCE S256 code challenge.
    pub code_challenge: &'a str,
    /// Opaque OAuth `state` correlation value.
    pub state: &'a str,
    /// Optional OIDC `nonce` when the dialect includes it on the authorize URL.
    pub nonce: Option<&'a str>,
    /// Additional dialect-owned query pairs, appended after standard fields.
    pub extra_pairs: &'a [(&'a str, &'a str)],
}

/// Builds a PKCE authorization URL from pins and attempt material.
///
/// Standard query fields, in order:
/// `response_type`, `client_id`, `redirect_uri`, `scope`, `code_challenge`,
/// `code_challenge_method=S256`, `state`, optional `nonce`, then `extra_pairs`.
///
/// # Errors
///
/// Returns [`OAuthWorkflowError::InvalidUrl`] when the pin origin or path cannot
/// form an absolute HTTP(S) URL, or when required string inputs are empty.
pub fn build_authorization_url(
    request: AuthorizeUrlRequest<'_>,
) -> Result<Zeroizing<String>, OAuthWorkflowError> {
    if request.redirect_uri.is_empty()
        || request.code_challenge.is_empty()
        || request.state.is_empty()
        || request.pins.client_id.is_empty()
        || request.pins.scopes.is_empty()
        || request.pins.authorize_path.is_empty()
        || request.nonce.is_some_and(str::is_empty)
        || request.auth_origin.is_some_and(str::is_empty)
    {
        return Err(OAuthWorkflowError::InvalidUrl);
    }

    let origin = request.auth_origin.unwrap_or(request.pins.auth_origin);
    let mut url = Url::parse(origin).map_err(|_| OAuthWorkflowError::InvalidUrl)?;
    if url.scheme() != "https" && url.scheme() != "http" {
        return Err(OAuthWorkflowError::InvalidUrl);
    }
    url.set_path(request.pins.authorize_path);
    url.set_query(None);
    url.set_fragment(None);

    {
        let mut pairs = url.query_pairs_mut();
        pairs
            .append_pair("response_type", "code")
            .append_pair("client_id", request.pins.client_id)
            .append_pair("redirect_uri", request.redirect_uri)
            .append_pair("scope", request.pins.scopes)
            .append_pair("code_challenge", request.code_challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", request.state);
        if let Some(nonce) = request.nonce {
            pairs.append_pair("nonce", nonce);
        }
        for (name, value) in request.extra_pairs {
            if name.is_empty() {
                return Err(OAuthWorkflowError::InvalidUrl);
            }
            pairs.append_pair(name, value);
        }
    }

    Ok(Zeroizing::new(url.into()))
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use super::{AuthorizeUrlRequest, build_authorization_url};
    use crate::oauth_pins::{browser_test_pins, oidc_test_pins};
    use crate::oauth_workflow::OAuthWorkflowError;
    use std::collections::BTreeMap;
    use url::Url;

    fn pairs(url: &str) -> BTreeMap<String, String> {
        Url::parse(url)
            .expect("url")
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }

    #[test]
    fn browser_pins_build_expected_authorize_url() {
        let pins = browser_test_pins();
        let url = build_authorization_url(AuthorizeUrlRequest {
            pins,
            auth_origin: None,
            redirect_uri: pins.loopback_redirect_uri,
            code_challenge: "challenge-value",
            state: "state-value-state-value-state-value-01",
            nonce: None,
            extra_pairs: &[
                ("id_token_add_organizations", "true"),
                ("codex_cli_simplified_flow", "true"),
                ("originator", "plak"),
            ],
        })
        .expect("codex authorize url");

        assert!(url.starts_with("https://browser.example/oauth/authorize?"));
        let map = pairs(&url);
        assert_eq!(map.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(
            map.get("client_id").map(String::as_str),
            Some(pins.client_id)
        );
        assert_eq!(
            map.get("redirect_uri").map(String::as_str),
            Some(pins.loopback_redirect_uri)
        );
        assert_eq!(map.get("scope").map(String::as_str), Some(pins.scopes));
        assert_eq!(
            map.get("code_challenge").map(String::as_str),
            Some("challenge-value")
        );
        assert_eq!(
            map.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(
            map.get("state").map(String::as_str),
            Some("state-value-state-value-state-value-01")
        );
        assert!(!map.contains_key("nonce"));
        assert_eq!(
            map.get("id_token_add_organizations").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            map.get("codex_cli_simplified_flow").map(String::as_str),
            Some("true")
        );
        assert_eq!(map.get("originator").map(String::as_str), Some("plak"));
    }

    #[test]
    fn oidc_pins_build_expected_authorize_url_with_nonce() {
        let pins = oidc_test_pins();
        let url = build_authorization_url(AuthorizeUrlRequest {
            pins,
            auth_origin: None,
            redirect_uri: pins.loopback_redirect_uri,
            code_challenge: "challenge-value",
            state: "state-value-state-value-state-value-01",
            nonce: Some("nonce-value-nonce-value-nonce-value-01"),
            extra_pairs: &[("plan", "generic"), ("referrer", "plak")],
        })
        .expect("xai authorize url");

        assert!(url.starts_with("https://oidc.example/oauth2/authorize?"));
        let map = pairs(&url);
        assert_eq!(
            map.get("client_id").map(String::as_str),
            Some(pins.client_id)
        );
        assert_eq!(
            map.get("redirect_uri").map(String::as_str),
            Some(pins.loopback_redirect_uri)
        );
        assert_eq!(map.get("scope").map(String::as_str), Some(pins.scopes));
        assert_eq!(
            map.get("nonce").map(String::as_str),
            Some("nonce-value-nonce-value-nonce-value-01")
        );
        assert_eq!(map.get("plan").map(String::as_str), Some("generic"));
        assert_eq!(map.get("referrer").map(String::as_str), Some("plak"));
        assert!(!url.contains("referrer=opencode"));
        assert!(!url.contains("grok-build"));
    }

    #[test]
    fn browser_and_oidc_authorize_urls_remain_distinct() {
        let codex = browser_test_pins();
        let xai = oidc_test_pins();
        let codex_url = build_authorization_url(AuthorizeUrlRequest {
            pins: codex,
            auth_origin: None,
            redirect_uri: codex.loopback_redirect_uri,
            code_challenge: "c",
            state: "s",
            nonce: None,
            extra_pairs: &[],
        })
        .expect("codex");
        let xai_url = build_authorization_url(AuthorizeUrlRequest {
            pins: xai,
            auth_origin: None,
            redirect_uri: xai.loopback_redirect_uri,
            code_challenge: "c",
            state: "s",
            nonce: Some("n"),
            extra_pairs: &[],
        })
        .expect("xai");
        assert_ne!(codex_url.as_str(), xai_url.as_str());
        assert!(codex_url.contains(codex.client_id));
        assert!(xai_url.contains(xai.client_id));
    }

    #[test]
    fn auth_origin_override_replaces_pin_origin() {
        let pins = browser_test_pins();
        let url = build_authorization_url(AuthorizeUrlRequest {
            pins,
            auth_origin: Some("https://127.0.0.1:9443/"),
            redirect_uri: pins.loopback_redirect_uri,
            code_challenge: "c",
            state: "s",
            nonce: None,
            extra_pairs: &[],
        })
        .expect("override");
        assert!(url.starts_with("https://127.0.0.1:9443/oauth/authorize?"));
        assert!(!url.contains("auth.openai.com"));
    }

    #[test]
    fn rejects_empty_required_fields() {
        let pins = browser_test_pins();
        assert_eq!(
            build_authorization_url(AuthorizeUrlRequest {
                pins,
                auth_origin: None,
                redirect_uri: "",
                code_challenge: "c",
                state: "s",
                nonce: None,
                extra_pairs: &[],
            }),
            Err(OAuthWorkflowError::InvalidUrl)
        );
        assert_eq!(
            build_authorization_url(AuthorizeUrlRequest {
                pins,
                auth_origin: None,
                redirect_uri: "http://localhost/cb",
                code_challenge: "c",
                state: "s",
                nonce: Some(""),
                extra_pairs: &[],
            }),
            Err(OAuthWorkflowError::InvalidUrl)
        );
    }
}
