//! Loopback TLS tests for connection reuse: what is kept, what is never kept,
//! and how a reused connection's failures are reported.

use std::convert::Infallible;
use std::error::Error;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use http_body_util::{BodyExt as _, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

use super::{IDLE_BOUND, MAX_IDLE_PER_KEY, Pool};
use crate::{
    Bytes, CancellationToken, Client, ConnectionUse, ErrorKind, ExchangeControl, Failure,
    HeaderMap, Limits, Method, Origin, Phase, Request, TlsCause, TransmissionFact, WireProtocol,
};

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

const H1: &[u8] = b"http/1.1";
const H2: &[u8] = b"h2";

struct Endpoint {
    listener: TcpListener,
    acceptor: TlsAcceptor,
    client: ClientConfig,
    origin: Origin,
}

async fn endpoint(alpn: &[u8]) -> TestResult<Endpoint> {
    endpoint_with_versions(alpn, rustls::DEFAULT_VERSIONS).await
}

async fn endpoint_with_versions(
    alpn: &[u8],
    versions: &[&'static rustls::SupportedProtocolVersion],
) -> TestResult<Endpoint> {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["127.0.0.1".to_owned()])?;
    let certificate = cert.der().clone();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
    let mut server = ServerConfig::builder_with_provider(Arc::clone(&provider))
        .with_protocol_versions(versions)?
        .with_no_client_auth()
        .with_single_cert(vec![certificate.clone()], key)?;
    server.alpn_protocols = vec![alpn.to_vec()];
    server.send_tls13_tickets = 0;
    let mut roots = RootCertStore::empty();
    roots.add(CertificateDer::from(certificate.to_vec()))?;
    let client = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(versions)?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let origin = Origin::parse(&format!("https://{}", listener.local_addr()?))?;
    Ok(Endpoint {
        listener,
        acceptor: TlsAcceptor::from(Arc::new(server)),
        client,
        origin,
    })
}

/// A keep-alive peer that counts the connections it accepted and closed.
/// `/big` answers 256 KiB, `/close` asks an HTTP/1.1 client to close, and
/// anything else answers `ok`. Sending on `shutdown` closes every connection
/// gracefully, with a GOAWAY on HTTP/2.
struct Server {
    client: ClientConfig,
    origin: Origin,
    connections: Arc<AtomicUsize>,
    closed: Arc<AtomicUsize>,
    shutdown: watch::Sender<bool>,
    /// Drops every later connection before its TLS handshake.
    refuse: Arc<std::sync::atomic::AtomicBool>,
    _task: JoinHandle<()>,
}

impl Server {
    async fn start(alpn: &'static [u8]) -> TestResult<Self> {
        let Endpoint {
            listener,
            acceptor,
            client,
            origin,
        } = endpoint(alpn).await?;
        let connections = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicUsize::new(0));
        let (shutdown, shutdown_seen) = watch::channel(false);
        let refuse = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let refusing = Arc::clone(&refuse);
        let accepted = Arc::clone(&connections);
        let ended = Arc::clone(&closed);
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                accepted.fetch_add(1, Ordering::SeqCst);
                if refusing.load(Ordering::SeqCst) {
                    continue;
                }
                let acceptor = acceptor.clone();
                let ended = Arc::clone(&ended);
                let mut shutdown = shutdown_seen.clone();
                // Only a later `shutdown` closes a connection accepted now.
                shutdown.borrow_and_update();
                tokio::spawn(async move {
                    if let Ok(stream) = acceptor.accept(stream).await {
                        serve(stream, alpn, shutdown).await;
                    }
                    ended.fetch_add(1, Ordering::SeqCst);
                });
            }
        });
        Ok(Self {
            client,
            origin,
            connections,
            closed,
            shutdown,
            refuse,
            _task: task,
        })
    }

    fn pooled_client(&self, limits: Limits) -> TestResult<Client> {
        Ok(Client::with_tls_config(limits, self.client.clone())?.pooled())
    }

    fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    fn closed(&self) -> usize {
        self.closed.load(Ordering::SeqCst)
    }
}

async fn serve(stream: TlsStream<TcpStream>, alpn: &[u8], mut shutdown: watch::Receiver<bool>) {
    let io = TokioIo::new(stream);
    let service = service_fn(respond);
    if alpn == H2 {
        let connection = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
            .serve_connection(io, service);
        tokio::pin!(connection);
        tokio::select! {
            _ = &mut connection => return,
            _ = shutdown.changed() => connection.as_mut().graceful_shutdown(),
        }
        let _ = connection.await;
    } else {
        let connection = hyper::server::conn::http1::Builder::new().serve_connection(io, service);
        tokio::pin!(connection);
        tokio::select! {
            _ = &mut connection => return,
            _ = shutdown.changed() => connection.as_mut().graceful_shutdown(),
        }
        let _ = connection.await;
    }
}

async fn respond(
    request: hyper::Request<Incoming>,
) -> Result<hyper::Response<Full<Bytes>>, Infallible> {
    let path = request.uri().path().to_owned();
    let _ = request.into_body().collect().await;
    let body = if path == "/big" {
        Bytes::from(vec![b'x'; 256 * 1_024])
    } else {
        Bytes::from_static(b"ok")
    };
    let mut response = hyper::Response::new(Full::new(body));
    if path == "/close" {
        response.headers_mut().insert(
            hyper::header::CONNECTION,
            hyper::header::HeaderValue::from_static("close"),
        );
    }
    Ok(response)
}

