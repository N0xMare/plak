//! Deterministic TLS mock-server tests for the physical HTTP transport.

use std::error::Error;
use std::future::Future as _;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use http_body_util::Full;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use plakous_http::{
    AddressPolicy, Bytes, CancellationToken, Client, ErrorKind, ExchangeControl, HeaderMap,
    HeaderValue, LimitKind, Limits, Method, Origin, Phase, Request, SensitiveBytes, StatusCode,
    TlsCause, TransmissionFact, Version, WireProtocol, sensitive_header_value,
};
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

type TestError = Box<dyn Error + Send + Sync>;
type TestResult<T = ()> = Result<T, TestError>;

#[tokio::test]
async fn reachability_probe_preserves_connect_io_cause() -> TestResult {
    // A port whose listener was closed refuses immediately on Linux and macOS;
    // a bound socket that never listens is not refused on macOS (the SYN waits).
    let closed = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?;
    let origin = Origin::parse(&format!("https://{closed}"))?;
    let client = Client::new(Limits::default())?;
    let failure = client
        .probe(
            &origin,
            ExchangeControl::new(Instant::now() + Duration::from_secs(5)),
        )
        .await
        .err()
        .ok_or("non-listening socket must refuse the TCP connection")?;
    assert_eq!(failure.phase(), Phase::Connect);
    assert_eq!(failure.transmission(), TransmissionFact::NotStarted);
    assert!(failure.os_error().is_some());
    assert!(failure.cause().is_some());
    Ok(())
}

#[tokio::test]
async fn reachability_probe_sends_zero_http_bytes() -> TestResult {
    for alpn in [b"http/1.1".as_slice(), b"h2".as_slice()] {
        let Endpoint {
            listener,
            acceptor,
            client_config,
            origin,
        } = make_endpoint(&[alpn]).await?;
        let mut server_config = acceptor.config().as_ref().clone();
        // The probe closes as soon as its handshake succeeds; this peer must
        // not need to flush post-handshake tickets before checking HTTP bytes.
        server_config.send_tls13_tickets = 0;
        let acceptor = TlsAcceptor::from(Arc::new(server_config));
        let client = make_client(client_config, Limits::default())?.pooled();
        let (signals, completions): (Vec<_>, Vec<_>) = (0..2).map(|_| oneshot::channel()).unzip();
        let peer = tokio::spawn(async move {
            let mut total = 0;
            for done in completions {
                let (stream, _) = listener.accept().await?;
                let mut stream = acceptor
                    .accept(ProbePeerIo {
                        stream,
                        done,
                        sent_flight: false,
                        probe_dropped: false,
                    })
                    .await?;
                let mut bytes = [0; 512];
                let count = match stream.read(&mut bytes).await {
                    Ok(count) => count,
                    Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => 0,
                    Err(error) => return Err(error.into()),
                };
                total += count;
            }
            Ok::<_, TestError>(total)
        });
        for done in signals {
            client
                .probe(
                    &origin,
                    ExchangeControl::new(Instant::now() + Duration::from_secs(5)),
                )
                .await?;
            done.send(()).map_err(|()| "probe peer stopped")?;
        }
        assert_eq!(
            peer.await??,
            0,
            "probe must send no HTTP/1 head or HTTP/2 preface"
        );
    }
    Ok(())
}

// Delay the client's Finished until the probe has dropped its connection.
// A peer cannot rely on writing TLS 1.3 tickets after that teardown.
struct ProbePeerIo {
    stream: TcpStream,
    done: oneshot::Receiver<()>,
    sent_flight: bool,
    probe_dropped: bool,
}

impl AsyncRead for ProbePeerIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.sent_flight && !self.probe_dropped {
            match Pin::new(&mut self.done).poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(())) => self.probe_dropped = true,
                Poll::Ready(Err(_)) => {
                    return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
                }
            }
        }
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for ProbePeerIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.probe_dropped {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let flushed = Pin::new(&mut self.stream).poll_flush(cx);
        if matches!(flushed, Poll::Ready(Ok(()))) {
            self.sent_flight = true;
        }
        flushed
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

#[tokio::test]
async fn reachability_probe_rejects_untrusted_tls_and_honors_cancellation() -> TestResult {
    let Endpoint {
        listener,
        acceptor,
        origin,
        ..
    } = make_endpoint(&[b"http/1.1"]).await?;
    let client = Client::new(Limits::default())?;
    let peer = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        assert!(acceptor.accept(stream).await.is_err());
        Ok::<_, TestError>(())
    });
    let failure = client
        .probe(
            &origin,
            ExchangeControl::new(Instant::now() + Duration::from_secs(5)),
        )
        .await
        .err()
        .ok_or("probe admitted an untrusted certificate")?;
    assert_eq!(failure.phase(), Phase::Tls);
    assert_eq!(failure.transmission(), TransmissionFact::NotStarted);
    peer.await??;
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let failure = client
        .probe(
            &origin,
            ExchangeControl::with_cancellation(
                Instant::now() + Duration::from_secs(5),
                cancellation,
            ),
        )
        .await
        .err()
        .ok_or("probe ignored cancellation")?;
    assert_eq!(failure.kind(), ErrorKind::Cancelled);
    assert_eq!(failure.transmission(), TransmissionFact::NotStarted);
    Ok(())
}

struct Endpoint {
    listener: TcpListener,
    acceptor: TlsAcceptor,
    client_config: ClientConfig,
    origin: Origin,
}

async fn make_endpoint(alpn: &[&[u8]]) -> TestResult<Endpoint> {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])?;
    let certificate = cert.der().clone();
    let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut server_config = ServerConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(vec![certificate.clone()], private_key)?;
    server_config.alpn_protocols = alpn.iter().map(|value| value.to_vec()).collect();

    let mut roots = RootCertStore::empty();
    roots.add(CertificateDer::from(certificate.to_vec()))?;
    let client_config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let origin = Origin::parse(&format!("https://{address}"))?;
    Ok(Endpoint {
        listener,
        acceptor: TlsAcceptor::from(Arc::new(server_config)),
        client_config,
        origin,
    })
}

fn make_client(config: ClientConfig, limits: Limits) -> TestResult<Client> {
    Ok(Client::with_tls_config(limits, config)?)
}

async fn read_request(stream: &mut TlsStream<TcpStream>) -> TestResult<Vec<u8>> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1_024];
    let header_end = loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "request ended before its head",
            )
            .into());
        }
        request.extend_from_slice(&buffer[..read]);
        if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
        if request.len() > 128 * 1_024 {
            return Err(io::Error::other("test request head exceeded bound").into());
        }
    };
    let head = std::str::from_utf8(&request[..header_end])?;
    let content_length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    let total = header_end.saturating_add(content_length);
    while request.len() < total {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "request ended before its body",
            )
            .into());
        }
        request.extend_from_slice(&buffer[..read]);
    }
    Ok(request)
}

async fn responding_server(
    response: Vec<u8>,
) -> TestResult<(
    ClientConfig,
    Origin,
    JoinHandle<TestResult<(Vec<u8>, usize)>>,
)> {
    let endpoint = make_endpoint(&[b"http/1.1"]).await?;
    let Endpoint {
        listener,
        acceptor,
        client_config,
        origin,
    } = endpoint;
    let handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut stream = acceptor.accept(stream).await?;
        let request = read_request(&mut stream).await?;
        stream.write_all(&response).await?;
        stream.shutdown().await?;
        let exchanges = if timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_ok()
        {
            2
        } else {
            1
        };
        Ok((request, exchanges))
    });
    Ok((client_config, origin, handle))
}

fn make_request(
    origin: Origin,
    method: Method,
    target: &str,
    headers: HeaderMap,
    body: Bytes,
    limits: &Limits,
) -> TestResult<Request> {
    Ok(Request::new(origin, method, target, headers, body, limits)?)
}

fn control() -> ExchangeControl {
    ExchangeControl::new(Instant::now() + Duration::from_secs(10))
}

async fn required_chunk(body: &mut plakous_http::ResponseBody) -> TestResult<Bytes> {
    body.next_chunk()
        .await
        .transpose()?
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "missing body chunk").into())
}

#[tokio::test]
async fn serializes_one_origin_bound_http1_request_and_redacts_debug() -> TestResult {
    let response =
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nX-Secret: response-secret\r\n\r\nok".to_vec();
    let (config, origin, server) = responding_server(response).await?;
    let limits = Limits::default();
    let client = make_client(config, limits.clone())?;
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::AUTHORIZATION,
        HeaderValue::from_static("Bearer request-secret"),
    );
    headers.insert("x-test", HeaderValue::from_static("present"));
    let request = make_request(
        origin,
        Method::POST,
        "/v1/items?token=query-secret",
        headers,
        Bytes::from_static(b"payload"),
        &limits,
    )?;
    let request_debug = format!("{request:?}");
    assert!(!request_debug.contains("request-secret"));
    assert!(!request_debug.contains("query-secret"));

    let response = client.execute(request, control()).await?;
    assert_eq!(response.status(), StatusCode::OK);
    let response_debug = format!("{response:?}");
    assert!(!response_debug.contains("response-secret"));
    let mut body = response.into_body();
    assert_eq!(required_chunk(&mut body).await?, "ok");
    assert!(body.next_chunk().await.is_none());

    let (captured, exchanges) = server.await??;
    let captured = String::from_utf8(captured)?;
    assert!(captured.starts_with("POST /v1/items?token=query-secret HTTP/1.1\r\n"));
    assert!(captured.to_ascii_lowercase().contains("host: 127.0.0.1:"));
    assert!(captured.contains("authorization: Bearer request-secret\r\n"));
    assert!(captured.to_ascii_lowercase().contains("content-length: 7"));
    assert!(captured.ends_with("\r\npayload"));
    assert_eq!(exchanges, 1);
    Ok(())
}

