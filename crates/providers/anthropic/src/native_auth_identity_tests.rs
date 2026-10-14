use super::*;
use plakous_http::{Limits, TlsConfigIdentity};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

const ACCOUNT: &str = "12345678-1234-4234-8234-123456789abc";
const OTHER_ACCOUNT: &str = "87654321-4321-4321-8321-cba987654321";

async fn fixture(
    body: serde_json::Value,
) -> (
    tempfile::TempDir,
    ClaudeNativeAuthManager,
    tokio::task::JoinHandle<String>,
) {
    fixture_with_overlapping_reads(body, false).await
}

async fn fixture_with_overlapping_reads(
    body: serde_json::Value,
    overlapping: bool,
) -> (
    tempfile::TempDir,
    ClaudeNativeAuthManager,
    tokio::task::JoinHandle<String>,
) {
    let temp = tempfile::tempdir().unwrap();
    let mut manager = tests::manager(tests::private_store(&temp));
    let certified = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
    let cert = certified.cert.der().clone();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der());
    let mut server = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.clone()], key.into())
        .unwrap();
    server.alpn_protocols = vec![b"http/1.1".to_vec()];
    let identity =
        TlsConfigIdentity::from_parts(1, &[cert.as_ref(), b"claude-identity-fixture"]).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert).unwrap();
    let client = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("https://{}", listener.local_addr().unwrap());
    let core = Arc::get_mut(&mut manager.core).unwrap();
    core.client =
        Client::with_tls_config_and_identity(Limits::default(), client, identity).unwrap();
    core.token_origin = origin.clone();
    core.control_origin = Some(origin);
    let peer = tokio::spawn(async move {
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server));
        // Wait for both requests before replying, so both managers hold a
        // shared Ready lease when the first attempts its exclusive CAS.
        let mut exchanges = Vec::new();
        for _ in 0..if overlapping { 2 } else { 1 } {
            let (socket, _) = listener.accept().await.unwrap();
            let mut stream = acceptor.accept(socket).await.unwrap();
            let request_line = read_fixture_request(&mut stream).await;
            exchanges.push((stream, request_line));
        }
        let body = body.to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let mut request_lines = Vec::new();
        for (index, (mut stream, request_line)) in exchanges.into_iter().enumerate() {
            if index > 0 {
                // Only async progress can release the second lease. A blocking
                // CAS on this current-thread executor prevents this timer firing.
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
            request_lines.push(request_line);
        }
        request_lines.join("\n")
    });
    (temp, manager, peer)
}

async fn read_fixture_request(
    stream: &mut tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
) -> String {
    let mut request = Vec::new();
    let mut chunk = [0_u8; 2048];
    loop {
        let n = stream.read(&mut chunk).await.unwrap();
        assert!(n > 0);
        request.extend_from_slice(&chunk[..n]);
        if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
            let header = String::from_utf8_lossy(&request[..end]);
            let length = header
                .lines()
                .find_map(|line| {
                    line.split_once(':').and_then(|(k, v)| {
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse::<usize>().unwrap())
                    })
                })
                .unwrap_or(0);
            if request.len() >= end + 4 + length {
                break;
            }
        }
    }
    // Only return the request line; fixture bearer values never enter output.
    String::from_utf8(request)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_owned()
}

