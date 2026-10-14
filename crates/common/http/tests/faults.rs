//! `PLAK_TEST_FAULTS` fault injection: each fault fires where the plan says
//! and produces the same response or [`Failure`](plakous_http::Failure) the
//! real network event produces, so every classifier above reads it the same.

use std::error::Error;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use plakous_http::{
    Bytes, Client, ConnectionUse, ErrorKind, ExchangeControl, Failure, FaultPlan, HeaderMap,
    HeaderName, Limits, Method, Origin, Phase, Request, StatusCode, TransmissionFact,
};
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

const SSE: &str = "data: one\n\ndata: two\n\ndata: three\n\n";

/// A local TLS peer that answers every connection with `response` (or, with
/// `claimed_length`, a head promising more body than it sends before closing),
/// counting the connections it accepted.
struct Peer {
    client: Client,
    origin: Origin,
    accepted: Arc<AtomicUsize>,
    _task: JoinHandle<()>,
}

async fn peer(
    body: &'static str,
    claimed_length: Option<usize>,
    limits: Limits,
) -> TestResult<Peer> {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["127.0.0.1".to_owned()])?;
    let certificate = cert.der().clone();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
    let mut server = ServerConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(vec![certificate.clone()], key)?;
    server.alpn_protocols = vec![b"http/1.1".to_vec()];
    let mut roots = RootCertStore::empty();
    roots.add(CertificateDer::from(certificate.to_vec()))?;
    let tls = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let origin = Origin::parse(&format!("https://{}", listener.local_addr()?))?;
    let acceptor = TlsAcceptor::from(Arc::new(server));
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&accepted);
    let task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            let Ok(mut stream) = acceptor.accept(stream).await else {
                continue;
            };
            let mut head = [0_u8; 4_096];
            let _ = stream.read(&mut head).await;
            let length = claimed_length.unwrap_or(body.len());
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {length}\r\nconnection: close\r\n\r\n{body}"
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
    });
    Ok(Peer {
        client: Client::with_tls_config(limits, tls)?,
        origin,
        accepted,
        _task: task,
    })
}

fn request(origin: &Origin, path: &str) -> TestResult<Request> {
    Ok(Request::new(
        origin.clone(),
        Method::POST,
        path,
        HeaderMap::new(),
        Bytes::from_static(b"{}"),
        &Limits::default(),
    )?)
}

fn control() -> ExchangeControl {
    ExchangeControl::new(Instant::now() + Duration::from_secs(10))
}

fn plan(spec: &str) -> TestResult<Option<FaultPlan>> {
    Ok(Some(FaultPlan::parse(spec)?))
}

/// Pulls the body to its end: the bytes delivered, then the failure or clean end.
async fn drain(
    client: &Client,
    request: Request,
) -> TestResult<(StatusCode, Vec<u8>, Option<Failure>)> {
    let response = client.execute(request, control()).await?;
    let status = response.status();
    let mut body = response.into_body();
    let mut bytes = Vec::new();
    while let Some(chunk) = body.next_chunk().await {
        match chunk {
            Ok(chunk) => bytes.extend_from_slice(&chunk),
            Err(failure) => return Ok((status, bytes, Some(failure))),
        }
    }
    Ok((status, bytes, None))
}

