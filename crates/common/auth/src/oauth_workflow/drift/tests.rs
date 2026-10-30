use super::*;

fn metadata(pins: &SubscriptionOAuthPins) -> Value {
    let base = pins.auth_origin.trim_end_matches('/');
    serde_json::json!({
        "issuer": pins.auth_origin,
        "authorization_endpoint": format!("{base}{}", pins.authorize_path),
        "token_endpoint": format!("{base}{}", pins.token_path),
        "device_authorization_endpoint": format!("{base}{}", pins.device_authorization_path),
        "revocation_endpoint": format!("{base}{}", pins.revoke_path),
        "userinfo_endpoint": format!("{base}{}", pins.userinfo_path),
        "jwks_uri": format!("{base}{}", pins.jwks_path),
        "scopes_supported": pins.scopes.split_ascii_whitespace().collect::<Vec<_>>(),
    })
}

#[test]
fn every_oidc_endpoint_is_compared_without_retaining_discovered_urls() {
    let pins = crate::oauth_pins::browser_test_pins();
    let original = metadata(&pins);
    assert_eq!(validate_subscription_oidc(pins, &original), Ok(()));
    for field in [
        "issuer",
        "authorization_endpoint",
        "token_endpoint",
        "device_authorization_endpoint",
        "revocation_endpoint",
        "userinfo_endpoint",
        "jwks_uri",
    ] {
        let mut changed = original.clone();
        changed[field] = Value::String("https://foreign.invalid/token".into());
        let failure =
            validate_subscription_oidc(pins, &changed).expect_err("foreign endpoint admitted");
        assert_eq!(failure, CompatibilityDrift::Endpoint { field });
        assert!(!format!("{failure:?}").contains("foreign.invalid"));
        assert!(
            failure
                .message("test route")
                .ends_with("install a newer build")
        );
    }
}

#[test]
fn oidc_supported_scopes_must_include_every_requested_scope() {
    let pins = crate::oauth_pins::browser_test_pins();
    let mut document = metadata(&pins);
    document["scopes_supported"] = serde_json::json!([]);
    assert_eq!(
        validate_subscription_oidc(pins, &document),
        Err(CompatibilityDrift::Endpoint {
            field: "scopes_supported"
        })
    );
    document
        .as_object_mut()
        .expect("object")
        .remove("scopes_supported");
    assert_eq!(validate_subscription_oidc(pins, &document), Ok(()));
}

#[test]
fn granted_scope_requires_only_needed_scopes_and_accepts_supersets() {
    for scope in ["openid", ""] {
        assert_eq!(
            validate_granted_scope(Some(scope), "openid profile"),
            Err(CompatibilityDrift::GrantedScope)
        );
    }
    for scope in [
        "profile openid",
        "openid profile extra",
        "openid profile profile",
    ] {
        assert_eq!(
            validate_granted_scope(Some(scope), "openid profile"),
            Ok(())
        );
    }
    assert_eq!(validate_granted_scope(None, "openid profile"), Ok(()));
}