#[tokio::test]
async fn sensitive_backing_preserves_http1_bounds_and_framing() -> TestResult {
    let response = b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n".to_vec();
    let (config, origin, server) = responding_server(response).await?;
    let limits = Limits::default();
    let client = make_client(config, limits.clone())?;
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::AUTHORIZATION,
        sensitive_header_value(b"Bearer sensitive-header".to_vec())?,
    );
    let request = make_request(
        origin,
        Method::POST,
        "/sensitive",
        headers,
        SensitiveBytes::new(b"sensitive-body".to_vec()).into_bytes(),
        &limits,
    )?;

    let response = client.execute(request, control()).await?;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(response.into_body().next_chunk().await.is_none());

    let (captured, exchanges) = server.await??;
    let captured = String::from_utf8(captured)?;
    assert!(captured.contains("authorization: Bearer sensitive-header\r\n"));
    assert!(captured.to_ascii_lowercase().contains("content-length: 14"));
    assert!(captured.ends_with("\r\nsensitive-body"));
    assert_eq!(exchanges, 1);
    Ok(())
}

#[tokio::test]
async fn returns_redirect_without_following_or_forwarding_authorization() -> TestResult {
    let target = TcpListener::bind("127.0.0.1:0").await?;
    let location = format!("https://{}/redirected", target.local_addr()?);
    let response =
        format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\n\r\n")
            .into_bytes();
    let (config, origin, server) = responding_server(response).await?;
    let limits = Limits::default();
    let client = make_client(config, limits.clone())?;
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::AUTHORIZATION,
        HeaderValue::from_static("Bearer do-not-forward"),
    );
    let request = make_request(
        origin,
        Method::GET,
        "/start",
        headers,
        Bytes::new(),
        &limits,
    )?;
    let response = client.execute(request, control()).await?;
    assert_eq!(response.status(), StatusCode::FOUND);
    response.into_body().close().await?;
    assert!(
        timeout(Duration::from_millis(150), target.accept())
            .await
            .is_err()
    );
    let (captured, exchanges) = server.await??;
    assert!(String::from_utf8(captured)?.contains("Bearer do-not-forward"));
    assert_eq!(exchanges, 1);
    Ok(())
}

#[test]
fn validates_https_origin_controlled_headers_and_request_limits() -> TestResult {
    assert!(Origin::parse("http://example.com").is_err());
    assert!(Origin::parse("https://user@example.com").is_err());
    assert!(Origin::parse("https://example.com/path").is_err());
    let origin = Origin::parse("https://example.com")?;
    let mut limits = Limits {
        max_request_body_bytes: 3,
        ..Limits::default()
    };
    let oversized = Request::new(
        origin.clone(),
        Method::POST,
        "/",
        HeaderMap::new(),
        Bytes::from_static(b"four"),
        &limits,
    );
    assert!(matches!(
        oversized,
        Err(plakous_http::RequestError::LimitExceeded(
            LimitKind::RequestBodyBytes
        ))
    ));
    let mut headers = HeaderMap::new();
    headers.insert(http::header::HOST, HeaderValue::from_static("other.test"));
    assert!(
        Request::new(
            origin.clone(),
            Method::GET,
            "/",
            headers,
            Bytes::new(),
            &limits
        )
        .is_err()
    );
    limits.max_request_headers = 2;
    assert!(
        Request::new(
            origin,
            Method::GET,
            "/",
            HeaderMap::new(),
            Bytes::new(),
            &limits
        )
        .is_ok()
    );
    Ok(())
}

#[tokio::test]
async fn enforces_response_head_body_and_chunk_limits() -> TestResult {
    let mut body_limits = Limits {
        max_response_body_bytes: 5,
        ..Limits::default()
    };
    let (config, origin, server) =
        responding_server(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\n123456".to_vec()).await?;
    let client = make_client(config, body_limits.clone())?;
    let request = make_request(
        origin,
        Method::GET,
        "/",
        HeaderMap::new(),
        Bytes::new(),
        &body_limits,
    )?;
    let failure = client
        .execute(request, control())
        .await
        .err()
        .ok_or_else(|| io::Error::other("oversized declared response unexpectedly succeeded"))?;
    assert_eq!(failure.phase(), Phase::ResponseHead);
    assert_eq!(
        failure.kind(),
        ErrorKind::LimitExceeded(LimitKind::ResponseBodyBytes)
    );
    assert_eq!(server.await??.1, 1);

    body_limits.max_response_body_bytes = 100;
    body_limits.max_response_chunk_bytes = 2;
    let (config, origin, server) = responding_server(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nabcd\r\n0\r\n\r\n".to_vec(),
    )
    .await?;
    let client = make_client(config, body_limits.clone())?;
    let request = make_request(
        origin,
        Method::GET,
        "/",
        HeaderMap::new(),
        Bytes::new(),
        &body_limits,
    )?;
    let mut body = client.execute(request, control()).await?.into_body();
    assert_eq!(required_chunk(&mut body).await?, "ab");
    assert_eq!(required_chunk(&mut body).await?, "cd");
    assert!(body.next_chunk().await.is_none());
    assert_eq!(server.await??.1, 1);

    let long_value = "a".repeat(9_000);
    let response = format!("HTTP/1.1 200 OK\r\nX-Long: {long_value}\r\nContent-Length: 0\r\n\r\n")
        .into_bytes();
    let head_limits = Limits {
        max_response_header_bytes: 8_192,
        ..Limits::default()
    };
    let (config, origin, server) = responding_server(response).await?;
    let client = make_client(config, head_limits.clone())?;
    let request = make_request(
        origin,
        Method::GET,
        "/",
        HeaderMap::new(),
        Bytes::new(),
        &head_limits,
    )?;
    let failure = client
        .execute(request, control())
        .await
        .err()
        .ok_or_else(|| io::Error::other("oversized response head unexpectedly succeeded"))?;
    assert_eq!(failure.phase(), Phase::ResponseHead);
    assert_eq!(failure.transmission(), TransmissionFact::MayHaveTransmitted);
    assert_eq!(server.await??.1, 1);
    Ok(())
}

#[tokio::test]
async fn chunked_body_is_pull_driven_and_bounded() -> TestResult {
    const BODY_BYTES: usize = 16 * 1_024 * 1_024;
    let endpoint = make_endpoint(&[b"http/1.1"]).await?;
    let Endpoint {
        listener,
        acceptor,
        client_config,
        origin,
    } = endpoint;
    let (finished_tx, mut finished_rx) = mpsc::channel(1);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut stream = acceptor.accept(stream).await?;
        let _request = read_request(&mut stream).await?;
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1000000\r\n")
            .await?;
        let block = vec![b'x'; 64 * 1_024];
        for _ in 0..(BODY_BYTES / block.len()) {
            stream.write_all(&block).await?;
        }
        stream.write_all(b"\r\n0\r\n\r\n").await?;
        // `write_all` may leave encrypted records in the TLS buffer; dropping the
        // stream here would discard the terminal chunk and the client would see an
        // EOF mid-body. `shutdown` flushes and sends close_notify.
        stream.shutdown().await?;
        finished_tx
            .send(())
            .await
            .map_err(|_| io::Error::other("finished receiver dropped"))?;
        Ok::<_, TestError>(())
    });
    let limits = Limits::default();
    let client = make_client(client_config, limits.clone())?;
    let request = make_request(
        origin,
        Method::GET,
        "/stream",
        HeaderMap::new(),
        Bytes::new(),
        &limits,
    )?;
    let mut body = client.execute(request, control()).await?.into_body();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(finished_rx.try_recv().is_err());
    let mut received = 0_usize;
    while let Some(chunk) = body.next_chunk().await {
        let chunk = chunk?;
        assert!(chunk.len() <= limits.max_response_chunk_bytes);
        received = received.saturating_add(chunk.len());
    }
    assert_eq!(received, BODY_BYTES);
    timeout(Duration::from_secs(2), finished_rx.recv())
        .await?
        .ok_or_else(|| io::Error::other("server completion signal missing"))?;
    server.await??;
    Ok(())
}