fn persist_without_uuid(manager: &ClaudeNativeAuthManager) {
    let grant = parse_claude_oauth_token_grant(br#"{"access_token":"fixture-access","refresh_token":"fixture-refresh","scope":"user:inference","expires_in":3600}"#).unwrap();
    manager.core.persist_grant(None, &grant).unwrap();
}

#[tokio::test]
async fn existing_credentials_hydrate_and_reopen_without_relogin() {
    let (temp, manager, peer) = fixture(serde_json::json!({"account":{"uuid":ACCOUNT}})).await;
    persist_without_uuid(&manager);
    let before = manager.dispatch_credentials().unwrap();
    manager.ensure_account_identity().await.unwrap();
    assert_eq!(peer.await.unwrap(), "GET /api/oauth/profile HTTP/1.1");
    let ((token, generation, binding), identity) = manager.identity_credentials().unwrap();
    assert_eq!(token, before.0);
    assert_eq!(generation, before.1 + 1);
    assert_eq!(binding, before.2);
    assert_eq!(identity.account_uuid().as_str(), ACCOUNT);
    assert_eq!(identity.device_id().as_str(), "ab".repeat(32));
    // A subsequent read uses durable metadata and requires no live socket.
    manager.ensure_account_identity().await.unwrap();
    let reopened = tests::manager(temp.path().join("credentials/claude.json"));
    assert_eq!(reopened.request_identity().unwrap(), identity);
}

#[tokio::test]
async fn missing_profile_uuid_preserves_credential_and_refuses_identity() {
    let (_temp, manager, peer) =
        fixture(serde_json::json!({"account":{"email":"fixture@example.invalid"}})).await;
    persist_without_uuid(&manager);
    let before = manager.core.store.load().unwrap().unwrap();
    assert!(matches!(
        manager.ensure_account_identity().await,
        Err(ClaudeNativeAuthError::AccountIdentityUnavailable)
    ));
    peer.await.unwrap();
    assert_eq!(manager.core.store.load().unwrap().unwrap(), before);
    assert!(manager.request_identity().is_err());
}

#[tokio::test]
async fn refresh_preserves_or_validates_provider_account_uuid() {
    for uuid in [None, Some(ACCOUNT), Some(OTHER_ACCOUNT)] {
        let mut body = serde_json::json!({"access_token":"fixture-new-access","refresh_token":"fixture-new-refresh","expires_in":3600,"scope":"user:inference"});
        if let Some(uuid) = uuid {
            body["account"] = serde_json::json!({"uuid":uuid});
        }
        let (_temp, manager, peer) = fixture(body).await;
        manager
            .persist_test_grant("fixture-access", "fixture-refresh", 3600, "user:inference")
            .unwrap();
        let result = manager.refresh_after_rejection().await;
        assert_eq!(peer.await.unwrap(), "POST /v1/oauth/token HTTP/1.1");
        if uuid == Some(OTHER_ACCOUNT) {
            assert!(result.is_err());
            assert_eq!(
                manager.core.store.load().unwrap().unwrap().status(),
                CredentialStatus::RefreshOutcomeUnknown
            );
        } else {
            result.unwrap();
            assert_eq!(
                manager.request_identity().unwrap().account_uuid().as_str(),
                ACCOUNT
            );
        }
    }
}

#[test]
fn stale_hydration_cannot_replace_refresh_or_login() {
    for login in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let manager = tests::manager(tests::private_store(&temp));
        persist_without_uuid(&manager);
        let before = manager.core.store.load().unwrap().unwrap();
        let mut stale = manager.core.read_stored(&before).unwrap();
        stale.account_uuid = Some(crate::ClaudeAccountUuid::new(ACCOUNT).unwrap());
        if login {
            manager
                .persist_test_grant("other-access", "other-refresh", 3600, "user:inference")
                .unwrap();
        } else {
            let mut refreshed = manager.core.read_stored(&before).unwrap();
            refreshed.access_token = "refreshed-access".to_owned();
            refreshed.refresh_token = "refreshed-refresh".to_owned();
            let replacement =
                credential_record(before.generation().next().unwrap(), &refreshed).unwrap();
            manager
                .core
                .store
                .replace(before.generation(), &replacement)
                .unwrap();
        }
        let replacement = credential_record(before.generation().next().unwrap(), &stale).unwrap();
        assert!(matches!(
            manager
                .core
                .store
                .replace(before.generation(), &replacement),
            Err(PrivateStoreError::StaleWriter)
        ));
        let after = manager.core.store.load().unwrap().unwrap();
        assert_eq!(after.account_binding() != before.account_binding(), login);
        assert_ne!(
            manager.core.read_stored(&after).unwrap().access_token,
            stale.access_token
        );
    }
}

#[tokio::test]
async fn revalidate_picks_up_rotated_credential_generation() {
    let (_temp, manager, peer) = fixture(serde_json::json!({"access_token":"fixture-new-access","refresh_token":"fixture-new-refresh","expires_in":3600,"scope":"user:inference"})).await;
    manager
        .persist_test_grant("fixture-access", "fixture-refresh", 3600, "user:inference")
        .unwrap();
    let (model, authority, _) = manager
        .resolve_model(
            "claude-sonnet-5",
            manager.core.client.clone(),
            plakous_model::SessionLineage::Root,
        )
        .await
        .unwrap();
    manager.refresh_after_rejection().await.unwrap();
    peer.await.unwrap();
    let refreshed = model
        .revalidate_runtime_authority(authority.observed_at())
        .await
        .unwrap();
    assert!(refreshed.credential_generation() > authority.credential_generation());
    assert_eq!(refreshed.account_binding(), authority.account_binding());
    assert_eq!(
        refreshed.credential_authority_binding(),
        authority.credential_authority_binding()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn separate_managers_hydrate_concurrently_without_blocking_the_executor() {
    let (temp, first, peer) =
        fixture_with_overlapping_reads(serde_json::json!({"account":{"uuid":ACCOUNT}}), true).await;
    persist_without_uuid(&first);
    let before = first.core.store.load().unwrap().unwrap();
    let mut second = tests::manager(temp.path().join("credentials/claude.json"));
    let core = Arc::get_mut(&mut second.core).unwrap();
    core.client = first.core.client.clone();
    core.control_origin.clone_from(&first.core.control_origin);
    let (first_result, second_result) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(
            first.ensure_account_identity(),
            second.ensure_account_identity()
        )
    })
    .await
    .unwrap();
    first_result.unwrap();
    second_result.unwrap();
    assert_eq!(
        peer.await.unwrap(),
        "GET /api/oauth/profile HTTP/1.1\nGET /api/oauth/profile HTTP/1.1"
    );
    let after = first.core.store.load().unwrap().unwrap();
    assert_eq!(after.generation().get(), before.generation().get() + 1);
    assert_eq!(after.account_binding(), before.account_binding());
    assert_eq!(
        first.request_identity().unwrap(),
        second.request_identity().unwrap()
    );
    assert_eq!(
        first.request_identity().unwrap().account_uuid().as_str(),
        ACCOUNT
    );
}
