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