#[tokio::test]
async fn cancellation_and_body_drop_close_and_join_connection_work() -> TestResult {
    let endpoint = make_endpoint(&[b"http/1.1"]).await?;
    let Endpoint {
        listener,
        acceptor,
        client_config,
        origin,
    } = endpoint;
    let (ready_tx, ready_rx) = oneshot::channel();
    let (closed_tx, closed_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut stream = acceptor.accept(stream).await?;
        let _request = read_request(&mut stream).await?;
        ready_tx
            .send(())
            .map_err(|()| io::Error::other("ready receiver dropped"))?;
        let mut byte = [0_u8; 1];
        let closed = !matches!(stream.read(&mut byte).await, Ok(1));
        closed_tx
            .send(closed)
            .map_err(|_| io::Error::other("closed receiver dropped"))?;
        Ok::<_, TestError>(())
    });
    let limits = Limits::default();
    let client = make_client(client_config, limits.clone())?;
    let request = make_request(
        origin,
        Method::GET,
        "/wait",
        HeaderMap::new(),
        Bytes::new(),
        &limits,
    )?;
    let cancellation = CancellationToken::new();
    let exchange_control = ExchangeControl::with_cancellation(
        Instant::now() + Duration::from_secs(10),
        cancellation.clone(),
    );
    let exchange = tokio::spawn(async move { client.execute(request, exchange_control).await });
    ready_rx.await?;
    cancellation.cancel();
    let failure = exchange
        .await?
        .err()
        .ok_or_else(|| io::Error::other("cancelled exchange unexpectedly succeeded"))?;
    assert_eq!(failure.phase(), Phase::RequestWriteOrResponseHead);
    assert_eq!(failure.kind(), ErrorKind::Cancelled);
    assert_eq!(failure.transmission(), TransmissionFact::MayHaveTransmitted);
    assert!(timeout(Duration::from_secs(2), closed_rx).await??);
    server.await??;

    let endpoint = make_endpoint(&[b"http/1.1"]).await?;
    let Endpoint {
        listener,
        acceptor,
        client_config,
        origin,
    } = endpoint;
    let (closed_tx, closed_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut stream = acceptor.accept(stream).await?;
        let _request = read_request(&mut stream).await?;
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n")
            .await?;
        let mut byte = [0_u8; 1];
        let closed = !matches!(stream.read(&mut byte).await, Ok(1));
        closed_tx
            .send(closed)
            .map_err(|_| io::Error::other("closed receiver dropped"))?;
        Ok::<_, TestError>(())
    });
    let client = make_client(client_config, limits.clone())?;
    let request = make_request(
        origin,
        Method::GET,
        "/drop",
        HeaderMap::new(),
        Bytes::new(),
        &limits,
    )?;
    let response = client.execute(request, control()).await?;
    drop(response.into_body());
    assert!(timeout(Duration::from_secs(2), closed_rx).await??);
    server.await??;
    Ok(())
}

#[tokio::test]
async fn admitted_exchange_deadline_is_conservative_and_joins_work() -> TestResult {
    let endpoint = make_endpoint(&[b"http/1.1"]).await?;
    let Endpoint {
        listener,
        acceptor,
        client_config,
        origin,
    } = endpoint;
    let (closed_tx, closed_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut stream = acceptor.accept(stream).await?;
        let _request = read_request(&mut stream).await?;
        let mut byte = [0_u8; 1];
        let closed = !matches!(stream.read(&mut byte).await, Ok(1));
        closed_tx
            .send(closed)
            .map_err(|_| io::Error::other("closed receiver dropped"))?;
        Ok::<_, TestError>(())
    });
    let limits = Limits::default();
    let client = make_client(client_config, limits.clone())?;
    let request = make_request(
        origin,
        Method::GET,
        "/timeout",
        HeaderMap::new(),
        Bytes::new(),
        &limits,
    )?;
    let failure = client
        .execute(
            request,
            ExchangeControl::new(Instant::now() + Duration::from_millis(100)),
        )
        .await
        .err()
        .ok_or_else(|| io::Error::other("timed-out exchange unexpectedly succeeded"))?;
    assert_eq!(failure.phase(), Phase::ResponseHead);
    assert_eq!(failure.kind(), ErrorKind::DeadlineExceeded);
    assert_eq!(failure.transmission(), TransmissionFact::MayHaveTransmitted);
    assert!(timeout(Duration::from_secs(2), closed_rx).await??);
    server.await??;
    Ok(())
}

#[tokio::test]
async fn flushed_request_head_deadline_is_precise_and_joins_work() -> TestResult {
    let endpoint = make_endpoint(&[b"http/1.1"]).await?;
    let Endpoint {
        listener,
        acceptor,
        client_config,
        origin,
    } = endpoint;
    let (closed_tx, closed_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut stream = acceptor.accept(stream).await?;
        let _request = read_request(&mut stream).await?;
        let mut byte = [0_u8; 1];
        let closed = !matches!(stream.read(&mut byte).await, Ok(1));
        closed_tx
            .send(closed)
            .map_err(|_| io::Error::other("closed receiver dropped"))?;
        Ok::<_, TestError>(())
    });
    let limits = Limits {
        response_head_timeout: Duration::from_millis(100),
        ..Limits::default()
    };
    let client = make_client(client_config, limits.clone())?;
    let request = make_request(
        origin,
        Method::GET,
        "/timeout",
        HeaderMap::new(),
        Bytes::new(),
        &limits,
    )?;
    let failure = client
        .execute(
            request,
            ExchangeControl::new(Instant::now() + Duration::from_secs(5)),
        )
        .await
        .err()
        .ok_or_else(|| io::Error::other("timed-out exchange unexpectedly succeeded"))?;
    assert_eq!(failure.phase(), Phase::ResponseHead);
    assert_eq!(failure.kind(), ErrorKind::DeadlineExceeded);
    assert_eq!(failure.transmission(), TransmissionFact::MayHaveTransmitted);
    assert!(timeout(Duration::from_secs(2), closed_rx).await??);
    server.await??;
    Ok(())
}

#[tokio::test]
async fn stalled_request_write_after_body_poll_keeps_write_deadline() -> TestResult {
    let endpoint = make_endpoint(&[b"http/1.1"]).await?;
    let Endpoint {
        listener,
        acceptor,
        client_config,
        origin,
    } = endpoint;
    let (release, released) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let _stream = acceptor.accept(stream).await?;
        let _ = released.await;
        Ok::<_, TestError>(())
    });
    let limits = Limits {
        request_write_timeout: Duration::from_millis(200),
        response_head_timeout: Duration::from_secs(5),
        ..Limits::default()
    };
    let client = make_client(client_config, limits.clone())?;
    let request = make_request(
        origin,
        Method::POST,
        "/slow-write",
        HeaderMap::new(),
        Bytes::from(vec![b'x'; limits.max_request_body_bytes]),
        &limits,
    )?;
    let outcome = timeout(Duration::from_secs(2), client.execute(request, control())).await;
    let _ = release.send(());
    server.await??;
    let failure = outcome?
        .err()
        .ok_or_else(|| io::Error::other("slow request write unexpectedly succeeded"))?;
    assert_eq!(failure.phase(), Phase::RequestWrite);
    assert_eq!(failure.kind(), ErrorKind::DeadlineExceeded);
    assert_eq!(failure.transmission(), TransmissionFact::MayHaveTransmitted);
    Ok(())
}

#[tokio::test]
async fn slow_allowed_write_does_not_spend_response_head_budget() -> TestResult {
    let Endpoint {
        listener,
        acceptor,
        client_config,
        origin,
    } = make_endpoint(&[b"http/1.1"]).await?;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut stream = acceptor.accept(stream).await?;
        // Longer than the head allowance, but inside the write allowance.
        tokio::time::sleep(Duration::from_secs(1)).await;
        let request = read_request(&mut stream).await?;
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .await?;
        stream.shutdown().await?;
        Ok::<_, TestError>(request.len())
    });
    let limits = Limits {
        request_write_timeout: Duration::from_secs(5),
        response_head_timeout: Duration::from_millis(500),
        ..Limits::default()
    };
    let client = make_client(client_config, limits.clone())?;
    let request = make_request(
        origin,
        Method::POST,
        "/slow-allowed-write",
        HeaderMap::new(),
        Bytes::from(vec![b'x'; limits.max_request_body_bytes]),
        &limits,
    )?;
    let outcome = timeout(Duration::from_secs(3), client.execute(request, control())).await;
    let peer = timeout(Duration::from_secs(3), server).await;
    let mut body = outcome??.into_body();
    assert_eq!(required_chunk(&mut body).await?, "ok");
    assert!(body.next_chunk().await.is_none());
    assert!(peer??? > limits.max_request_body_bytes);
    Ok(())
}