fn request(origin: &Origin, path: &str, body: Bytes, limits: &Limits) -> TestResult<Request> {
    Ok(Request::new(
        origin.clone(),
        Method::POST,
        path,
        HeaderMap::new(),
        body,
        limits,
    )?)
}

fn control() -> ExchangeControl {
    ExchangeControl::new(Instant::now() + Duration::from_secs(10))
}

fn pool(client: &Client) -> TestResult<&Arc<Pool>> {
    client
        .inner
        .pool
        .as_ref()
        .ok_or_else(|| "the client is not pooled".into())
}

/// Runs one exchange to its clean end and reports how its connection was used.
async fn exchange(client: &Client, origin: &Origin, path: &str) -> TestResult<ConnectionUse> {
    let limits = client.limits().clone();
    let response = client
        .execute(
            request(origin, path, Bytes::from_static(b"{}"), &limits)?,
            control(),
        )
        .await?;
    let connection = response
        .connection_use()
        .ok_or("a real exchange names its connection")?;
    let mut body = response.into_body();
    while let Some(chunk) = body.next_chunk().await {
        chunk?;
    }
    Ok(connection)
}

async fn eventually(mut condition: impl FnMut() -> bool) -> bool {
    for _ in 0..500 {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    condition()
}

#[tokio::test]
async fn sequential_exchanges_reuse_one_connection_on_both_protocols() -> TestResult {
    for alpn in [H1, H2] {
        let server = Server::start(alpn).await?;
        let client = server.pooled_client(Limits::default())?;
        let mut uses = Vec::new();
        for _ in 0..3 {
            uses.push(exchange(&client, &server.origin, "/ok").await?);
        }
        assert_eq!(
            uses,
            [
                ConnectionUse::Fresh,
                ConnectionUse::Reused,
                ConnectionUse::Reused
            ]
        );
        assert_eq!(server.connections(), 1);
        assert_eq!(pool(&client)?.idle_count(), 1);
    }
    Ok(())
}

#[tokio::test]
async fn fresh_control_bypasses_healthy_idle_connections_and_keeps_checkin() -> TestResult {
    let server = Server::start(H1).await?;
    let client = server.pooled_client(Limits::default())?;
    assert_eq!(
        exchange(&client, &server.origin, "/ok").await?,
        ConnectionUse::Fresh
    );
    let response = client
        .execute(
            request(
                &server.origin,
                "/ok",
                Bytes::from_static(b"{}"),
                client.limits(),
            )?,
            control().with_fresh_connection(true),
        )
        .await?;
    assert_eq!(response.connection_use(), Some(ConnectionUse::Fresh));
    let mut body = response.into_body();
    while let Some(chunk) = body.next_chunk().await {
        chunk?;
    }
    assert_eq!(server.connections(), 2);
    assert_eq!(pool(&client)?.idle_count(), 2);
    assert_eq!(
        exchange(&client, &server.origin, "/ok").await?,
        ConnectionUse::Reused
    );
    Ok(())
}

#[tokio::test]
async fn an_unpooled_client_opens_a_fresh_connection_every_time() -> TestResult {
    for alpn in [H1, H2] {
        let server = Server::start(alpn).await?;
        let client = Client::with_tls_config(Limits::default(), server.client.clone())?;
        assert!(!client.is_pooled());
        for _ in 0..2 {
            assert_eq!(
                exchange(&client, &server.origin, "/ok").await?,
                ConnectionUse::Fresh
            );
        }
        assert_eq!(server.connections(), 2);
        assert!(eventually(|| server.closed() == 2).await);
    }
    Ok(())
}

#[tokio::test]
async fn concurrent_exchanges_hold_separate_connections_and_at_most_two_stay_idle() -> TestResult {
    for alpn in [H1, H2] {
        let server = Server::start(alpn).await?;
        let client = server.pooled_client(Limits::default())?;
        let (first, second, third) = tokio::join!(
            exchange(&client, &server.origin, "/ok"),
            exchange(&client, &server.origin, "/ok"),
            exchange(&client, &server.origin, "/ok"),
        );
        for connection in [first?, second?, third?] {
            assert_eq!(connection, ConnectionUse::Fresh);
        }
        assert_eq!(server.connections(), 3);
        assert_eq!(pool(&client)?.idle_count(), MAX_IDLE_PER_KEY);
        // The connection evicted beyond the cap is closed, not leaked.
        assert!(eventually(|| server.closed() == 1).await);
    }
    Ok(())
}

#[tokio::test]
async fn a_connection_idle_for_the_bound_is_closed_instead_of_reused() -> TestResult {
    assert_eq!(IDLE_BOUND, Duration::from_secs(5));
    for alpn in [H1, H2] {
        let server = Server::start(alpn).await?;
        let client = server.pooled_client(Limits::default())?;
        assert_eq!(
            exchange(&client, &server.origin, "/ok").await?,
            ConnectionUse::Fresh
        );
        pool(&client)?.age(IDLE_BOUND.saturating_sub(Duration::from_secs(1)));
        assert_eq!(
            exchange(&client, &server.origin, "/ok").await?,
            ConnectionUse::Reused
        );
        pool(&client)?.age(IDLE_BOUND);
        assert_eq!(
            exchange(&client, &server.origin, "/ok").await?,
            ConnectionUse::Fresh
        );
        assert_eq!(server.connections(), 2);
        assert!(eventually(|| server.closed() == 1).await);
    }
    Ok(())
}

/// Every way an exchange can end short of a clean, fully read body closes
/// its connection, so the next exchange opens a fresh one.
#[tokio::test]
async fn a_connection_is_never_kept_after_an_unclean_end() -> TestResult {
    for alpn in [H1, H2] {
        let server = Server::start(alpn).await?;
        let client = server.pooled_client(Limits::default())?;
        let limits = client.limits().clone();
        let origin = &server.origin;
        let big = || request(origin, "/big", Bytes::from_static(b"{}"), &limits);

        // Dropped after one chunk.
        let mut body = client.execute(big()?, control()).await?.into_body();
        assert!(matches!(body.next_chunk().await, Some(Ok(_))));
        drop(body);
        assert_eq!(pool(&client)?.idle_count(), 0);

        // Closed before the end.
        let body = client.execute(big()?, control()).await?.into_body();
        body.close().await?;
        assert_eq!(pool(&client)?.idle_count(), 0);

        // Cancelled mid-body.
        let cancel = CancellationToken::new();
        let cancellable = ExchangeControl::with_cancellation(
            Instant::now() + Duration::from_secs(10),
            cancel.clone(),
        );
        let mut body = client.execute(big()?, cancellable).await?.into_body();
        assert!(matches!(body.next_chunk().await, Some(Ok(_))));
        cancel.cancel();
        assert!(matches!(
            body.next_chunk().await,
            Some(Err(failure)) if failure.kind() == ErrorKind::Cancelled
                && failure.connection_use() == Some(ConnectionUse::Fresh)
        ));
        assert_eq!(pool(&client)?.idle_count(), 0);

        // Failed on a body limit.
        let small = client.with_limits(Limits {
            max_response_body_bytes: 1_024,
            ..Limits::default()
        })?;
        // A declared length over the limit fails at the head, a streamed one
        // in the body; neither keeps the connection.
        let failure = match small.execute(big()?, control()).await {
            Err(failure) => Some(failure),
            Ok(response) => {
                let mut body = response.into_body();
                let mut failure = None;
                while let Some(chunk) = body.next_chunk().await {
                    if let Err(error) = chunk {
                        failure = Some(error);
                    }
                }
                failure
            }
        };
        assert!(matches!(
            failure,
            Some(failure) if matches!(failure.kind(), ErrorKind::LimitExceeded(_))
        ));
        assert_eq!(pool(&client)?.idle_count(), 0);

        assert_eq!(server.connections(), 4);
        assert_eq!(
            exchange(&client, origin, "/ok").await?,
            ConnectionUse::Fresh
        );
        assert_eq!(server.connections(), 5);
        assert!(eventually(|| server.closed() == 4).await);
    }
    Ok(())
}

#[cfg(feature = "fault-injection")]
#[tokio::test]
async fn a_fault_injected_exchange_never_keeps_its_connection() -> TestResult {
    let server = Server::start(H1).await?;
    let client = server.pooled_client(Limits::default())?;
    let faulted = client
        .clone()
        .with_faults(Some(crate::FaultPlan::parse("fault=eof,after-bytes=1")?));
    assert_eq!(
        exchange(&faulted, &server.origin, "/ok").await?,
        ConnectionUse::Fresh
    );
    assert_eq!(pool(&client)?.idle_count(), 0);
    let clean = client.with_faults(None);
    assert_eq!(
        exchange(&clean, &server.origin, "/ok").await?,
        ConnectionUse::Fresh
    );
    assert_eq!(
        exchange(&clean, &server.origin, "/ok").await?,
        ConnectionUse::Reused
    );
    assert_eq!(server.connections(), 2);
    Ok(())
}

#[tokio::test]
async fn a_response_asking_to_close_is_not_kept() -> TestResult {
    let server = Server::start(H1).await?;
    let client = server.pooled_client(Limits::default())?;
    assert_eq!(
        exchange(&client, &server.origin, "/close").await?,
        ConnectionUse::Fresh
    );
    assert_eq!(pool(&client)?.idle_count(), 0);
    assert_eq!(
        exchange(&client, &server.origin, "/ok").await?,
        ConnectionUse::Fresh
    );
    assert_eq!(server.connections(), 2);
    Ok(())
}

/// A server closing an idle connection, with a GOAWAY on HTTP/2, takes it out
/// of the pool before the next checkout.
#[tokio::test]
async fn an_idle_connection_the_server_closed_is_not_offered() -> TestResult {
    for alpn in [H1, H2] {
        let server = Server::start(alpn).await?;
        let client = server.pooled_client(Limits::default())?;
        assert_eq!(
            exchange(&client, &server.origin, "/ok").await?,
            ConnectionUse::Fresh
        );
        server.shutdown.send(true)?;
        assert!(eventually(|| server.closed() == 1).await);
        let pool = pool(&client)?;
        assert!(eventually(|| pool.usable_count() == 0).await);
        assert_eq!(
            exchange(&client, &server.origin, "/ok").await?,
            ConnectionUse::Fresh
        );
        assert_eq!(server.connections(), 2);
    }
    Ok(())
}

/// The race the idle check cannot close: a connection the server closed is
/// handed out anyway. It fails before taking a byte, so the request goes once
/// more on a fresh connection.
#[tokio::test]
async fn a_stale_connection_is_replaced_once_by_a_fresh_one() -> TestResult {
    for alpn in [H1, H2] {
        let server = Server::start(alpn).await?;
        let client = server.pooled_client(Limits::default())?;
        assert_eq!(
            exchange(&client, &server.origin, "/ok").await?,
            ConnectionUse::Fresh
        );
        server.shutdown.send(true)?;
        assert!(eventually(|| server.closed() == 1).await);
        let pool = pool(&client)?;
        assert!(eventually(|| pool.usable_count() == 0).await);
        assert_eq!(pool.idle_count(), 1);
        pool.offer_unchecked();
        assert_eq!(
            exchange(&client, &server.origin, "/ok").await?,
            ConnectionUse::ReplacedStale
        );
        assert_eq!(server.connections(), 2);
        assert_eq!(
            exchange(&client, &server.origin, "/ok").await?,
            ConnectionUse::Reused
        );
    }
    Ok(())
}

#[tokio::test]
async fn dropping_the_last_pooled_client_closes_its_idle_connections() -> TestResult {
    for alpn in [H1, H2] {
        let server = Server::start(alpn).await?;
        let client = server.pooled_client(Limits::default())?;
        let derived = client.with_limits(Limits::for_streaming_inference())?;
        assert_eq!(
            exchange(&client, &server.origin, "/ok").await?,
            ConnectionUse::Fresh
        );
        drop(client);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(server.closed(), 0, "a derived client still owns the pool");
        drop(derived);
        assert!(eventually(|| server.closed() == 1).await);
    }
    Ok(())
}

#[tokio::test]
async fn derived_clients_share_the_pool() -> TestResult {
    let server = Server::start(H2).await?;
    let client = server.pooled_client(Limits::default())?;
    let streaming = client.with_limits(Limits::for_streaming_inference())?;
    let observed = client.clone().with_observer(None);
    assert!(streaming.is_pooled() && observed.is_pooled());
    assert_eq!(
        exchange(&client, &server.origin, "/ok").await?,
        ConnectionUse::Fresh
    );
    assert_eq!(
        exchange(&streaming, &server.origin, "/ok").await?,
        ConnectionUse::Reused
    );
    assert_eq!(
        exchange(&observed, &server.origin, "/ok").await?,
        ConnectionUse::Reused
    );
    // Pooling twice starts a separate pool.
    assert_eq!(
        exchange(&client.pooled(), &server.origin, "/ok").await?,
        ConnectionUse::Fresh
    );
    assert_eq!(server.connections(), 2);
    Ok(())
}

#[tokio::test]
async fn a_protocol_terminal_close_keeps_a_connection_whose_body_ended() -> TestResult {
    for alpn in [H1, H2] {
        let server = Server::start(alpn).await?;
        let client = server.pooled_client(Limits::default())?;
        let limits = client.limits().clone();
        let response = client
            .execute(
                request(&server.origin, "/ok", Bytes::from_static(b"{}"), &limits)?,
                control(),
            )
            .await?;
        let mut body = response.into_body();
        assert!(matches!(body.next_chunk().await, Some(Ok(chunk)) if chunk.as_ref() == b"ok"));
        body.close_after_protocol_terminal().await?;
        assert_eq!(pool(&client)?.idle_count(), 1);
        assert_eq!(
            exchange(&client, &server.origin, "/ok").await?,
            ConnectionUse::Reused
        );
        assert_eq!(server.connections(), 1);
    }
    Ok(())
}

#[test]
fn pooling_is_part_of_the_transport_behavior_identity() -> TestResult {
    let client = Client::new(Limits::default())?;
    let pooled = client.pooled();
    assert!(pooled.behavior_identity().is_some());
    assert_ne!(pooled.behavior_identity(), client.behavior_identity());
    assert_eq!(
        pooled.with_limits(Limits::default())?.behavior_identity(),
        pooled.behavior_identity()
    );
    assert_eq!(
        client.pooled().behavior_identity(),
        pooled.behavior_identity()
    );
    Ok(())
}

/// What a raw HTTP/1.1 peer does with each request it reads.
#[derive(Clone, Copy)]
enum Action {
    Respond,
    /// Reads the request head and closes with the body unread.
    CloseAfterHead,
    /// Reads the whole request and closes without answering.
    CloseAfterRequest,
    /// Answers 429 after the request head and holds the connection open
    /// without reading the body.
    RespondBeforeBody,
    /// Answers with a chunked body that never sends its terminal frame.
    EndPending,
}

/// A raw HTTP/1.1 peer serving one connection, acting on its requests in turn.
async fn raw_peer(actions: Vec<Action>) -> TestResult<(Client, Origin, Arc<AtomicUsize>)> {
    let Endpoint {
        listener,
        acceptor,
        client,
        origin,
    } = endpoint(H1).await?;
    let connections = Arc::new(AtomicUsize::new(0));
    let accepted = Arc::clone(&connections);
    tokio::spawn(async move {
        let mut actions = actions.into_iter();
        while let Ok((stream, _)) = listener.accept().await {
            accepted.fetch_add(1, Ordering::SeqCst);
            let Ok(mut stream) = acceptor.accept(stream).await else {
                continue;
            };
            for action in actions.by_ref() {
                let Ok(content_length) = read_head(&mut stream).await else {
                    break;
                };
                match action {
                    Action::CloseAfterHead => break,
                    Action::RespondBeforeBody => {
                        let early =
                            b"HTTP/1.1 429 Too Many Requests\r\ncontent-length: 2\r\n\r\nno";
                        let _ = stream.write_all(early).await;
                        // Hold the connection without reading the body.
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        break;
                    }
                    Action::Respond | Action::CloseAfterRequest | Action::EndPending => {}
                }
                if read_body(&mut stream, content_length).await.is_err() {
                    break;
                }
                let response: &[u8] = match action {
                    Action::CloseAfterRequest => break,
                    Action::EndPending => {
                        b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n2\r\nok\r\n"
                    }
                    Action::Respond | Action::CloseAfterHead | Action::RespondBeforeBody => {
                        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok"
                    }
                };
                if stream.write_all(response).await.is_err() {
                    break;
                }
                if matches!(action, Action::EndPending) {
                    let mut byte = [0_u8; 1];
                    let _ = stream.read(&mut byte).await;
                    break;
                }
            }
        }
    });
    let limits = Limits {
        max_request_body_bytes: 64 * 1_024 * 1_024,
        ..Limits::default()
    };
    let client = Client::with_tls_config(limits, client)?.pooled();
    Ok((client, origin, connections))
}

/// Reads one request head byte by byte, so nothing past it is consumed.
async fn read_head(stream: &mut TlsStream<TcpStream>) -> io::Result<usize> {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        head.push(stream.read_u8().await?);
        if head.len() > 64 * 1_024 {
            return Err(io::Error::other("request head exceeded bound"));
        }
    }
    let head = String::from_utf8_lossy(&head);
    Ok(head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())
                .flatten()
        })
        .unwrap_or(0))
}

