use super::{
    access_needs_refresh, authorization_code_token_form, capped_deadline, decode_json_bounded,
    refresh_token_form, remaining_until, unsigned_access_token_exp, urlencoded_form,
    urlencoded_form_bytes,
};
use crate::oauth_workflow::OAuthWorkflowError;
use base64::Engine as _;
use serde::Deserialize;
use std::time::{Duration, Instant};

#[derive(Debug, Deserialize, PartialEq)]
struct Sample {
    value: u32,
}

#[test]
fn form_encodes_pairs_in_order_and_escapes() {
    let form = urlencoded_form(&[
        ("grant_type", "authorization_code"),
        ("redirect_uri", "http://127.0.0.1:56121/callback"),
        ("scope", "openid profile"),
    ]);
    assert_eq!(
        form.as_str(),
        "grant_type=authorization_code&redirect_uri=http%3A%2F%2F127.0.0.1%3A56121%2Fcallback&scope=openid+profile"
    );
    let bytes = urlencoded_form_bytes(&[("a", "1"), ("b", "2")]);
    assert_eq!(bytes.as_slice(), b"a=1&b=2");
}

#[test]
fn authorization_code_token_form_is_stable() {
    let fields =
        authorization_code_token_form("code-value", "http://localhost/cb", "client", "verifier");
    assert_eq!(
        fields,
        [
            ("grant_type", "authorization_code"),
            ("code", "code-value"),
            ("redirect_uri", "http://localhost/cb"),
            ("client_id", "client"),
            ("code_verifier", "verifier"),
        ]
    );
    let encoded = urlencoded_form(&fields);
    assert!(encoded.contains("grant_type=authorization_code"));
    assert!(encoded.contains("code=code-value"));
    assert!(encoded.contains("code_verifier=verifier"));
}

#[test]
fn refresh_token_form_is_stable() {
    let fields = refresh_token_form("refresh-secret", "public-client");
    assert_eq!(
        fields,
        [
            ("grant_type", "refresh_token"),
            ("refresh_token", "refresh-secret"),
            ("client_id", "public-client"),
        ]
    );
    let encoded = urlencoded_form(&fields);
    assert!(encoded.contains("grant_type=refresh_token"));
    assert!(encoded.contains("refresh_token=refresh-secret"));
    assert!(encoded.contains("client_id=public-client"));
}

#[test]
fn decode_json_bounded_enforces_size_and_trailing() {
    let ok = decode_json_bounded::<Sample>(br#"{"value":7}"#, 64).expect("json");
    assert_eq!(ok, Sample { value: 7 });
    assert_eq!(
        decode_json_bounded::<Sample>(br#"{"value":7}"#, 5),
        Err(OAuthWorkflowError::ResponseTooLarge)
    );
    assert_eq!(
        decode_json_bounded::<Sample>(b"", 64),
        Err(OAuthWorkflowError::ResponseTooLarge)
    );
    assert_eq!(
        decode_json_bounded::<Sample>(br#"{"value":7}0"#, 64),
        Err(OAuthWorkflowError::InvalidJson)
    );
    assert_eq!(
        decode_json_bounded::<Sample>(br#"{"value":"x"}"#, 64),
        Err(OAuthWorkflowError::InvalidJson)
    );
}

#[test]
fn unsigned_access_token_exp_reads_exp_and_rejects_malformed() {
    let payload =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"exp":1700000000,"sub":"s"}"#);
    let token = format!("e30.{payload}.sig");
    assert_eq!(unsigned_access_token_exp(&token, 4096), Some(1_700_000_000));
    assert_eq!(unsigned_access_token_exp("", 4096), None);
    assert_eq!(unsigned_access_token_exp("not-a-jwt", 4096), None);
    assert_eq!(unsigned_access_token_exp(&token, 8), None);
    let no_exp = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"sub":"s"}"#);
    assert_eq!(
        unsigned_access_token_exp(&format!("e30.{no_exp}.sig"), 4096),
        None
    );
    assert_eq!(
        unsigned_access_token_exp(&format!("e30.{payload}.sig.extra"), 4096),
        None
    );
}

#[test]
fn capped_deadline_never_passes_the_flow_deadline() {
    let now = Instant::now();
    let deadline = now + Duration::from_secs(10);
    assert_eq!(
        capped_deadline(deadline, Duration::from_secs(3), now),
        Some(now + Duration::from_secs(3))
    );
    assert_eq!(
        capped_deadline(deadline, Duration::from_secs(30), now),
        Some(deadline)
    );
    assert_eq!(
        capped_deadline(deadline, Duration::from_secs(3), deadline),
        None
    );
}

#[test]
fn remaining_until_is_positive_only() {
    let now = Instant::now();
    let deadline = now + Duration::from_secs(5);
    assert_eq!(remaining_until(deadline, now), Some(Duration::from_secs(5)));
    assert_eq!(remaining_until(now, now), None);
    assert_eq!(remaining_until(now, deadline), None);
}

#[test]
fn access_needs_refresh_respects_skew() {
    assert!(!access_needs_refresh(1_000, 100, 60));
    assert!(access_needs_refresh(150, 100, 60));
    assert!(access_needs_refresh(100, 100, 0));
}
