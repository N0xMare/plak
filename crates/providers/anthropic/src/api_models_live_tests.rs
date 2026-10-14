//! Loopback TLS coverage of the real `GET /v1/models` listing path: headers,
//! `after_id` pagination, cursor checks and every bound. Only the dial origin
//! and trust root differ from production; there is no public dial seam.

use std::sync::Arc;
use std::time::{Duration, Instant};

use plakous_http::{Client, Limits, Origin};
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;

use super::{LIST_TIMEOUT, MAX_BODY_BYTES, list_deadline, list_from};
use crate::{AnthropicApiKey, AnthropicApiKeySource, AnthropicError};

const KEY: &str = "plak-test-anthropic-models-key";

struct Fixture;

impl AnthropicApiKeySource for Fixture {
    fn current_key(&self) -> Result<AnthropicApiKey, AnthropicError> {
        AnthropicApiKey::new(KEY)
    }
}

/// Serves one canned response per connection, in order, and returns each
/// request head.
async fn serve(responses: Vec<Vec<u8>>) -> (Client, Origin, JoinHandle<Vec<String>>) {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).expect("cert");
    let certificate = cert.der().clone();
    let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut server_config = ServerConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_no_client_auth()
        .with_single_cert(vec![certificate.clone()], private_key)
        .expect("server cert");
    server_config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(certificate.as_ref().to_vec()))
        .expect("root");
    let client_config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let origin = Origin::parse(&format!(
        "https://{}/",
        listener.local_addr().expect("addr")
    ))
    .expect("origin");
    let identity = plakous_http::TlsConfigIdentity::from_parts(
        1,
        &[certificate.as_ref(), b"anthropic-models-list-test"],
    )
    .expect("identity");
    let client = Client::with_tls_config_and_identity(Limits::default(), client_config, identity)
        .expect("client");
    let acceptor = TlsAcceptor::from(Arc::new(server_config));
    let server = tokio::spawn(async move {
        let mut heads = Vec::new();
        for response in responses {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let mut stream = acceptor.accept(stream).await.expect("tls");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).await.expect("read");
                assert!(read > 0, "request closed early");
                request.extend_from_slice(&buffer[..read]);
            }
            heads.push(String::from_utf8(request).expect("utf8"));
            let _ = stream.write_all(&response).await;
            let _ = stream.shutdown().await;
        }
        heads
    });
    (client, origin, server)
}

