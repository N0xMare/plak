use super::{
    CallbackQueryPolicy, OAuthCallbackOutcome, loopback_redirect_authority,
    parse_absolute_redirect_query, parse_loopback_callback_target, parse_oauth_callback_query,
    parse_remote_authorization_input,
};
use crate::oauth_pins::{browser_test_pins, oidc_test_pins};
use crate::oauth_workflow::OAuthWorkflowError;
use zeroize::Zeroizing;

#[test]
fn standard_callbacks_ignore_extensions_but_reject_duplicate_recognized_keys() {
    let policy = CallbackQueryPolicy::standard();
    let redirect = "http://127.0.0.1:49152/callback";
    let query = "code=c&state=s&session_state=keycloak&other=value&other=ignored";
    assert!(
        parse_absolute_redirect_query(&format!("{redirect}?{query}"), redirect, policy).is_ok()
    );
    assert!(
        parse_loopback_callback_target(
            "127.0.0.1:49152",
            &format!("/callback?{query}"),
            redirect,
            policy
        )
        .is_ok()
    );
    for query in [
        "code=c&state=s&state=s",
        "code=c&code=c&state=s",
        "error=denied&error=denied&state=s",
        "error=denied&state=s&error_description=x&error_description=x",
        "error=denied&state=s&error_uri=x&error_uri=x",
    ] {
        assert!(
            parse_oauth_callback_query(query, policy).is_err(),
            "{query}"
        );
    }
    for policy in [
        CallbackQueryPolicy::codex("openid"),
        CallbackQueryPolicy::xai("openid"),
    ] {
        assert!(parse_oauth_callback_query(query, policy).is_err());
    }
}

#[test]
fn browser_policy_accepts_success_and_error() {
    let pins = browser_test_pins();
    let policy = CallbackQueryPolicy::codex(pins.scopes);
    let state = "state-value-state-value-state-value-01";
    let success = parse_oauth_callback_query(
        &format!(
            "code=auth-code&state={state}&scope={}",
            urlencoding_scope(pins.scopes)
        ),
        policy,
    )
    .expect("success");
    assert_eq!(
        success,
        OAuthCallbackOutcome::AuthorizationCode {
            code: Zeroizing::new(String::from("auth-code")),
            state: Zeroizing::new(String::from(state)),
        }
    );
    let error = parse_oauth_callback_query(
        &format!("error=access_denied&error_description=cancelled&state={state}"),
        policy,
    )
    .expect("error");
    assert_eq!(
        error,
        OAuthCallbackOutcome::ProviderError {
            state: Zeroizing::new(String::from(state)),
        }
    );
    assert_eq!(
        parse_oauth_callback_query(
            &format!("code=auth-code&state={state}&scope=openid"),
            policy,
        ),
        Err(OAuthWorkflowError::Callback)
    );
}

#[test]
fn oidc_policy_parses_absolute_redirect() {
    let pins = oidc_test_pins();
    let policy = CallbackQueryPolicy::xai(pins.scopes);
    let state = "state-value-state-value-state-value-01";
    let full = format!(
        "{}?code=browser-code&state={state}",
        pins.loopback_redirect_uri
    );
    let outcome =
        parse_absolute_redirect_query(&full, pins.loopback_redirect_uri, policy).expect("absolute");
    assert_eq!(
        outcome,
        OAuthCallbackOutcome::AuthorizationCode {
            code: Zeroizing::new(String::from("browser-code")),
            state: Zeroizing::new(String::from(state)),
        }
    );
    assert_eq!(
        parse_absolute_redirect_query(
            &format!("{}?code=x&state={state}#frag", pins.loopback_redirect_uri),
            pins.loopback_redirect_uri,
            policy,
        ),
        Err(OAuthWorkflowError::Callback)
    );
    assert_eq!(
        parse_absolute_redirect_query(
            "http://evil.example/callback?code=x&state=y",
            pins.loopback_redirect_uri,
            policy,
        ),
        Err(OAuthWorkflowError::Callback)
    );
}

#[test]
fn remote_input_accepts_exact_redirect_or_attempt_bound_bare_code() {
    let pins = browser_test_pins();
    let policy = CallbackQueryPolicy::codex(pins.scopes);
    let state = "attempt-state-attempt-state-attempt-state";
    let bare = "opaque/code+safe_value=1234";
    assert_eq!(
        parse_remote_authorization_input(bare, state, pins.loopback_redirect_uri, policy,)
            .expect("bare code"),
        OAuthCallbackOutcome::AuthorizationCode {
            code: Zeroizing::new(bare.to_owned()),
            state: Zeroizing::new(state.to_owned()),
        }
    );
    let full = format!(
        "{}?code=full-code&state={state}",
        pins.loopback_redirect_uri
    );
    assert!(
        parse_remote_authorization_input(&full, state, pins.loopback_redirect_uri, policy,).is_ok()
    );
}

#[test]
fn remote_input_rejection_is_payload_free() {
    let pins = browser_test_pins();
    let secret = "secret-code?with=query";
    let error = parse_remote_authorization_input(
        secret,
        "attempt-state-attempt-state-attempt-state",
        pins.loopback_redirect_uri,
        CallbackQueryPolicy::codex(pins.scopes),
    )
    .expect_err("invalid input");
    assert!(!format!("{error:?}").contains(secret));
}

#[test]
fn loopback_callback_host_and_path_derive_from_the_redirect_used() {
    let policy = CallbackQueryPolicy::codex("openid");
    let redirect = "http://localhost:49152/auth/callback";
    assert_eq!(
        loopback_redirect_authority(redirect),
        Some("localhost:49152")
    );
    for rejected in ["https://localhost:1/cb", "http:///cb", "http://localhost:1"] {
        assert_eq!(loopback_redirect_authority(rejected), None, "{rejected}");
    }
    let target = "/auth/callback?code=c&state=s";
    assert!(matches!(
        parse_loopback_callback_target("localhost:49152", target, redirect, policy),
        Ok(OAuthCallbackOutcome::AuthorizationCode { .. })
    ));
    for (host, target) in [
        ("localhost:1455", target),
        ("127.0.0.1:49152", target),
        ("localhost:49153", target),
        ("localhost:49152", "/other?code=c&state=s"),
        ("localhost:49152", "/auth/callback"),
        ("localhost:49152", "/auth/callback?"),
    ] {
        assert_eq!(
            parse_loopback_callback_target(host, target, redirect, policy),
            Err(OAuthWorkflowError::Callback),
            "{host}{target}"
        );
    }
}

fn urlencoding_scope(scope: &str) -> String {
    url::form_urlencoded::byte_serialize(scope.as_bytes()).collect()
}