async fn read_body(stream: &mut TlsStream<TcpStream>, length: usize) -> io::Result<()> {
    let mut buffer = vec![0_u8; 64 * 1_024];
    let mut remaining = length;
    while remaining > 0 {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        remaining = remaining.saturating_sub(read);
    }
    Ok(())
}

async fn send_record_alert(stream: TlsStream<TcpStream>, bad_mac: bool) -> TestResult {
    let (mut tcp, mut tls) = stream.into_inner();
    let alert = record_alert(&mut tls, bad_mac)?;
    tcp.write_all(&alert).await?;
    // Drain unread upload records so closing the socket cannot reset away
    // the alert before the client receives it.
    let mut buffer = vec![0_u8; 16 * 1_024];
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        while tcp.read(&mut buffer).await.is_ok_and(|read| read > 0) {}
    })
    .await;
    Ok(())
}

fn record_alert(tls: &mut rustls::ServerConnection, bad_mac: bool) -> TestResult<Vec<u8>> {
    // Make Rustls emit an authenticated fatal alert, not a corrupt server
    // record (which would test a local DecryptError rather than AlertReceived).
    let record = if bad_mac {
        let mut record = vec![0x17, 0x03, 0x03, 0x00, 0x20];
        // A partial upload may leave an unfinished client record in Rustls.
        // Enough corrupt bytes complete that record and trigger alert 20 too.
        record.resize(64 * 1_024, 0x5a);
        record
    } else {
        vec![0xff, 0x03, 0x03, 0x00, 0x01, 0x00]
    };
    let mut input = io::Cursor::new(record);
    let error = loop {
        if tls.read_tls(&mut input)? == 0 {
            return Err("fixture did not reject the record".into());
        }
        if let Err(error) = tls.process_new_packets() {
            break error;
        }
    };
    if bad_mac {
        assert_eq!(error, rustls::Error::DecryptError);
    }
    let mut alert = Vec::new();
    while tls.wants_write() {
        tls.write_tls(&mut alert)?;
    }
    Ok(alert)
}

