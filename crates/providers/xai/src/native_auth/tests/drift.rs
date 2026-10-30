use super::*;
use plakous_auth::CompatibilityDrift;

pub(super) fn oidc_document() -> serde_json::Value {
    let p = XAI_OAUTH;
    let origin = p.auth_origin;
    json!({
        "issuer":origin,
        "authorization_endpoint":format!("{origin}{}", p.authorize_path),
        "token_endpoint":format!("{origin}{}", p.token_path),
        "device_authorization_endpoint":format!("{origin}{}", p.device_authorization_path),
        "revocation_endpoint":format!("{origin}{}", p.revoke_path),
        "userinfo_endpoint":format!("{origin}{}", p.userinfo_path),
        "jwks_uri":format!("{origin}{}", p.jwks_path),
        "scopes_supported":p.scopes.split_ascii_whitespace().collect::<Vec<_>>(),
    })
}

#[tokio::test]
async fn foreign_oidc_token_endpoint_refuses_before_token_or_device_dispatch() -> TestResult {
    let endpoint = plakous_test_support::native_auth::test_endpoint().await?;
    let temp = tempdir()?;
    let manager = XaiNativeAuthManager::open_test_slot(
        private_store(&temp)?,
        endpoint.client,
        endpoint.origin.trim_end_matches('/'),
    )?;
    let peer = tokio::spawn(async move {
        let (tcp, _) = endpoint.listener.accept().await?;
        let mut stream = endpoint.acceptor.accept(tcp).await?;
        let request = plakous_test_support::native_auth::read_request(&mut stream).await?;
        assert!(request.starts_with(b"GET /.well-known/openid-configuration "));
        let mut document = oidc_document();
        document["token_endpoint"] = json!("https://foreign.invalid/oauth2/token");
        let reply = plakous_test_support::native_auth::response(
            "200 OK",
            "application/json",
            document.to_string().as_bytes(),
            "",
        );
        stream.write_all(&reply).await?;
        stream.shutdown().await?;
        assert!(
            tokio::time::timeout(Duration::from_millis(100), endpoint.listener.accept())
                .await
                .is_err(),
            "discovery mismatch dispatched a second request"
        );
        TestResult::Ok(())
    });
    let failure = manager
        .begin_device_login()
        .await
        .err()
        .ok_or("foreign endpoint admitted")?;
    assert!(matches!(
        failure,
        XaiNativeAuthError::CompatibilityDrift(CompatibilityDrift::Endpoint {
            field: "token_endpoint"
        })
    ));
    assert!(failure.to_string().contains("install a newer build"));
    assert!(manager.core.store.load()?.is_none());
    peer.await??;
    Ok(())
}

#[tokio::test]
async fn refresh_accepts_a_differing_granted_scope() -> TestResult {
    let peer = mock_peer().await?;
    let temp = tempdir()?;
    let manager = manager(&temp, &peer)?;
    let tokens: TokenResponse = decode_json(
        br#"{"access_token":"fixture-access","refresh_token":"fixture-refresh","scope":"openid"}"#,
        MAX_TOKEN_BYTES,
    )?;
    let expected = AccountBinding::new("fixture-subject")?;
    let refreshed = manager
        .core
        .validate_tokens(tokens, None, Some(&expected), None, None, None)
        .await?;
    assert_eq!(refreshed.subject, expected.as_str());
    assert_eq!(refreshed.access_token, "fixture-access");
    assert!(peer.requests.lock().await.is_empty());
    assert!(manager.core.store.load()?.is_none());
    peer.abort_join().await?;
    Ok(())
}