#[tokio::test]
async fn asymmetric_write_and_head_timeouts_are_sequential() -> TestResult {
    let endpoint = make_endpoint(&[b"http/1.1"]).await?;
    let Endpoint {
        listener,
        acceptor,
        client_config,
        origin,
    } = endpoint;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut stream = acceptor.accept(stream).await?;
        let _request = read_request(&mut stream).await?;
        tokio::time::sleep(Duration::from_millis(100)).await;
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .await?;
        Ok::<_, TestError>(())
    });
    let limits = Limits {
        request_write_timeout: Duration::from_millis(30),
        response_head_timeout: Duration::from_millis(500),
        ..Limits::default()
    };
    let client = make_client(client_config, limits.clone())?;
    let request = make_request(
        origin,
        Method::POST,
        "/asymmetric-timeouts",
        HeaderMap::new(),
        Bytes::from_static(b"payload"),
        &limits,
    )?;
    let mut body = client.execute(request, control()).await?.into_body();
    assert_eq!(required_chunk(&mut body).await?, "ok");
    assert!(body.next_chunk().await.is_none());
    server.await??;
    Ok(())
}

#[tokio::test]
async fn peer_reset_after_request_is_not_mislabeled_response_head() -> TestResult {
    let endpoint = make_endpoint(&[b"http/1.1"]).await?;
    let Endpoint {
        listener,
        acceptor,
        client_config,
        origin,
    } = endpoint;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut stream = acceptor.accept(stream).await?;
        let _request = read_request(&mut stream).await?;
        Ok::<_, TestError>(())
    });
    let limits = Limits::default();
    let client = make_client(client_config, limits.clone())?;
    let request = make_request(
        origin,
        Method::GET,
        "/reset",
        HeaderMap::new(),
        Bytes::new(),
        &limits,
    )?;
    let failure = client
        .execute(request, control())
        .await
        .err()
        .ok_or_else(|| io::Error::other("reset exchange unexpectedly succeeded"))?;
    assert_eq!(failure.phase(), Phase::RequestWriteOrResponseHead);
    assert_eq!(failure.kind(), ErrorKind::Protocol);
    assert_eq!(failure.transmission(), TransmissionFact::MayHaveTransmitted);
    assert!(!failure.transmission().peer_cannot_have_acted());
    assert!(matches!(
        failure.cause(),
        Some("incomplete_message" | "unexpected_eof" | "connection_reset")
    ));
    assert_eq!(failure.wire_protocol(), Some(WireProtocol::Http1));
    assert_eq!(failure.tls_cause(), None);
    assert!(!failure.via_http2_io());
    server.await??;
    Ok(())
}

/// The real-use shape: a negotiated HTTP/1.1 connection whose peer sends a
/// record that fails TLS authentication after reading the request. The I/O
/// kind alone is `invalid_data`; the Rustls cause and protocol say which.
#[tokio::test]
async fn tls_record_failure_after_request_names_rustls_cause_and_protocol() -> TestResult {
    let Endpoint {
        listener,
        acceptor,
        client_config,
        origin,
    } = make_endpoint(&[b"http/1.1"]).await?;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut stream = acceptor.accept(stream).await?;
        let _request = read_request(&mut stream).await?;
        let (mut tcp, _) = stream.into_inner();
        // An application-data record whose ciphertext cannot authenticate.
        let mut record = vec![0x17, 0x03, 0x03, 0x00, 0x20];
        record.extend_from_slice(&[0x5a; 0x20]);
        tcp.write_all(&record).await?;
        let mut sink = [0_u8; 256];
        let _ = timeout(Duration::from_secs(2), tcp.read(&mut sink)).await;
        Ok::<_, TestError>(())
    });
    let limits = Limits::default();
    let client = make_client(client_config, limits.clone())?;
    let request = make_request(
        origin,
        Method::POST,
        "/v1/messages",
        HeaderMap::new(),
        Bytes::from_static(b"{}"),
        &limits,
    )?;
    let failure = client
        .execute(request, control())
        .await
        .err()
        .ok_or("a corrupted record unexpectedly produced a response")?;
    assert_eq!(failure.phase(), Phase::RequestWriteOrResponseHead);
    assert_eq!(failure.kind(), ErrorKind::Protocol);
    assert_eq!(failure.transmission(), TransmissionFact::MayHaveTransmitted);
    assert_eq!(failure.cause(), Some("invalid_data"));
    assert_eq!(failure.tls_cause(), Some(TlsCause::DecryptError));
    assert_eq!(failure.wire_protocol(), Some(WireProtocol::Http1));
    assert!(format!("{failure:?}").contains("tls: Some(\"tls_decrypt_error\")"));
    timeout(Duration::from_secs(3), server).await???;
    Ok(())
}

/// A handshake refusal keeps the peer's alert code instead of discarding it.
#[tokio::test]
async fn handshake_alert_and_certificate_rejection_name_their_tls_cause() -> TestResult {
    let Endpoint {
        listener,
        acceptor,
        origin,
        ..
    } = make_endpoint(&[b"http/1.1"]).await?;
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let tls12_server = ServerConfig::builder_with_provider(Arc::clone(&provider))
        .with_protocol_versions(&[&rustls::version::TLS12])?
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der())),
        )?;
    let mut roots = RootCertStore::empty();
    roots.add(cert.der().clone())?;
    let tls13_client = ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let untrusting_client = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(RootCertStore::empty())
        .with_no_client_auth();
    let incompatible = TlsAcceptor::from(Arc::new(tls12_server));
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let _refused = incompatible.accept(stream).await;
        let (stream, _) = listener.accept().await?;
        let _refused = acceptor.accept(stream).await;
        Ok::<_, TestError>(())
    });
    let limits = Limits::default();
    let request = || {
        make_request(
            origin.clone(),
            Method::GET,
            "/",
            HeaderMap::new(),
            Bytes::new(),
            &limits,
        )
    };
    let alert = make_client(tls13_client, limits.clone())?
        .execute(request()?, control())
        .await
        .err()
        .ok_or("an incompatible handshake unexpectedly succeeded")?;
    assert_eq!(alert.phase(), Phase::Tls);
    assert_eq!(alert.kind(), ErrorKind::Tls);
    assert_eq!(alert.transmission(), TransmissionFact::NotStarted);
    assert_eq!(alert.cause(), Some("invalid_data"));
    // protocol_version
    assert_eq!(alert.tls_cause(), Some(TlsCause::AlertReceived(70)));
    assert_eq!(alert.wire_protocol(), None);

    let rejected = make_client(untrusting_client, limits.clone())?
        .execute(request()?, control())
        .await
        .err()
        .ok_or("an untrusted certificate unexpectedly succeeded")?;
    assert_eq!(rejected.phase(), Phase::Tls);
    assert_eq!(rejected.tls_cause(), Some(TlsCause::InvalidCertificate));
    timeout(Duration::from_secs(3), server).await???;
    Ok(())
}

/// A peer that drops the connection while the body is still being written
/// never received the whole request, so a replay cannot duplicate it.
#[tokio::test]
async fn peer_reset_during_body_write_is_request_incomplete() -> TestResult {
    let endpoint = make_endpoint(&[b"http/1.1"]).await?;
    let Endpoint {
        listener,
        acceptor,
        client_config,
        origin,
    } = endpoint;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut stream = acceptor.accept(stream).await?;
        let mut head = Vec::new();
        let mut buffer = [0_u8; 1_024];
        while !head.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            head.extend_from_slice(&buffer[..read]);
        }
        // Closing with unread request bytes makes the kernel reset the
        // connection instead of draining it.
        drop(stream);
        Ok::<_, TestError>(())
    });
    let limits = Limits {
        max_request_body_bytes: 64 * 1_024 * 1_024,
        ..Limits::default()
    };
    let client = make_client(client_config, limits.clone())?;
    let request = make_request(
        origin,
        Method::POST,
        "/large",
        HeaderMap::new(),
        Bytes::from(vec![b'x'; 48 * 1_024 * 1_024]),
        &limits,
    )?;
    let failure = client
        .execute(request, control())
        .await
        .err()
        .ok_or_else(|| io::Error::other("reset upload unexpectedly succeeded"))?;
    assert_eq!(failure.phase(), Phase::RequestWriteOrResponseHead);
    assert_eq!(failure.transmission(), TransmissionFact::RequestIncomplete);
    assert!(failure.transmission().peer_cannot_have_acted());
    server.await??;
    Ok(())
}

#[tokio::test]
async fn a_peer_that_read_the_whole_body_before_failing_may_have_acted() -> TestResult {
    const BODY: usize = 4 * 1_024;
    let endpoint = make_endpoint(&[b"http/1.1"]).await?;
    let Endpoint {
        listener,
        acceptor,
        client_config,
        origin,
    } = endpoint;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut stream = acceptor.accept(stream).await?;
        let mut received = Vec::new();
        let mut buffer = [0_u8; 1_024];
        let complete = |bytes: &[u8]| {
            bytes
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .is_some_and(|end| bytes.len() >= end + 4 + BODY)
        };
        while !complete(&received) {
            let read = stream.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            received.extend_from_slice(&buffer[..read]);
        }
        drop(stream);
        Ok::<_, TestError>(())
    });
    let limits = Limits::default();
    let client = make_client(client_config, limits.clone())?;
    let request = make_request(
        origin,
        Method::POST,
        "/whole",
        HeaderMap::new(),
        Bytes::from(vec![b'x'; BODY]),
        &limits,
    )?;
    let failure = client
        .execute(request, control())
        .await
        .err()
        .ok_or_else(|| io::Error::other("an unanswered request unexpectedly succeeded"))?;
    assert_eq!(failure.transmission(), TransmissionFact::MayHaveTransmitted);
    assert!(!failure.transmission().peer_cannot_have_acted());
    server.await??;
    Ok(())
}