#[tokio::test]
async fn response_byte_guard_covers_delivered_and_buffered_plaintext_deterministically()
-> TestResult {
    use super::super::{ProgressSlot, RequestIo, RequestProgress, WireStream};
    for buffered in [false, true] {
        let Endpoint {
            listener,
            acceptor,
            client,
            ..
        } = endpoint_with_versions(H1, &[&rustls::version::TLS13]).await?;
        let address = listener.local_addr()?;
        let head = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n";
        let (size_tx, size_rx) = tokio::sync::oneshot::channel();
        let (read_tx, read_rx) = tokio::sync::oneshot::channel();
        let peer = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await?;
            let mut stream = acceptor.accept(tcp).await?;
            if !buffered {
                stream.write_all(head).await?;
                stream.flush().await?;
                read_rx.await?;
            }
            let (mut tcp, mut tls) = stream.into_inner();
            if buffered {
                io::Write::write_all(&mut tls.writer(), head)?;
            }
            let ciphertext = record_alert(&mut tls, true)?;
            size_tx
                .send(ciphertext.len())
                .map_err(|_| "size receiver closed")?;
            // Application data and fatal alert are one ciphertext write.
            tcp.write_all(&ciphertext).await?;
            Ok::<_, Box<dyn Error + Send + Sync>>(())
        });
        let tcp = TcpStream::connect(address).await?;
        let name = rustls::pki_types::ServerName::try_from("127.0.0.1")?.to_owned();
        let stream = tokio_rustls::TlsConnector::from(Arc::new(client))
            .connect(name, tcp)
            .await?;
        let progress = Arc::new(RequestProgress::new(true, false, 0));
        let mut io = RequestIo {
            stream: WireStream::Tls(Box::new(stream)),
            slot: ProgressSlot::new(Arc::clone(&progress)),
        };
        if buffered {
            // Process the whole ciphertext batch at once, independent of TCP
            // segmentation, before Hyper can consume the buffered plaintext.
            let mut wire = vec![0; size_rx.await?];
            let WireStream::Tls(stream) = &mut io.stream else {
                return Err("the test connection is TLS".into());
            };
            let (tcp, tls) = stream.get_mut();
            tcp.read_exact(&mut wire).await?;
            let mut wire = io::Cursor::new(wire);
            while wire.position() < u64::try_from(wire.get_ref().len())? {
                tls.read_tls(&mut wire)?;
            }
            let error = tls.process_new_packets().err().ok_or("missing alert")?;
            assert_eq!(
                error,
                rustls::Error::AlertReceived(rustls::AlertDescription::BadRecordMac)
            );
            assert!(!progress.response_started.load(Ordering::Acquire));
            io.record_io_error(&io::Error::new(io::ErrorKind::InvalidData, error));
        } else {
            let mut received = vec![0; head.len()];
            io.read_exact(&mut received).await?;
            assert_eq!(received, head);
            assert!(progress.response_started.load(Ordering::Acquire));
            read_tx.send(()).map_err(|()| "peer closed")?;
            let _ = size_rx.await?;
            let error = io.read_u8().await.err().ok_or("missing alert")?;
            assert!(matches!(
                error
                    .get_ref()
                    .and_then(|e| e.downcast_ref::<rustls::Error>()),
                Some(rustls::Error::AlertReceived(
                    rustls::AlertDescription::BadRecordMac
                ))
            ));
        }
        assert!(progress.response_started.load(Ordering::Acquire));
        assert!(!progress.peer_rejected_record.load(Ordering::Acquire));
        peer.await??;
    }
    Ok(())
}

