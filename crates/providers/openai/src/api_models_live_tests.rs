//! Loopback coverage of the real `GET /v1/models` listing. Only the dial
//! origin and trust root differ from production; there is no public dial seam.

use std::sync::Arc;
use std::time::Duration;

use plakous_http::{Client, Limits, Origin};
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;

use super::{get_models, list_via};
use crate::{
    CredentialAuthorityId, CredentialReference, SecretString, StaticPlatformCredentialSource,
};

fn reference() -> CredentialReference {
    CredentialReference::application("openai-list-fixture-reference").expect("reference")
}

fn fixture_source() -> StaticPlatformCredentialSource {
    StaticPlatformCredentialSource::new(
        SecretString::new("plak-test-openai-models-key").expect("key"),
        reference(),
        CredentialAuthorityId::new("openai-list-fixture").expect("authority"),
    )
}

struct Loop {
    listener: TcpListener,
    acceptor: TlsAcceptor,
    client: Client,
    origin: Origin,
}

async fn loopback() -> Loop {
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
        &[certificate.as_ref(), b"openai-models-list-test"],
    )
    .expect("identity");
    let client = Client::with_tls_config_and_identity(Limits::default(), client_config, identity)
        .expect("client");
    Loop {
        listener,
        acceptor: TlsAcceptor::from(Arc::new(server_config)),
        client,
        origin,
    }
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

async fn read_head(stream: &mut tokio_rustls::server::TlsStream<TcpStream>) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        let read = stream.read(&mut buffer).await.expect("read");
        assert!(read > 0, "request closed early");
        request.extend_from_slice(&buffer[..read]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    request
}

#[tokio::test]
async fn past_deadline_does_not_connect() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let origin = Origin::parse(&format!(
        "https://{}/",
        listener.local_addr().expect("addr")
    ))
    .expect("origin");
    let client = Client::new_http1(Limits::default()).expect("client");
    let deadline = std::time::Instant::now();
    tokio::time::sleep(Duration::from_millis(5)).await;
    let error = get_models(&client, origin, "unused", deadline)
        .await
        .expect_err("deadline");
    assert!(error.to_string().contains("DeadlineExceeded"), "{error}");
    let accepted = tokio::time::timeout(Duration::from_millis(50), listener.accept()).await;
    assert!(
        accepted.is_err(),
        "a past deadline must not open a connection"
    );
}

#[tokio::test]
async fn loopback_list_sends_the_bearer_and_ignores_a_cursor() {
    let endpoint = loopback().await;
    let origin = endpoint.origin.clone();
    let client = endpoint.client.clone();
    let body =
        br#"{"object":"list","has_more":true,"data":[{"id":"gpt-6-luna","context_window":9}]}"#;
    let server = tokio::spawn(async move {
        let (stream, _) = endpoint.listener.accept().await.expect("accept");
        let mut stream = endpoint.acceptor.accept(stream).await.expect("tls");
        let request = read_head(&mut stream).await;
        stream
            .write_all(&http_response("200 OK", body))
            .await
            .expect("write");
        stream.shutdown().await.expect("shutdown");
        request
    });
    let listed = list_via(&reference(), &fixture_source(), &client, origin)
        .await
        .expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, "gpt-6-luna");
    let request = String::from_utf8(server.await.expect("server")).expect("utf8");
    let head = request.to_ascii_lowercase();
    assert!(head.starts_with("get /v1/models "), "{request}");
    assert!(head.contains("accept: application/json"), "{request}");
    assert!(
        head.contains("authorization: bearer plak-test-openai-models-key"),
        "{request}"
    );
}

#[tokio::test]
async fn permanent_non_200_fails_the_list() {
    let endpoint = loopback().await;
    let origin = endpoint.origin.clone();
    let client = endpoint.client.clone();
    tokio::spawn(async move {
        let (stream, _) = endpoint.listener.accept().await.expect("accept");
        let mut stream = endpoint.acceptor.accept(stream).await.expect("tls");
        let _ = read_head(&mut stream).await;
        stream
            .write_all(&http_response("400 Bad Request", b"{}"))
            .await
            .expect("write");
        stream.shutdown().await.expect("shutdown");
    });
    let error = list_via(&reference(), &fixture_source(), &client, origin)
        .await
        .expect_err("status");
    assert!(error.to_string().contains("HTTP 400"), "{error}");
}

#[tokio::test]
async fn oversized_body_fails_the_list() {
    let endpoint = loopback().await;
    let origin = endpoint.origin.clone();
    let client = endpoint.client.clone();
    tokio::spawn(async move {
        let (stream, _) = endpoint.listener.accept().await.expect("accept");
        let mut stream = endpoint.acceptor.accept(stream).await.expect("tls");
        let _ = read_head(&mut stream).await;
        let mut body = vec![b' '; super::MAX_BODY_BYTES + 1];
        body[..1].copy_from_slice(b"{");
        let response = http_response("200 OK", &body);
        stream.write_all(&response).await.expect("write");
        stream.shutdown().await.expect("shutdown");
    });
    let error = list_via(&reference(), &fixture_source(), &client, origin)
        .await
        .expect_err("bound");
    assert!(
        error.to_string().contains("body bound") || error.to_string().contains("request failed"),
        "{error}"
    );
}

#[test]
fn the_fingerprint_reads_the_key_under_the_callers_reference() {
    let fingerprint =
        super::openai_api_key_fingerprint(&reference(), &fixture_source()).expect("fingerprint");
    assert_eq!(fingerprint.len(), 16);
    let other = CredentialReference::application("plak/openai").expect("reference");
    assert!(super::openai_api_key_fingerprint(&other, &fixture_source()).is_err());
}