#[tokio::test]
async fn pre_cancel_is_not_started_and_does_not_connect() -> TestResult {
    let endpoint = make_endpoint(&[b"http/1.1"]).await?;
    let limits = Limits::default();
    let client = make_client(endpoint.client_config, limits.clone())?;
    let request = make_request(
        endpoint.origin,
        Method::GET,
        "/cancelled",
        HeaderMap::new(),
        Bytes::new(),
        &limits,
    )?;
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let failure = client
        .execute(
            request,
            ExchangeControl::with_cancellation(
                Instant::now() + Duration::from_secs(1),
                cancellation,
            ),
        )
        .await
        .err()
        .ok_or_else(|| io::Error::other("pre-cancelled exchange unexpectedly succeeded"))?;
    assert_eq!(failure.phase(), Phase::Resolution);
    assert_eq!(failure.kind(), ErrorKind::Cancelled);
    assert_eq!(failure.transmission(), TransmissionFact::NotStarted);
    assert!(
        timeout(Duration::from_millis(100), endpoint.listener.accept())
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn idle_body_cancellation_stops_socket_without_another_pull() -> TestResult {
    let endpoint = make_endpoint(&[b"http/1.1"]).await?;
    let Endpoint {
        listener,
        acceptor,
        client_config,
        origin,
    } = endpoint;
    let (closed_tx, closed_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut stream = acceptor.accept(stream).await?;
        let _request = read_request(&mut stream).await?;
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n")
            .await?;
        let mut byte = [0_u8; 1];
        let closed = !matches!(stream.read(&mut byte).await, Ok(1));
        closed_tx
            .send(closed)
            .map_err(|_| io::Error::other("closed receiver dropped"))?;
        Ok::<_, TestError>(())
    });
    let limits = Limits::default();
    let client = make_client(client_config, limits.clone())?;
    let request = make_request(
        origin,
        Method::GET,
        "/idle",
        HeaderMap::new(),
        Bytes::new(),
        &limits,
    )?;
    let cancellation = CancellationToken::new();
    let exchange_control = ExchangeControl::with_cancellation(
        Instant::now() + Duration::from_secs(10),
        cancellation.clone(),
    );
    let mut body = client.execute(request, exchange_control).await?.into_body();
    cancellation.cancel();
    assert!(timeout(Duration::from_secs(2), closed_rx).await??);
    let failure = body
        .next_chunk()
        .await
        .transpose()
        .err()
        .ok_or_else(|| io::Error::other("cancelled idle body unexpectedly succeeded"))?;
    assert_eq!(failure.phase(), Phase::ResponseBody);
    assert_eq!(failure.kind(), ErrorKind::Cancelled);
    server.await??;
    Ok(())
}

async fn held_streaming_body(
    response_body_timeout: Duration,
    control: ExchangeControl,
) -> TestResult<(
    JoinHandle<Result<Option<Bytes>, plakous_http::Failure>>,
    oneshot::Sender<()>,
    JoinHandle<TestResult>,
)> {
    let endpoint = make_endpoint(&[b"http/1.1"]).await?;
    let Endpoint {
        listener,
        acceptor,
        client_config,
        origin,
    } = endpoint;
    let (release, held) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut stream = acceptor.accept(stream).await?;
        let _request = read_request(&mut stream).await?;
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n")
            .await?;
        stream.flush().await?;
        let _ = held.await;
        let _ = stream.write_all(b"ok").await;
        let _ = stream.shutdown().await;
        Ok::<_, TestError>(())
    });
    let mut limits = Limits::for_streaming_inference();
    limits.response_body_timeout = response_body_timeout;
    let client = make_client(client_config, limits.clone())?;
    let request = make_request(
        origin,
        Method::GET,
        "/held",
        HeaderMap::new(),
        Bytes::new(),
        &limits,
    )?;
    let body = client.execute(request, control).await?.into_body();
    tokio::time::pause();
    let pending = tokio::spawn(async move {
        let mut body = body;
        body.next_chunk().await.transpose()
    });
    tokio::task::yield_now().await;
    Ok((pending, release, server))
}

fn streaming_control(head_deadline: Duration) -> ExchangeControl {
    ExchangeControl::streaming(Instant::now() + head_deadline, CancellationToken::new())
}

#[tokio::test]
async fn streaming_body_outlives_its_head_deadline_while_inside_the_idle_bound() -> TestResult {
    let (body, release, server) = held_streaming_body(
        Duration::from_secs(180),
        streaming_control(Duration::from_secs(10)),
    )
    .await?;
    tokio::time::advance(Duration::from_secs(170)).await;
    tokio::task::yield_now().await;
    assert!(
        !body.is_finished(),
        "a streaming head deadline must not bound the response body"
    );
    // The body arrives over a real socket. Resume the virtual clock before
    // waiting for it so Tokio cannot advance an idle paused runtime to the
    // idle bound while the OS is delivering the released bytes.
    tokio::time::resume();
    release
        .send(())
        .map_err(|()| io::Error::other("held response closed"))?;
    assert_eq!(
        body.await??
            .ok_or_else(|| io::Error::other("missing body"))?,
        "ok"
    );
    server.await??;
    Ok(())
}

#[tokio::test]
async fn stalled_streaming_body_fails_at_the_idle_bound() -> TestResult {
    let (body, release, server) = held_streaming_body(
        Duration::from_secs(180),
        streaming_control(Duration::from_secs(10)),
    )
    .await?;
    tokio::time::advance(Duration::from_secs(181)).await;
    let failure = body
        .await?
        .err()
        .ok_or_else(|| io::Error::other("stalled body unexpectedly succeeded"))?;
    assert_eq!(failure.phase(), Phase::ResponseBody);
    assert_eq!(failure.kind(), ErrorKind::DeadlineExceeded);
    let _ = release.send(());
    tokio::time::resume();
    server.await??;
    Ok(())
}

#[tokio::test]
async fn whole_exchange_deadline_still_bounds_the_body() -> TestResult {
    let (body, release, server) = held_streaming_body(
        Duration::from_secs(300),
        ExchangeControl::new(Instant::now() + Duration::from_secs(180)),
    )
    .await?;
    tokio::time::advance(Duration::from_secs(181)).await;
    let failure = body
        .await?
        .err()
        .ok_or_else(|| io::Error::other("late body unexpectedly succeeded"))?;
    assert_eq!(failure.phase(), Phase::ResponseBody);
    assert_eq!(failure.kind(), ErrorKind::DeadlineExceeded);
    let _ = release.send(());
    tokio::time::resume();
    server.await??;
    Ok(())
}

/// A stream that keeps sending stays open well past its head deadline and
/// several idle bounds: progress, not elapsed time, keeps it alive.
#[tokio::test]
async fn progressing_streaming_body_has_no_absolute_cap() -> TestResult {
    const CHUNKS: usize = 12;
    let endpoint = make_endpoint(&[b"http/1.1"]).await?;
    let Endpoint {
        listener,
        acceptor,
        client_config,
        origin,
    } = endpoint;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut stream = acceptor.accept(stream).await?;
        let _request = read_request(&mut stream).await?;
        stream
            .write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {CHUNKS}\r\n\r\n").as_bytes())
            .await?;
        stream.flush().await?;
        for _ in 0..CHUNKS {
            tokio::time::sleep(Duration::from_millis(100)).await;
            stream.write_all(b"x").await?;
            stream.flush().await?;
        }
        let _ = stream.shutdown().await;
        Ok::<_, TestError>(())
    });
    let mut limits = Limits::for_streaming_inference();
    limits.response_body_timeout = Duration::from_millis(500);
    let client = make_client(client_config, limits.clone())?;
    let request = make_request(
        origin,
        Method::GET,
        "/progress",
        HeaderMap::new(),
        Bytes::new(),
        &limits,
    )?;
    let started = Instant::now();
    let mut body = client
        .execute(request, streaming_control(Duration::from_millis(300)))
        .await?
        .into_body();
    let mut received = 0;
    while let Some(chunk) = body.next_chunk().await {
        received += chunk?.len();
    }
    assert_eq!(received, CHUNKS);
    assert!(
        started.elapsed() > Duration::from_millis(1_000),
        "the stream must outlast its head deadline and its idle bound"
    );
    server.await??;
    Ok(())
}

