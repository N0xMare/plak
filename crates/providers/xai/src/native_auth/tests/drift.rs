use super::*;
use plakous_auth::CompatibilityDrift;

#[test]
fn only_the_documented_426_version_shape_is_compatibility_drift() {
    let body = br#"{"error":"Your Grok CLI version (0.2.93) is outdated. Please update to version 1.0.13 or later via `grok update` or the installation documentation."}"#;
    assert_eq!(
        dispatch::version_gate(426, body),
        Some(CompatibilityDrift::VersionGate)
    );
    assert_eq!(dispatch::version_gate(400, body), None);
    assert_eq!(
        dispatch::version_gate(426, br#"{"error":"unrelated refusal"}"#),
        None
    );
}

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
async fn foreign_oidc_issuer_refuses_a_browser_callback_before_the_code_exchange() -> TestResult {
    let endpoint = plakous_test_support::native_auth::test_endpoint().await?;
    let temp = tempdir()?;
    let manager = XaiNativeAuthManager::open_test_slot(
        private_store(&temp)?,
        endpoint.client,
        endpoint.origin.trim_end_matches('/'),
    )?;
    let (settle, mut settled) = oneshot::channel::<()>();
    let peer = tokio::spawn(async move {
        let (tcp, _) = endpoint.listener.accept().await?;
        let mut stream = endpoint.acceptor.accept(tcp).await?;
        let request = plakous_test_support::native_auth::read_request(&mut stream).await?;
        assert!(request.starts_with(b"GET /.well-known/openid-configuration "));
        let mut document = oidc_document();
        document["issuer"] = json!("https://foreign.invalid");
        let reply = plakous_test_support::native_auth::response(
            "200 OK",
            "application/json",
            document.to_string().as_bytes(),
            "",
        );
        stream.write_all(&reply).await?;
        stream.shutdown().await?;
        // Every later connection until the login settles would be a code
        // exchange; count them instead of waiting out a quiet period.
        let mut later = 0_usize;
        loop {
            tokio::select! {
                biased;
                accepted = endpoint.listener.accept() => {
                    accepted?;
                    later += 1;
                }
                _ = &mut settled => break,
            }
        }
        TestResult::Ok(later)
    });
    let login = manager.begin_remote_browser_login()?;
    let parsed = Url::parse(login.authorization_url())?;
    let pairs = parsed.query_pairs().collect::<BTreeMap<_, _>>();
    let state = pairs.get("state").ok_or("browser state")?;
    let failure = login
        .complete(&format!(
            "http://127.0.0.1:56121/callback?code=must-not-exchange&state={state}"
        ))
        .await
        .err()
        .ok_or("foreign issuer admitted")?;
    settle.send(()).map_err(|()| "discovery peer ended early")?;
    assert_eq!(
        peer.await??,
        0,
        "discovery mismatch dispatched the code exchange"
    );
    assert!(matches!(
        failure,
        XaiNativeAuthError::CompatibilityDrift(CompatibilityDrift::Endpoint { field: "issuer" })
    ));
    assert!(failure.to_string().contains("install a newer build"));
    assert!(!failure.to_string().contains("foreign.invalid"));
    assert!(manager.core.store.load()?.is_none());
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