#[tokio::test]
async fn bad_record_mac_on_tls12_after_full_upload_is_not_proven_unsent() -> TestResult {
    let Endpoint {
        listener,
        acceptor,
        client,
        origin,
    } = endpoint_with_versions(H1, &[&rustls::version::TLS12]).await?;
    let peer = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await?;
        let mut stream = acceptor.accept(tcp).await?;
        assert_eq!(
            stream.get_ref().1.protocol_version(),
            Some(rustls::ProtocolVersion::TLSv1_2)
        );
        let length = read_head(&mut stream).await?;
        read_body(&mut stream, length).await?;
        send_record_alert(stream, true).await
    });
    let client = Client::with_tls_config(Limits::default(), client)?;
    let failure = failed_exchange(&client, &origin, Bytes::from_static(b"{}")).await?;
    assert_eq!(failure.transmission(), TransmissionFact::MayHaveTransmitted);
    assert_ne!(failure.cause(), Some("peer_rejected_record"));
    assert_eq!(failure.tls_cause(), Some(TlsCause::AlertReceived(20)));
    peer.await??;
    Ok(())
}

#[tokio::test]
async fn bad_record_mac_before_response_is_incomplete_even_after_full_upload() -> TestResult {
    for mid_body in [true, false] {
        let Endpoint {
            listener,
            acceptor,
            client,
            origin,
        } = endpoint(H1).await?;
        let peer = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await?;
            let mut stream = acceptor.accept(tcp).await?;
            let length = read_head(&mut stream).await?;
            read_body(&mut stream, if mid_body { 1_024 } else { length }).await?;
            send_record_alert(stream, true).await
        });
        let client = Client::with_tls_config(Limits::default(), client)?.pooled();
        let failure =
            failed_exchange(&client, &origin, Bytes::from(vec![b'x'; 512 * 1_024])).await?;
        assert_eq!(failure.transmission(), TransmissionFact::RequestIncomplete);
        assert!(failure.transmission().peer_cannot_have_acted());
        assert_eq!(failure.cause(), Some("peer_rejected_record"));
        assert_eq!(failure.tls_cause(), Some(TlsCause::AlertReceived(20)));
        assert_eq!(failure.wire_protocol(), Some(WireProtocol::Http1));
        peer.await??;
    }
    Ok(())
}