#[tokio::test]
async fn malformed_response_is_not_retried() -> TestResult {
    let (config, origin, server) =
        responding_server(b"HTTP/1.1 not-a-status\r\nBroken Header\r\n\r\n".to_vec()).await?;
    let limits = Limits::default();
    let client = make_client(config, limits.clone())?;
    let request = make_request(
        origin,
        Method::GET,
        "/malformed",
        HeaderMap::new(),
        Bytes::new(),
        &limits,
    )?;
    let failure = client
        .execute(request, control())
        .await
        .err()
        .ok_or_else(|| io::Error::other("malformed response unexpectedly succeeded"))?;
    assert_eq!(failure.phase(), Phase::ResponseHead);
    assert_eq!(failure.kind(), ErrorKind::Protocol);
    assert_eq!(failure.transmission(), TransmissionFact::MayHaveTransmitted);
    assert_eq!(failure.cause(), Some("http_parse"));
    assert_eq!(server.await??.1, 1);
    Ok(())
}

#[tokio::test]
async fn http2_remote_reset_retains_reason_without_retry() -> TestResult {
    let Endpoint {
        listener,
        acceptor,
        client_config,
        origin,
    } = make_endpoint(&[b"h2"]).await?;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let stream = acceptor.accept(stream).await?;
        let mut connection = h2::server::handshake(stream).await?;
        let (_, mut response) = connection.accept().await.ok_or("no HTTP2 request")??;
        response.send_reset(h2::Reason::REFUSED_STREAM);
        assert!(connection.accept().await.is_none());
        Ok::<_, TestError>(())
    });
    let limits = Limits::default();
    let client = make_client(client_config, limits.clone())?;
    let request = make_request(
        origin,
        Method::GET,
        "/reset",
        HeaderMap::new(),
        Bytes::new(),
        &limits,
    )?;
    let failure = client
        .execute(request, control())
        .await
        .err()
        .ok_or("reset unexpectedly succeeded")?;
    assert_eq!(failure.kind(), ErrorKind::Protocol);
    assert_eq!(failure.cause(), Some("http2_remote_reset"));
    assert_eq!(
        failure.http2_reason(),
        Some(u32::from(h2::Reason::REFUSED_STREAM))
    );
    assert_eq!(failure.transmission(), TransmissionFact::MayHaveTransmitted);
    timeout(Duration::from_secs(3), server).await???;
    Ok(())
}

#[tokio::test]
async fn http2_body_failures_retain_reset_and_io_causes() -> TestResult {
    for reset in [false, true] {
        let Endpoint {
            listener,
            acceptor,
            client_config,
            origin,
        } = make_endpoint(&[b"h2"]).await?;
        let (ready, proceed) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let stream = acceptor.accept(stream).await?;
            let mut connection = h2::server::handshake(stream).await?;
            let (_, mut response) = connection.accept().await.ok_or("no HTTP2 request")??;
            let mut body = response.send_response(http::Response::new(()), false)?;
            tokio::select! {
                result = proceed => { result?; }
                _ = connection.accept() => return Err("connection ended before body test".into()),
            }
            if reset {
                body.send_reset(h2::Reason::CANCEL);
                assert!(connection.accept().await.is_none());
            }
            Ok::<_, TestError>(())
        });
        let limits = Limits::default();
        let client = make_client(client_config, limits.clone())?;
        let request = make_request(
            origin,
            Method::GET,
            "/body",
            HeaderMap::new(),
            Bytes::new(),
            &limits,
        )?;
        let mut body = client.execute(request, control()).await?.into_body();
        ready.send(()).map_err(|()| "body peer disappeared")?;
        let failure = body
            .next_chunk()
            .await
            .ok_or("body fault unexpectedly completed")?
            .err()
            .ok_or("body fault yielded data")?;
        assert_eq!(failure.phase(), Phase::ResponseBody);
        assert_eq!(failure.wire_protocol(), Some(WireProtocol::Http2));
        if reset {
            assert_eq!(failure.cause(), Some("http2_remote_reset"));
            assert_eq!(failure.http2_reason(), Some(u32::from(h2::Reason::CANCEL)));
            assert!(!failure.via_http2_io());
        } else {
            assert!(matches!(
                failure.cause(),
                Some("unexpected_eof" | "connection_reset" | "broken_pipe")
            ));
            assert_eq!(failure.http2_reason(), None);
        }
        timeout(Duration::from_secs(3), server).await???;
    }
    Ok(())
}

#[tokio::test]
async fn negotiates_http2_with_one_request_and_owned_tasks() -> TestResult {
    let endpoint = make_endpoint(&[b"h2"]).await?;
    let Endpoint {
        listener,
        acceptor,
        client_config,
        origin,
    } = endpoint;
    let requests = Arc::new(AtomicUsize::new(0));
    let server_requests = Arc::clone(&requests);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let stream = acceptor.accept(stream).await?;
        let service = service_fn(move |_request| {
            server_requests.fetch_add(1, Ordering::AcqRel);
            async {
                Ok::<_, InfallibleResponse>(hyper::Response::new(Full::new(Bytes::from_static(
                    b"h2-ok",
                ))))
            }
        });
        let _result = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
            .serve_connection(TokioIo::new(stream), service)
            .await;
        Ok::<_, TestError>(())
    });
    let limits = Limits::default();
    let client = make_client(client_config, limits.clone())?;
    let request = make_request(
        origin,
        Method::GET,
        "/h2",
        HeaderMap::new(),
        Bytes::new(),
        &limits,
    )?;
    let response = client.execute(request, control()).await?;
    assert_eq!(response.version(), Version::HTTP_2);
    let mut body = response.into_body();
    assert_eq!(required_chunk(&mut body).await?, "h2-ok");
    assert!(body.next_chunk().await.is_none());
    timeout(Duration::from_secs(2), server).await???;
    assert_eq!(requests.load(Ordering::Acquire), 1);
    Ok(())
}

struct GatedHttp2Body(mpsc::Receiver<Bytes>);

impl GatedHttp2Body {
    fn take(receiver: &std::sync::Mutex<Option<mpsc::Receiver<Bytes>>>) -> io::Result<Self> {
        receiver
            .lock()
            .map_err(|_| io::Error::other("fixture body lock poisoned"))?
            .take()
            .map(Self)
            .ok_or_else(|| io::Error::other("unexpected second request"))
    }
}

impl hyper::body::Body for GatedHttp2Body {
    type Data = Bytes;
    type Error = InfallibleResponse;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<hyper::body::Frame<Bytes>, Self::Error>>> {
        self.0
            .poll_recv(cx)
            .map(|value| value.map(|bytes| Ok(hyper::body::Frame::data(bytes))))
    }
}

#[tokio::test]
async fn http2_response_head_keeps_delayed_stream_body_alive() -> TestResult {
    delayed_http2_body(false).await
}

#[tokio::test]
async fn http2_graceful_shutdown_keeps_accepted_stream_body_alive() -> TestResult {
    delayed_http2_body(true).await
}

async fn delayed_http2_body(graceful_shutdown: bool) -> TestResult {
    let Endpoint {
        listener,
        acceptor,
        client_config,
        origin,
    } = make_endpoint(&[b"h2"]).await?;
    let (send, receive) = mpsc::channel(2);
    let receive = Arc::new(std::sync::Mutex::new(Some(receive)));
    let requests = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&requests);
    let (shutdown, shutdown_requested) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let stream = acceptor.accept(stream).await?;
        let service = service_fn(move |_request| {
            observed.fetch_add(1, Ordering::AcqRel);
            let body = GatedHttp2Body::take(&receive);
            async move { Ok::<_, io::Error>(hyper::Response::new(body?)) }
        });
        let builder = hyper::server::conn::http2::Builder::new(TokioExecutor::new());
        let connection = builder.serve_connection(TokioIo::new(stream), service);
        tokio::pin!(connection);
        if graceful_shutdown {
            tokio::select! {
                result = &mut connection => { result?; return Ok::<_, TestError>(()); }
                _ = shutdown_requested => connection.as_mut().graceful_shutdown(),
            }
        }
        let _result = connection.await;
        Ok::<_, TestError>(())
    });
    let limits = Limits {
        max_response_chunk_bytes: 16 * 1024,
        ..Limits::default()
    };
    let client = make_client(client_config, limits.clone())?;
    let request = make_request(
        origin,
        Method::POST,
        "/stream",
        HeaderMap::new(),
        Bytes::from_static(b"{}"),
        &limits,
    )?;
    let response = client.execute(request, control()).await?;
    assert_eq!(response.version(), Version::HTTP_2);
    let mut body = response.into_body();
    let _ = shutdown.send(());
    // The peer cannot publish any body until the caller owns the response head.
    send.send(Bytes::from_static(b"first")).await?;
    assert_eq!(required_chunk(&mut body).await?, "first");
    // Cross the advertised H2 receive window so continuation needs flow control.
    let expected = vec![b'x'; 128 * 1024];
    send.send(Bytes::from(expected.clone())).await?;
    let mut received = Vec::new();
    while received.len() < expected.len() {
        received.extend_from_slice(&required_chunk(&mut body).await?);
    }
    assert_eq!(received, expected);
    drop(send);
    while let Some(chunk) = body.next_chunk().await {
        assert!(
            chunk?.is_empty(),
            "unexpected bytes after complete fixture body"
        );
    }
    timeout(Duration::from_secs(2), server).await???;
    assert_eq!(requests.load(Ordering::Acquire), 1);
    Ok(())
}