#[tokio::test]
async fn status_faults_answer_before_anything_is_sent_upstream() -> TestResult {
    for (status, error_type) in [
        (429, "rate_limit_error"),
        (500, "api_error"),
        (502, "api_error"),
        (503, "api_error"),
        (529, "overloaded_error"),
    ] {
        let server = peer(SSE, None, Limits::default()).await?;
        let client = server
            .client
            .with_faults(plan(&format!("fault={status},retry-after=7"))?);
        let response = client
            .execute(request(&server.origin, "/v1/x")?, control())
            .await?;
        assert_eq!(response.status().as_u16(), status);
        assert_eq!(response.connection_use(), None);
        let header = |name: &str| {
            response
                .headers()
                .get(&HeaderName::from_bytes(name.as_bytes()).ok()?)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        assert_eq!(header("retry-after").as_deref(), Some("7"));
        assert_eq!(header("content-type").as_deref(), Some("application/json"));
        let mut body = response.into_body();
        let mut text = Vec::new();
        while let Some(chunk) = body.next_chunk().await {
            text.extend_from_slice(&chunk?);
        }
        let text = String::from_utf8(text)?;
        assert!(
            text.contains(&format!(r#""type":"{error_type}""#)),
            "{text}"
        );
        assert!(text.contains(&format!(r#""code":{status}"#)), "{text}");
        assert_eq!(
            server.accepted.load(Ordering::SeqCst),
            0,
            "HTTP {status} reached the peer"
        );
    }
    Ok(())
}

#[tokio::test]
async fn only_the_nth_matching_request_on_the_route_is_faulted() -> TestResult {
    let server = peer(SSE, None, Limits::default()).await?;
    let client = server
        .client
        .with_faults(plan("path=/v1/chat,nth=2,fault=503")?);
    let mut statuses = Vec::new();
    for path in [
        "/v1/models",
        "/v1/chat",
        "/v1/models",
        "/v1/chat",
        "/v1/chat",
    ] {
        statuses.push(
            drain(&client, request(&server.origin, path)?)
                .await?
                .0
                .as_u16(),
        );
    }
    assert_eq!(statuses, [200, 200, 200, 503, 200]);
    assert_eq!(server.accepted.load(Ordering::SeqCst), 4);
    Ok(())
}

#[tokio::test]
async fn a_connect_fault_fails_like_a_refused_connection() -> TestResult {
    let closed = TcpListener::bind("127.0.0.1:0").await?;
    let refused = Origin::parse(&format!("https://{}", closed.local_addr()?))?;
    drop(closed);
    let real = Client::new(Limits::default())?
        .with_faults(None)
        .execute(request(&refused, "/")?, control())
        .await
        .err()
        .ok_or("refused connect succeeded")?;
    assert_eq!(
        (real.phase(), real.transmission(), real.kind()),
        (Phase::Connect, TransmissionFact::NotStarted, ErrorKind::Io)
    );

    let server = peer(SSE, None, Limits::default()).await?;
    let client = server.client.with_faults(plan("fault=connect")?);
    let injected = client
        .execute(request(&server.origin, "/")?, control())
        .await
        .err()
        .ok_or("connect fault succeeded")?;
    // The injected failure never had a connection to name.
    assert_eq!(real.connection_use(), Some(ConnectionUse::Fresh));
    assert_eq!(injected.connection_use(), None);
    assert_eq!(
        (injected.phase(), injected.transmission(), injected.kind()),
        (real.phase(), real.transmission(), real.kind())
    );
    assert_eq!(injected.cause(), real.cause());
    assert_eq!(server.accepted.load(Ordering::SeqCst), 0);
    Ok(())
}

/// The real event is proven in `transport.rs`
/// (`peer_reset_during_body_write_is_request_incomplete`).
#[tokio::test]
async fn a_write_reset_fails_like_a_peer_that_drops_the_request_body() -> TestResult {
    let server = peer(SSE, None, Limits::default()).await?;
    let client = server.client.with_faults(plan("fault=write-reset")?);
    let injected = client
        .execute(request(&server.origin, "/")?, control())
        .await
        .err()
        .ok_or("write-reset fault succeeded")?;
    assert_eq!(
        (injected.phase(), injected.transmission(), injected.kind()),
        (
            Phase::RequestWriteOrResponseHead,
            TransmissionFact::RequestIncomplete,
            ErrorKind::Protocol
        )
    );
    assert!(injected.transmission().peer_cannot_have_acted());
    assert_eq!(injected.connection_use(), None);
    assert_eq!(server.accepted.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn record_rejected_fault_preserves_tls_rejection_proof() -> TestResult {
    let server = peer(SSE, None, Limits::default()).await?;
    let client = server.client.with_faults(plan("fault=record-rejected")?);
    let failure = client
        .execute(request(&server.origin, "/")?, control())
        .await
        .err()
        .ok_or("record rejection succeeded")?;
    assert_eq!(failure.cause(), Some("peer_rejected_record"));
    assert_eq!(failure.transmission(), TransmissionFact::RequestIncomplete);
    assert_eq!(
        failure.tls_cause(),
        Some(plakous_http::TlsCause::AlertReceived(20))
    );
    assert_eq!(
        failure.wire_protocol(),
        Some(plakous_http::WireProtocol::Http1)
    );
    assert_eq!(server.accepted.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn a_reset_after_events_ends_like_a_peer_that_drops_mid_stream() -> TestResult {
    // The real thing: the peer promises more body than it sends, then closes.
    let dropping = peer(
        "data: one\n\ndata: two\n\n",
        Some(SSE.len() + 64),
        Limits::default(),
    )
    .await?;
    let (_, real_bytes, real) = drain(&dropping.client, request(&dropping.origin, "/")?).await?;
    let real = real.ok_or("the dropped stream ended cleanly")?;

    let server = peer(SSE, None, Limits::default()).await?;
    let client = server
        .client
        .with_faults(plan("fault=reset,after-events=2")?);
    let (status, bytes, failure) = drain(&client, request(&server.origin, "/")?).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, real_bytes);
    assert_eq!(bytes, b"data: one\n\ndata: two\n\n");
    let injected = failure.ok_or("reset fault unexpectedly completed")?;
    assert_eq!(
        (injected.phase(), injected.transmission(), injected.kind()),
        (real.phase(), real.transmission(), real.kind())
    );
    assert!(real.cause().is_some());
    assert_eq!(injected.cause(), None);
    assert_eq!(
        (real.phase(), real.transmission(), real.kind()),
        (
            Phase::ResponseBody,
            TransmissionFact::MayHaveTransmitted,
            ErrorKind::Protocol
        )
    );
    Ok(())
}

#[tokio::test]
async fn an_eof_after_bytes_is_a_clean_end_of_body() -> TestResult {
    let server = peer(SSE, None, Limits::default()).await?;
    let client = server.client.with_faults(plan("fault=eof,after-bytes=13")?);
    let (_, bytes, failure) = drain(&client, request(&server.origin, "/")?).await?;
    assert_eq!(bytes, b"data: one\n\nda");
    assert_eq!(failure, None);
    Ok(())
}

#[tokio::test]
async fn a_stall_is_bounded_by_the_body_deadline_and_otherwise_resumes() -> TestResult {
    let limits = Limits {
        response_body_timeout: Duration::from_millis(200),
        ..Limits::default()
    };
    let server = peer(SSE, None, limits).await?;
    let client = server
        .client
        .with_faults(plan("fault=stall,seconds=30,after-events=1")?);
    let (_, bytes, failure) = drain(&client, request(&server.origin, "/")?).await?;
    assert_eq!(bytes, b"data: one\n\n");
    let failure = failure.ok_or("the stall outlived the body deadline")?;
    assert_eq!(
        (failure.phase(), failure.transmission(), failure.kind()),
        (
            Phase::ResponseBody,
            TransmissionFact::MayHaveTransmitted,
            ErrorKind::DeadlineExceeded
        )
    );

    let server = peer(SSE, None, Limits::default()).await?;
    let client = server
        .client
        .with_faults(plan("fault=stall,seconds=0.3,after-events=1")?);
    let started = Instant::now();
    let (_, bytes, failure) = drain(&client, request(&server.origin, "/")?).await?;
    assert!(started.elapsed() >= Duration::from_millis(300));
    assert_eq!(bytes, SSE.as_bytes());
    assert_eq!(failure, None);
    Ok(())
}

#[test]
fn a_malformed_plan_is_refused_with_its_rule() {
    let error = FaultPlan::parse("fault=429;fault=teapot")
        .err()
        .map(|error| error.to_string());
    assert_eq!(
        error.as_deref(),
        Some("PLAK_TEST_FAULTS rule 2: unknown fault `teapot`")
    );
}