#[tokio::test]
async fn bad_record_mac_mid_upload_drops_fresh_and_reused_connections() -> TestResult {
    for reused in [false, true] {
        let Endpoint {
            listener,
            acceptor,
            client,
            origin,
        } = endpoint(H1).await?;
        let peer = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await?;
            let mut stream = acceptor.accept(tcp).await?;
            if reused {
                let length = read_head(&mut stream).await?;
                read_body(&mut stream, length).await?;
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
                    .await?;
                stream.flush().await?;
            }
            let _length = read_head(&mut stream).await?;
            read_body(&mut stream, 1_024).await?;
            send_record_alert(stream, true).await?;
            let (tcp, _) = listener.accept().await?;
            let mut stream = acceptor.accept(tcp).await?;
            let length = read_head(&mut stream).await?;
            read_body(&mut stream, length).await?;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
                .await?;
            stream.flush().await?;
            // Keep the clean connection alive until the test sees check-in.
            let mut byte = [0];
            let _ = stream.read(&mut byte).await;
            Ok::<_, Box<dyn Error + Send + Sync>>(())
        });
        let client = Client::with_tls_config(Limits::default(), client)?.pooled();
        if reused {
            assert_eq!(
                exchange(&client, &origin, "/warm").await?,
                ConnectionUse::Fresh
            );
            assert_eq!(pool(&client)?.idle_count(), 1);
        }
        let failure =
            failed_exchange(&client, &origin, Bytes::from(vec![b'x'; 512 * 1_024])).await?;
        assert_eq!(failure.cause(), Some("peer_rejected_record"));
        assert_eq!(failure.transmission(), TransmissionFact::RequestIncomplete);
        assert_eq!(
            failure.connection_use(),
            Some(if reused {
                ConnectionUse::Reused
            } else {
                ConnectionUse::Fresh
            })
        );
        assert_eq!(pool(&client)?.idle_count(), 0);
        assert_eq!(
            exchange(&client, &origin, "/resend").await?,
            ConnectionUse::Fresh
        );
        drop(client);
        peer.await??;
    }
    Ok(())
}