#[tokio::test]
async fn http2_delayed_body_cleanup_closes_owned_connection() -> TestResult {
    for mode in ["close", "drop", "cancel", "deadline", "limit"] {
        let Endpoint {
            listener,
            acceptor,
            client_config,
            origin,
        } = make_endpoint(&[b"h2"]).await?;
        let (send, receive) = mpsc::channel(1);
        let receive = Arc::new(std::sync::Mutex::new(Some(receive)));
        let requests = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&requests);
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let stream = acceptor.accept(stream).await?;
            let service = service_fn(move |_request| {
                observed.fetch_add(1, Ordering::AcqRel);
                let body = GatedHttp2Body::take(&receive);
                async move { Ok::<_, io::Error>(hyper::Response::new(body?)) }
            });
            let _result = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                .serve_connection(TokioIo::new(stream), service)
                .await;
            Ok::<_, TestError>(())
        });
        let limits = Limits {
            max_response_body_bytes: 4,
            ..Limits::default()
        };
        let client = make_client(client_config, limits.clone())?;
        let request = make_request(
            origin,
            Method::GET,
            "/cleanup",
            HeaderMap::new(),
            Bytes::new(),
            &limits,
        )?;
        let cancellation = CancellationToken::new();
        let response = client
            .execute(
                request,
                ExchangeControl::with_cancellation(
                    Instant::now() + Duration::from_secs(5),
                    cancellation.clone(),
                ),
            )
            .await?;
        assert_eq!(response.version(), Version::HTTP_2);
        let mut body = response.into_body();
        send.send(Bytes::from_static(b"x")).await?;
        assert_eq!(required_chunk(&mut body).await?, "x");
        assert!(!server.is_finished(), "cleanup requires a live stream");
        match mode {
            "close" => body.close().await?,
            "drop" => drop(body),
            "cancel" | "deadline" => {
                if mode == "cancel" {
                    cancellation.cancel();
                }
                // Cancellation must stop the idle socket without a further pull.
                timeout(Duration::from_secs(7), server).await???;
                let failure = body
                    .next_chunk()
                    .await
                    .ok_or("missing cancellation")?
                    .err()
                    .ok_or("cancelled body unexpectedly returned bytes")?;
                assert_eq!(
                    failure.kind(),
                    if mode == "cancel" {
                        ErrorKind::Cancelled
                    } else {
                        ErrorKind::DeadlineExceeded
                    }
                );
                assert_eq!(requests.load(Ordering::Acquire), 1);
                continue;
            }
            "limit" => {
                send.send(Bytes::from_static(b"12345")).await?;
                let failure = loop {
                    match body.next_chunk().await.ok_or("missing limit failure")? {
                        Ok(_) => {}
                        Err(failure) => break failure,
                    }
                };
                assert_eq!(
                    failure.kind(),
                    ErrorKind::LimitExceeded(LimitKind::ResponseBodyBytes)
                );
            }
            _ => unreachable!(),
        }
        timeout(Duration::from_secs(7), server).await???;
        assert_eq!(requests.load(Ordering::Acquire), 1);
    }
    Ok(())
}

#[derive(Debug)]
struct InfallibleResponse;

impl std::fmt::Display for InfallibleResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("infallible response")
    }
}

impl Error for InfallibleResponse {}

#[derive(Default)]
struct ExchangeLog(std::sync::Mutex<Vec<String>>);

impl ExchangeLog {
    fn push(&self, value: String) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(value);
    }
    fn values(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

struct RecordingObserver(Arc<ExchangeLog>);
struct RecordingExchange(Arc<ExchangeLog>);

impl plakous_http::ExchangeObserver for RecordingObserver {
    fn start(&self, body: &[u8]) -> Option<Box<dyn plakous_http::ExchangeObservation>> {
        self.0
            .push(format!("request:{}", String::from_utf8_lossy(body)));
        Some(Box::new(RecordingExchange(Arc::clone(&self.0))))
    }
}

impl plakous_http::ExchangeObservation for RecordingExchange {
    fn response_head(&mut self, status: u16) {
        self.0.push(format!("head:{status}"));
    }
    fn response_chunk(&mut self, bytes: &[u8]) {
        self.0
            .push(format!("chunk:{}", String::from_utf8_lossy(bytes)));
    }
    fn terminal(&mut self, terminal: &plakous_http::ExchangeTerminal) {
        self.0.push(format!("terminal:{terminal:?}"));
    }
}

#[tokio::test]
async fn observation_sees_bounded_body_bytes_without_headers_and_one_terminal() -> TestResult {
    let (config, origin, server) = responding_server(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nX-Secret: response-secret\r\n\r\n2\r\nab\r\n2\r\ncd\r\n0\r\n\r\n".to_vec(),
    ).await?;
    let log = Arc::new(ExchangeLog::default());
    let limits = Limits {
        max_response_chunk_bytes: 1,
        ..Limits::default()
    };
    let client = make_client(config, limits.clone())?;
    let identity = client.behavior_identity();
    let client = client.with_observer(Some(Arc::new(RecordingObserver(Arc::clone(&log)))));
    assert_eq!(identity, client.behavior_identity());
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::AUTHORIZATION,
        HeaderValue::from_static("Bearer synthetic-secret"),
    );
    let request = make_request(
        origin,
        Method::POST,
        "/secret-path",
        headers,
        Bytes::from_static(b"payload"),
        &limits,
    )?;
    let mut body = client.execute(request, control()).await?.into_body();
    let mut bytes = Vec::new();
    while let Some(chunk) = body.next_chunk().await {
        bytes.extend_from_slice(&chunk?);
    }
    assert_eq!(bytes, b"abcd");
    assert!(body.next_chunk().await.is_none());
    body.close().await?;
    assert_eq!(
        log.values(),
        [
            "request:payload",
            "head:200",
            "chunk:a",
            "chunk:b",
            "chunk:c",
            "chunk:d",
            "terminal:Complete"
        ]
    );
    assert_eq!(server.await??.1, 1);
    Ok(())
}