fn http_response(status: &str, body: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

fn page(ids: &[String], has_more: bool, last_id: Option<&str>) -> Vec<u8> {
    let data: Vec<_> = ids
        .iter()
        .map(|id| serde_json::json!({"id": id, "type": "model"}))
        .collect();
    let body = serde_json::json!({"data": data, "has_more": has_more, "last_id": last_id});
    http_response("200 OK", body.to_string().as_bytes())
}

fn ids(prefix: &str, count: usize) -> Vec<String> {
    (0..count)
        .map(|index| format!("{prefix}-{index}"))
        .collect()
}

async fn list(
    client: &Client,
    origin: Origin,
) -> Result<Vec<super::AnthropicListedModel>, AnthropicError> {
    list_from(&Fixture, client, origin, list_deadline().expect("deadline")).await
}

#[tokio::test]
async fn listing_sends_the_key_and_version_and_follows_after_id() {
    let first = ids("claude-a", 2);
    let second = ids("claude-b", 1);
    let (client, origin, server) = serve(vec![
        page(&first, true, Some("claude-a-1")),
        page(&second, false, None),
    ])
    .await;
    let listed = list(&client, origin).await.expect("list");
    assert_eq!(
        listed.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
        ["claude-a-0", "claude-a-1", "claude-b-0"]
    );
    let heads = server.await.expect("server");
    assert_eq!(heads.len(), 2);
    let first_head = heads[0].to_ascii_lowercase();
    assert!(
        first_head.starts_with("get /v1/models?limit=1000 "),
        "{first_head}"
    );
    assert!(
        first_head.contains(&format!("x-api-key: {KEY}")),
        "{first_head}"
    );
    assert!(
        first_head.contains(&format!(
            "anthropic-version: {}",
            crate::ANTHROPIC_API_VERSION
        )),
        "{first_head}"
    );
    assert!(
        first_head.contains("accept: application/json"),
        "{first_head}"
    );
    assert!(!first_head.contains("authorization:"), "{first_head}");
    assert!(
        heads[1]
            .to_ascii_lowercase()
            .starts_with("get /v1/models?limit=1000&after_id=claude-a-1 "),
        "{}",
        heads[1]
    );
}

#[tokio::test]
async fn a_cursor_that_is_not_the_last_row_or_is_unsafe_fails_the_list() {
    for cursor in [Some("claude-elsewhere"), Some("claude-a-1&x=1"), None] {
        let (client, origin, _server) = serve(vec![page(&ids("claude-a", 2), true, cursor)]).await;
        let error = list(&client, origin).await.expect_err("cursor");
        assert!(error.to_string().contains("safe cursor"), "{error}");
    }
}

#[tokio::test]
async fn non_200_and_401_fail_the_list() {
    for status in ["401 Unauthorized", "503 Service Unavailable"] {
        let (client, origin, _server) = serve(vec![http_response(status, b"{}")]).await;
        let error = list(&client, origin).await.expect_err("status");
        let code = status.split(' ').next().expect("code");
        assert!(
            error.to_string().contains(&format!("HTTP {code}")),
            "{error}"
        );
        assert!(!error.to_string().contains(KEY));
    }
}

#[tokio::test]
async fn a_body_over_four_mebibytes_fails_the_list() {
    assert_eq!(MAX_BODY_BYTES, 4 * 1024 * 1024);
    let mut body = vec![b' '; MAX_BODY_BYTES + 1];
    body[0] = b'{';
    let (client, origin, _server) = serve(vec![http_response("200 OK", &body)]).await;
    let error = list(&client, origin).await.expect_err("bound");
    assert!(
        error.to_string().contains("body bound") || error.to_string().contains("request failed"),
        "{error}"
    );
}

#[tokio::test]
async fn more_than_eight_pages_fail_the_list() {
    let pages = (0..9)
        .map(|index| {
            let id = format!("claude-page-{index}");
            page(std::slice::from_ref(&id), true, Some(&id))
        })
        .collect();
    let (client, origin, server) = serve(pages).await;
    let error = list(&client, origin).await.expect_err("pages");
    assert!(error.to_string().contains("page bound"), "{error}");
    drop(server);
}

#[tokio::test]
async fn more_than_2048_rows_across_pages_fail_with_the_row_bound() {
    let first = ids("claude-a", 1000);
    let second = ids("claude-b", 1000);
    let third = ids("claude-c", 1000);
    let (client, origin, _server) = serve(vec![
        page(&first, true, Some("claude-a-999")),
        page(&second, true, Some("claude-b-999")),
        page(&third, false, None),
    ])
    .await;
    let error = list(&client, origin).await.expect_err("rows");
    assert!(error.to_string().contains("row bound"), "{error}");
    assert!(!error.to_string().contains("repeats"), "{error}");
}

#[tokio::test]
async fn a_repeated_id_across_pages_fails_the_list() {
    let (client, origin, _server) = serve(vec![
        page(&ids("claude-a", 2), true, Some("claude-a-1")),
        page(&ids("claude-a", 1), false, None),
    ])
    .await;
    let error = list(&client, origin).await.expect_err("repeat");
    assert!(error.to_string().contains("repeats a model id"), "{error}");
}

#[tokio::test]
async fn the_twenty_second_deadline_bounds_the_whole_listing() {
    assert_eq!(LIST_TIMEOUT, Duration::from_secs(20));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let origin = Origin::parse(&format!(
        "https://{}/",
        listener.local_addr().expect("addr")
    ))
    .expect("origin");
    let client = Client::new_http1(Limits::default()).expect("client");
    let deadline = Instant::now();
    tokio::time::sleep(Duration::from_millis(5)).await;
    let error = list_from(&Fixture, &client, origin, deadline)
        .await
        .expect_err("deadline");
    assert!(error.to_string().contains("request failed"), "{error}");
    let accepted = tokio::time::timeout(Duration::from_millis(50), listener.accept()).await;
    assert!(
        accepted.is_err(),
        "a past deadline must not open a connection"
    );
}