#[tokio::test]
async fn bad_record_mac_after_any_response_byte_is_not_unsent() -> TestResult {
    for head in [
        b"H".as_slice(),
        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n".as_slice(),
    ] {
        let Endpoint {
            listener,
            acceptor,
            client,
            origin,
        } = endpoint(H1).await?;
        let peer = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await?;
            let mut stream = acceptor.accept(tcp).await?;
            let length = read_head(&mut stream).await?;
            read_body(&mut stream, length).await?;
            stream.write_all(head).await?;
            stream.flush().await?;
            send_record_alert(stream, true).await
        });
        let client = Client::with_tls_config(Limits::default(), client)?;
        let result = client
            .execute(
                request(&origin, "/", Bytes::from_static(b"{}"), client.limits())?,
                control(),
            )
            .await;
        let failure = match result {
            Err(failure) => failure,
            Ok(response) => response
                .into_body()
                .next_chunk()
                .await
                .ok_or("missing body failure")?
                .err()
                .ok_or("alert after head succeeded")?,
        };
        assert_eq!(failure.transmission(), TransmissionFact::MayHaveTransmitted);
        assert_ne!(failure.cause(), Some("peer_rejected_record"));
        assert_eq!(failure.tls_cause(), Some(TlsCause::AlertReceived(20)));
        peer.await??;
    }
    Ok(())
}

#[tokio::test]
async fn another_tls_alert_before_response_keeps_existing_classification() -> TestResult {
    let Endpoint {
        listener,
        acceptor,
        client,
        origin,
    } = endpoint(H1).await?;
    let peer = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await?;
        let mut stream = acceptor.accept(tcp).await?;
        let length = read_head(&mut stream).await?;
        read_body(&mut stream, length).await?;
        send_record_alert(stream, false).await
    });
    let client = Client::with_tls_config(Limits::default(), client)?;
    let failure = failed_exchange(&client, &origin, Bytes::from_static(b"{}")).await?;
    assert_eq!(failure.transmission(), TransmissionFact::MayHaveTransmitted);
    assert_eq!(failure.cause(), Some("invalid_data"));
    assert_eq!(failure.tls_cause(), Some(TlsCause::AlertReceived(50)));
    peer.await??;
    Ok(())
}

#[tokio::test]
async fn bad_record_mac_on_http2_keeps_existing_classification() -> TestResult {
    let Endpoint {
        listener,
        acceptor,
        client,
        origin,
    } = endpoint(H2).await?;
    let peer = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await?;
        let mut stream = acceptor.accept(tcp).await?;
        let mut preface = [0_u8; 24];
        stream.read_exact(&mut preface).await?;
        assert_eq!(&preface, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
        stream.write_all(&[0, 0, 0, 4, 0, 0, 0, 0, 0]).await?;
        stream.flush().await?;
        loop {
            let mut head = [0_u8; 9];
            stream.read_exact(&mut head).await?;
            let length =
                usize::from(head[0]) << 16 | usize::from(head[1]) << 8 | usize::from(head[2]);
            let mut payload = vec![0_u8; length];
            stream.read_exact(&mut payload).await?;
            if matches!(head[3], 0 | 1) && head[4] & 1 != 0 {
                break;
            }
        }
        send_record_alert(stream, true).await
    });
    let client = Client::with_tls_config(Limits::default(), client)?;
    let failure = failed_exchange(&client, &origin, Bytes::from_static(b"{}")).await?;
    assert_eq!(failure.transmission(), TransmissionFact::MayHaveTransmitted);
    assert_eq!(failure.cause(), Some("invalid_data"));
    assert_eq!(failure.wire_protocol(), Some(WireProtocol::Http2));
    assert_ne!(failure.cause(), Some("peer_rejected_record"));
    peer.await??;
    Ok(())
}

async fn failed_exchange(client: &Client, origin: &Origin, body: Bytes) -> TestResult<Failure> {
    let request = request(origin, "/turn", body, client.limits())?;
    match client.execute(request, control()).await {
        Ok(_) => Err("the exchange succeeded".into()),
        Err(failure) => Ok(failure),
    }
}