#[tokio::test]
async fn observation_records_pre_dispatch_failure_and_early_body_end() -> TestResult {
    for terminal in ["Abandoned", "ClosedEarly", "ProtocolComplete"] {
        let (config, origin, server) =
            responding_server(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nbody".to_vec()).await?;
        let limits = Limits::default();
        let log = Arc::new(ExchangeLog::default());
        let client = make_client(config, limits.clone())?
            .with_observer(Some(Arc::new(RecordingObserver(Arc::clone(&log)))));
        let request = make_request(
            origin,
            Method::GET,
            "/",
            HeaderMap::new(),
            Bytes::new(),
            &limits,
        )?;
        let body = client.execute(request, control()).await?.into_body();
        match terminal {
            "ProtocolComplete" => body.close_after_protocol_terminal().await?,
            "ClosedEarly" => body.close().await?,
            _ => drop(body),
        }
        let values = log.values();
        assert_eq!(values.len(), 3);
        assert_eq!(values[2], format!("terminal:{terminal}"));
        assert_eq!(server.await??.1, 1);
    }
    let limits = Limits::default();
    let log = Arc::new(ExchangeLog::default());
    let client = Client::new(limits.clone())?
        .with_observer(Some(Arc::new(RecordingObserver(Arc::clone(&log)))));
    let request = make_request(
        Origin::parse("https://127.0.0.1:1")?,
        Method::GET,
        "/",
        HeaderMap::new(),
        Bytes::new(),
        &limits,
    )?;
    let control = control();
    control.cancellation().cancel();
    assert!(client.execute(request, control).await.is_err());
    let values = log.values();
    assert_eq!(values.len(), 2);
    assert!(values[1].contains("NotStarted"));
    assert!(values[1].contains("Cancelled"));
    Ok(())
}

struct PanickingObserver;
impl plakous_http::ExchangeObserver for PanickingObserver {
    fn start(&self, _: &[u8]) -> Option<Box<dyn plakous_http::ExchangeObservation>> {
        panic!("synthetic observer panic");
    }
}

struct RequiredCapture {
    fail_at: u8,
    failed: bool,
}

impl plakous_http::ExchangeObserver for RequiredCapture {
    fn required(&self) -> bool {
        true
    }

    fn start(&self, _: &[u8]) -> Option<Box<dyn plakous_http::ExchangeObservation>> {
        if self.fail_at == 0 {
            return None;
        }
        Some(Box::new(Self {
            fail_at: self.fail_at,
            failed: false,
        }))
    }
}

impl plakous_http::ExchangeObservation for RequiredCapture {
    fn failed(&self) -> bool {
        self.failed
    }
    fn response_head(&mut self, _: u16) {
        self.failed |= self.fail_at == 1;
    }
    fn response_chunk(&mut self, _: &[u8]) {
        self.failed |= self.fail_at == 2;
    }
    fn terminal(&mut self, _: &plakous_http::ExchangeTerminal) {
        self.failed |= self.fail_at == 3;
    }
}

#[tokio::test]
async fn required_capture_refusal_prevents_physical_dispatch() -> TestResult {
    let endpoint = make_endpoint(&[b"http/1.1"]).await?;
    let limits = Limits::default();
    let client = make_client(endpoint.client_config, limits.clone())?.with_observer(Some(
        Arc::new(RequiredCapture {
            fail_at: 0,
            failed: false,
        }),
    ));
    let request = make_request(
        endpoint.origin,
        Method::POST,
        "/",
        HeaderMap::new(),
        Bytes::from_static(b"private request"),
        &limits,
    )?;
    let failure = client
        .execute(request, control())
        .await
        .err()
        .ok_or_else(|| io::Error::other("uncaptured dispatch unexpectedly succeeded"))?;
    assert_eq!(failure.kind(), ErrorKind::Internal);
    assert_eq!(failure.transmission(), TransmissionFact::NotStarted);
    assert!(
        timeout(Duration::from_millis(100), endpoint.listener.accept())
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn required_capture_failure_cannot_deliver_uncaptured_bytes_or_successful_eof() -> TestResult
{
    for fail_at in 1..=3 {
        let (config, origin, server) =
            responding_server(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nbody".to_vec()).await?;
        let limits = Limits::default();
        let client =
            make_client(config, limits.clone())?.with_observer(Some(Arc::new(RequiredCapture {
                fail_at,
                failed: false,
            })));
        let request = make_request(
            origin,
            Method::GET,
            "/",
            HeaderMap::new(),
            Bytes::new(),
            &limits,
        )?;
        let failure = if fail_at == 1 {
            client
                .execute(request, control())
                .await
                .err()
                .ok_or_else(|| {
                    io::Error::other("uncaptured response head unexpectedly succeeded")
                })?
        } else {
            let mut body = client.execute(request, control()).await?.into_body();
            if fail_at == 3 {
                assert_eq!(required_chunk(&mut body).await?, "body");
            }
            let failure = body
                .next_chunk()
                .await
                .ok_or("missing capture failure")?
                .err()
                .ok_or_else(|| io::Error::other("uncaptured response unexpectedly succeeded"))?;
            assert!(
                body.next_chunk().await.is_none(),
                "error must be returned once"
            );
            failure
        };
        assert_eq!(failure.kind(), ErrorKind::Internal);
        assert_eq!(failure.transmission(), TransmissionFact::MayHaveTransmitted);
        assert_eq!(server.await??.1, 1);
    }
    Ok(())
}

#[tokio::test]
async fn required_terminal_failure_is_not_masked_by_a_truncated_refusal() -> TestResult {
    for fail_at in [3, 4] {
        let (config, origin, server) = responding_server(
            b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 8\r\n\r\nbody".to_vec(),
        )
        .await?;
        let limits = Limits::default();
        let client =
            make_client(config, limits.clone())?.with_observer(Some(Arc::new(RequiredCapture {
                fail_at,
                failed: false,
            })));
        let request = make_request(
            origin,
            Method::GET,
            "/",
            HeaderMap::new(),
            Bytes::new(),
            &limits,
        )?;
        let mut body = client.execute(request, control()).await?.into_body();
        let failure = loop {
            match body
                .next_chunk()
                .await
                .ok_or("truncated refusal unexpectedly completed")?
            {
                Ok(_) => {}
                Err(failure) => break failure,
            }
        };
        assert_eq!(failure.kind() == ErrorKind::Internal, fail_at == 3);
        assert_eq!(failure.phase(), Phase::ResponseBody);
        assert_eq!(failure.transmission(), TransmissionFact::MayHaveTransmitted);
        assert!(body.next_chunk().await.is_none());
        assert_eq!(server.await??.1, 1);
    }
    Ok(())
}

#[tokio::test]
async fn observer_panic_does_not_change_exchange_or_retry() -> TestResult {
    let (config, origin, server) =
        responding_server(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_vec()).await?;
    let limits = Limits::default();
    let client =
        make_client(config, limits.clone())?.with_observer(Some(Arc::new(PanickingObserver)));
    let request = make_request(
        origin,
        Method::GET,
        "/",
        HeaderMap::new(),
        Bytes::new(),
        &limits,
    )?;
    let mut body = client.execute(request, control()).await?.into_body();
    assert_eq!(required_chunk(&mut body).await?, "ok");
    assert!(body.next_chunk().await.is_none());
    assert_eq!(server.await??.1, 1);
    Ok(())
}

#[tokio::test]
async fn observation_body_failure_remains_failure_after_subsequent_poll() -> TestResult {
    let (config, origin, server) =
        responding_server(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nshort".to_vec()).await?;
    let limits = Limits::default();
    let log = Arc::new(ExchangeLog::default());
    let client = make_client(config, limits.clone())?
        .with_observer(Some(Arc::new(RecordingObserver(Arc::clone(&log)))));
    let request = make_request(
        origin,
        Method::GET,
        "/",
        HeaderMap::new(),
        Bytes::new(),
        &limits,
    )?;
    let mut body = client.execute(request, control()).await?.into_body();
    let mut failed = false;
    while let Some(chunk) = body.next_chunk().await {
        if chunk.is_err() {
            failed = true;
            break;
        }
    }
    assert!(failed);
    assert!(body.next_chunk().await.is_none());
    body.close().await?;
    let values = log.values();
    let terminals: Vec<_> = values
        .iter()
        .filter(|value| value.starts_with("terminal:"))
        .collect();
    assert_eq!(terminals.len(), 1);
    assert!(terminals[0].contains("Failed"));
    assert!(terminals[0].contains("ResponseBody"));
    assert_eq!(server.await??.1, 1);
    Ok(())
}

#[test]
fn loopback_http_origins_are_exactly_the_two_loopback_literals() -> TestResult {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    let v4 = Origin::loopback_http(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080)?;
    assert!(v4.is_loopback_http());
    assert_eq!(v4.host(), "127.0.0.1");
    assert_eq!(v4.binding_authority(), "127.0.0.1:8080");
    let v6 = Origin::loopback_http(IpAddr::V6(Ipv6Addr::LOCALHOST), 3000)?;
    assert!(v6.is_loopback_http());
    assert_eq!(v6.binding_authority(), "[::1]:3000");
    for refused in [
        "127.0.0.2",
        "0.0.0.0",
        "10.0.0.1",
        "8.8.8.8",
        "::",
        "::ffff:127.0.0.1",
        "fe80::1",
    ] {
        let address = refused.parse::<IpAddr>()?;
        assert!(Origin::loopback_http(address, 8080).is_err(), "{refused}");
    }
    assert!(Origin::loopback_http(IpAddr::V4(Ipv4Addr::LOCALHOST), 0).is_err());
    for refused in [
        "http://127.0.0.1:8080",
        "http://[::1]:8080",
        "http://localhost:8080",
    ] {
        assert!(Origin::parse(refused).is_err(), "{refused}");
    }
    let https = Origin::parse("https://127.0.0.1:8080")?;
    assert!(!https.is_loopback_http());
    assert_ne!(https, v4);
    Ok(())
}

#[tokio::test]
async fn loopback_http_origin_exchanges_plain_http1() -> TestResult {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await?;
        stream.shutdown().await?;
        Ok::<_, TestError>(request)
    });
    let origin = Origin::loopback_http(std::net::IpAddr::from([127, 0, 0, 1]), port)?;
    let limits = Limits::default();
    let client = Client::new(limits.clone())?;
    let request = make_request(
        origin,
        Method::GET,
        "/mcp",
        HeaderMap::new(),
        Bytes::new(),
        &limits,
    )?;
    let response = client.execute(request, control()).await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.version(), Version::HTTP_11);
    let mut body = response.into_body();
    assert_eq!(required_chunk(&mut body).await?, Bytes::from_static(b"ok"));
    let request = String::from_utf8(server.await??)?;
    assert!(request.starts_with("GET /mcp HTTP/1.1\r\n"), "{request}");
    assert!(
        request
            .to_ascii_lowercase()
            .contains(&format!("host: 127.0.0.1:{port}\r\n")),
        "{request}"
    );
    Ok(())
}

#[tokio::test]
async fn an_address_policy_refuses_a_loopback_origin_before_connecting() -> TestResult {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let client =
        Client::new(Limits::default())?.with_address_policy(Some(AddressPolicy::public_only()));
    assert_eq!(client.address_policy(), Some(AddressPolicy::public_only()));
    for origin in [
        Origin::loopback_http(std::net::IpAddr::from([127, 0, 0, 1]), port)?,
        Origin::parse(&format!("https://127.0.0.1:{port}"))?,
        Origin::parse(&format!("https://localhost:{port}"))?,
    ] {
        let failure = client
            .probe(&origin, control())
            .await
            .err()
            .ok_or("loopback must be refused")?;
        assert_eq!(failure.phase(), Phase::Resolution);
        assert_eq!(failure.kind(), ErrorKind::AddressRefused);
    }
    assert!(
        timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err(),
        "nothing may connect"
    );
    Ok(())
}