/// Once a reused connection took part of the request, its death is reported
/// with what the peer may have seen and is never retried.
#[tokio::test]
async fn a_reused_connection_dying_mid_request_is_not_retried() -> TestResult {
    let (client, origin, connections) =
        raw_peer(vec![Action::Respond, Action::CloseAfterHead]).await?;
    assert_eq!(
        exchange(&client, &origin, "/ok").await?,
        ConnectionUse::Fresh
    );
    let failure = failed_exchange(
        &client,
        &origin,
        Bytes::from(vec![b'x'; 32 * 1_024 * 1_024]),
    )
    .await?;
    assert_eq!(failure.connection_use(), Some(ConnectionUse::Reused));
    assert!(failure.idle_age().is_some());
    assert!(format!("{failure:?}").contains("idle_age"));
    assert!(failure.to_string().contains("idle at checkout"));
    assert_eq!(failure.transmission(), TransmissionFact::RequestIncomplete);
    assert_eq!(failure.phase(), Phase::RequestWriteOrResponseHead);
    assert_eq!(failure.kind(), ErrorKind::Protocol);
    assert_eq!(failure.wire_protocol(), Some(WireProtocol::Http1));
    assert_eq!(connections.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn a_reused_connection_dying_after_the_request_is_not_retried() -> TestResult {
    let (client, origin, connections) =
        raw_peer(vec![Action::Respond, Action::CloseAfterRequest]).await?;
    assert_eq!(
        exchange(&client, &origin, "/ok").await?,
        ConnectionUse::Fresh
    );
    let failure = failed_exchange(&client, &origin, Bytes::from_static(b"{}")).await?;
    assert_eq!(failure.connection_use(), Some(ConnectionUse::Reused));
    assert!(failure.idle_age().is_some());
    assert!(format!("{failure:?}").contains("idle_age"));
    assert!(failure.to_string().contains("idle at checkout"));
    assert_eq!(failure.transmission(), TransmissionFact::MayHaveTransmitted);
    assert_eq!(connections.load(Ordering::SeqCst), 1);
    Ok(())
}

/// The one replacement for a stale connection is never itself replaced.
#[tokio::test]
async fn a_failed_replacement_reports_it_and_is_not_retried_again() -> TestResult {
    for alpn in [H1, H2] {
        let server = Server::start(alpn).await?;
        let client = server.pooled_client(Limits::default())?;
        assert_eq!(
            exchange(&client, &server.origin, "/ok").await?,
            ConnectionUse::Fresh
        );
        server.shutdown.send(true)?;
        assert!(eventually(|| server.closed() == 1).await);
        let pool = pool(&client)?;
        assert!(eventually(|| pool.usable_count() == 0).await);
        pool.offer_unchecked();
        server.refuse.store(true, Ordering::SeqCst);
        let failure = failed_exchange(&client, &server.origin, Bytes::from_static(b"{}")).await?;
        assert_eq!(failure.connection_use(), Some(ConnectionUse::ReplacedStale));
        assert_eq!(failure.transmission(), TransmissionFact::NotStarted);
        assert_eq!(server.connections(), 2);
        assert_eq!(pool.idle_count(), 0);
    }
    Ok(())
}

/// A response that arrives while the request body is still going out ends
/// its exchange at once and never pools the connection.
#[tokio::test]
async fn an_early_response_to_an_unfinished_upload_is_not_kept() -> TestResult {
    let (client, origin, connections) = raw_peer(vec![Action::RespondBeforeBody]).await?;
    let request = request(
        &origin,
        "/upload",
        Bytes::from(vec![b'x'; 32 * 1_024 * 1_024]),
        client.limits(),
    )?;
    let response = client.execute(request, control()).await?;
    assert_eq!(response.status().as_u16(), 429);
    let mut body = response.into_body();
    assert!(matches!(body.next_chunk().await, Some(Ok(chunk)) if chunk.as_ref() == b"no"));
    let started = Instant::now();
    assert!(body.next_chunk().await.is_none());
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "the body end waited {:?}",
        started.elapsed()
    );
    assert_eq!(pool(&client)?.idle_count(), 0);
    assert_eq!(connections.load(Ordering::SeqCst), 1);
    Ok(())
}

/// Closing at a protocol terminal never waits for a body end still in flight.
#[tokio::test]
async fn a_protocol_terminal_close_before_the_body_end_closes_at_once() -> TestResult {
    let (client, origin, _connections) = raw_peer(vec![Action::EndPending]).await?;
    let request = request(&origin, "/turn", Bytes::from_static(b"{}"), client.limits())?;
    let mut body = client.execute(request, control()).await?.into_body();
    assert!(matches!(body.next_chunk().await, Some(Ok(chunk)) if chunk.as_ref() == b"ok"));
    // The peer cannot finish the body. Paused time catches any timed wait in
    // close without charging scheduler latency for the aborted task's join.
    tokio::time::pause();
    let started = tokio::time::Instant::now();
    body.close_after_protocol_terminal().await?;
    assert_eq!(started.elapsed(), Duration::ZERO);
    tokio::time::resume();
    assert_eq!(pool(&client)?.idle_count(), 0);
    Ok(())
}
