//! Large request bodies through a TLS peer that reads slowly.
//!
//! The peer authenticates every record (Rustls answers a record it cannot
//! authenticate with alert 20, `bad_record_mac`) and checks every body byte
//! against a position- and request-dependent pattern, so a corrupted, lost,
//! duplicated or reordered record or plaintext byte fails the test. Uploads
//! run on fresh and reused pooled connections, with and without an observer,
//! with peer-initiated TLS 1.3 key updates during the body and while the
//! connection idles in the pool, and around cancellation and write deadlines.

use std::error::Error;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use plakous_http::{
    Bytes, CancellationToken, Client, ConnectionUse, ErrorKind, ExchangeControl,
    ExchangeObservation, ExchangeObserver, ExchangeTerminal, HeaderMap, Limits, Method, Origin,
    Request, StatusCode,
};
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, ReadBuf};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::task::JoinHandle;
use tokio_rustls::server::TlsStream;
use tokio_rustls::{TlsAcceptor, TlsConnector};

type TestError = Box<dyn Error + Send + Sync>;
type TestResult<T = ()> = Result<T, TestError>;

const KIB: usize = 1_024;
const MIB: usize = 1_024 * KIB;

/// How the peer drains a request body.
#[derive(Clone, Copy, Debug)]
enum Pace {
    /// Reads of 1 B to 17 KiB, sizes from a fixed sequence, yielding
    /// between reads, so reads split records at every offset.
    Trickle,
    /// 64 KiB reads with a 2 ms pause after each.
    Bursts,
    /// Free reads, stalling `stall` after every 512 KiB.
    Stalls { stall: Duration },
}

#[derive(Clone, Copy, Debug)]
struct Peer {
    pace: Pace,
    /// Sends a TLS 1.3 `KeyUpdate` (update requested) halfway through every
    /// body and again after every response, while the client idles.
    key_update: bool,
}

/// How one peer connection ended.
#[derive(Debug)]
#[expect(
    dead_code,
    reason = "fields are read through Debug in failure messages"
)]
enum Ended {
    /// Clean end after this many verified requests.
    Clean(usize),
    /// The client went away mid-request (cancellation or a deadline).
    Abandoned(io::ErrorKind),
    /// A record failed to authenticate or another TLS error.
    Tls(String),
    /// A plaintext body byte differed from its pattern.
    Mismatch { seed: u64, offset: usize },
    /// Anything else the peer could not parse.
    Malformed(String),
}

impl Ended {
    fn is_corruption(&self) -> bool {
        matches!(
            self,
            Self::Tls(_) | Self::Mismatch { .. } | Self::Malformed(_)
        )
    }
}

struct Server {
    client: ClientConfig,
    origin: Origin,
    ended: Arc<Mutex<Vec<Ended>>>,
    served: Arc<AtomicUsize>,
    _task: JoinHandle<()>,
}

fn configs(
    versions: &[&'static rustls::SupportedProtocolVersion],
) -> TestResult<(ServerConfig, ClientConfig)> {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["127.0.0.1".to_owned()])?;
    let certificate = cert.der().clone();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
    let mut server = ServerConfig::builder_with_provider(Arc::clone(&provider))
        .with_protocol_versions(versions)?
        .with_no_client_auth()
        .with_single_cert(vec![certificate.clone()], key)?;
    server.alpn_protocols = vec![b"http/1.1".to_vec()];
    let mut roots = RootCertStore::empty();
    roots.add(CertificateDer::from(certificate.to_vec()))?;
    let client = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(versions)?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok((server, client))
}

/// A listener whose accepted sockets have a small receive buffer, so a slow
/// reader pushes back on the client's writes after a few records.
fn small_window_listener() -> TestResult<TcpListener> {
    let socket = TcpSocket::new_v4()?;
    socket.set_recv_buffer_size(32 * 1_024)?;
    socket.bind("127.0.0.1:0".parse()?)?;
    Ok(socket.listen(64)?)
}

impl Server {
    fn start(peer: Peer) -> TestResult<Self> {
        Self::start_with(peer, rustls::DEFAULT_VERSIONS)
    }

    fn start_with(
        peer: Peer,
        versions: &[&'static rustls::SupportedProtocolVersion],
    ) -> TestResult<Self> {
        let (server, client) = configs(versions)?;
        let listener = small_window_listener()?;
        let origin = Origin::parse(&format!("https://{}", listener.local_addr()?))?;
        let acceptor = TlsAcceptor::from(Arc::new(server));
        let ended = Arc::new(Mutex::new(Vec::new()));
        let requests = Arc::new(AtomicUsize::new(0));
        let record = Arc::clone(&ended);
        let count = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                let record = Arc::clone(&record);
                let count = Arc::clone(&count);
                tokio::spawn(async move {
                    let outcome = match acceptor.accept(stream).await {
                        Ok(mut stream) => {
                            let outcome = serve(&mut stream, peer, &count).await;
                            if outcome.is_corruption() {
                                drain(stream.get_mut().0).await;
                            }
                            outcome
                        }
                        Err(error) => classify(&error),
                    };
                    record
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(outcome);
                });
            }
        });
        Ok(Self {
            client,
            origin,
            ended,
            served: requests,
            _task: task,
        })
    }

    fn client(&self, observed: bool) -> TestResult<Client> {
        let client = Client::with_tls_config(Limits::default(), self.client.clone())?;
        Ok(if observed {
            client.with_observer(Some(Arc::new(CopyingObserver)))
        } else {
            client
        })
    }

    /// Waits until the peer recorded `connections` ends, then returns them.
    async fn ended(&self, connections: usize) -> TestResult<Vec<String>> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            {
                let ended = self.ended.lock().unwrap_or_else(PoisonError::into_inner);
                if ended.len() >= connections {
                    return Ok(ended.iter().map(|end| format!("{end:?}")).collect());
                }
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other("peer connections did not end").into());
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn corruption(&self) -> Vec<String> {
        self.ended
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|end| end.is_corruption())
            .map(|end| format!("{end:?}"))
            .collect()
    }

    fn served(&self) -> usize {
        self.served.load(Ordering::SeqCst)
    }
}

fn classify(error: &io::Error) -> Ended {
    match error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<rustls::Error>())
    {
        Some(tls) => Ended::Tls(format!("{tls:?}")),
        None => Ended::Abandoned(error.kind()),
    }
}

fn pattern(seed: u64, offset: usize) -> u8 {
    // 251 is prime to every record size, so a shifted record never matches.
    let value = (offset as u64)
        .wrapping_mul(31)
        .wrapping_add(seed.wrapping_mul(7_919))
        % 251;
    u8::try_from(value).unwrap_or(0)
}

fn body(seed: u64, length: usize) -> Bytes {
    (0..length).map(|offset| pattern(seed, offset)).collect()
}

/// Read sizes the trickle pace cycles through: 1 B up to just over a record.
fn trickle_size(step: usize) -> usize {
    const SIZES: [usize; 8] = [1, 5, 517, 4_093, 16_384, 16_385, 9_000, 17_400];
    SIZES[step % SIZES.len()]
}

/// Reads what the client still sends after the peer rejected a record, so
/// closing does not reset the connection before the client reads the alert.
async fn drain(tcp: &mut TcpStream) {
    let mut buffer = vec![0_u8; 64 * KIB];
    while let Ok(Ok(read)) =
        tokio::time::timeout(Duration::from_millis(200), tcp.read(&mut buffer)).await
    {
        if read == 0 {
            break;
        }
    }
}

async fn serve(stream: &mut TlsStream<TcpStream>, peer: Peer, counter: &AtomicUsize) -> Ended {
    let mut served = 0;
    loop {
        let head = match read_head(stream).await {
            Ok(Some(head)) => head,
            Ok(None) => return Ended::Clean(served),
            Err(end) => return end,
        };
        if let Err(end) = read_body(stream, peer, head).await {
            return end;
        }
        served += 1;
        counter.fetch_add(1, Ordering::SeqCst);
        let response = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok";
        if let Err(error) = stream.write_all(response).await {
            return classify(&error);
        }
        if let Err(error) = stream.flush().await {
            return classify(&error);
        }
        if peer.key_update
            && let Err(end) = key_update(stream).await
        {
            return end;
        }
    }
}

struct Head {
    seed: u64,
    length: usize,
    /// Body bytes read with the head.
    early: Vec<u8>,
}

async fn read_head(stream: &mut TlsStream<TcpStream>) -> Result<Option<Head>, Ended> {
    let mut head = Vec::new();
    let mut buffer = [0_u8; 1_024];
    let end = loop {
        let read = match stream.read(&mut buffer).await {
            Ok(read) => read,
            // A client that drops its connection sends no close_notify.
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => 0,
            Err(error) => return Err(classify(&error)),
        };
        if read == 0 {
            return if head.is_empty() {
                Ok(None)
            } else {
                Err(Ended::Abandoned(io::ErrorKind::UnexpectedEof))
            };
        }
        head.extend_from_slice(&buffer[..read]);
        if let Some(end) = head.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
        if head.len() > 64 * 1_024 {
            return Err(Ended::Malformed("head exceeded 64 KiB".to_owned()));
        }
    };
    let text = std::str::from_utf8(&head[..end])
        .map_err(|_| Ended::Malformed("head is not UTF-8".to_owned()))?;
    let seed = text
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("POST /upload/"))
        .and_then(|rest| rest.split(' ').next())
        .and_then(|seed| seed.parse().ok())
        .ok_or_else(|| Ended::Malformed(format!("request line {:?}", text.lines().next())))?;
    let length = text
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .ok_or_else(|| Ended::Malformed("no content-length".to_owned()))?;
    Ok(Some(Head {
        seed,
        length,
        early: head[end..].to_vec(),
    }))
}

async fn read_body(stream: &mut TlsStream<TcpStream>, peer: Peer, head: Head) -> Result<(), Ended> {
    let Head {
        seed,
        length,
        early,
    } = head;
    let verify = |bytes: &[u8], at: usize| {
        bytes
            .iter()
            .enumerate()
            .find(|(index, byte)| **byte != pattern(seed, at + index))
            .map_or(Ok(()), |(index, _)| {
                Err(Ended::Mismatch {
                    seed,
                    offset: at + index,
                })
            })
    };
    if early.len() > length {
        return Err(Ended::Malformed("bytes past the body".to_owned()));
    }
    verify(&early, 0)?;
    let mut received = early.len();
    let mut buffer = vec![0_u8; 64 * 1_024];
    let mut updated = !peer.key_update;
    let mut step = 0;
    while received < length {
        let want = match peer.pace {
            Pace::Trickle => trickle_size(step),
            Pace::Bursts | Pace::Stalls { .. } => buffer.len(),
        }
        .min(length - received);
        step += 1;
        let read = stream
            .read(&mut buffer[..want])
            .await
            .map_err(|error| classify(&error))?;
        if read == 0 {
            return Err(Ended::Abandoned(io::ErrorKind::UnexpectedEof));
        }
        verify(&buffer[..read], received)?;
        let before = received;
        received += read;
        if !updated && received >= length / 2 {
            key_update(stream).await?;
            updated = true;
        }
        match peer.pace {
            Pace::Trickle => tokio::task::yield_now().await,
            Pace::Bursts => tokio::time::sleep(Duration::from_millis(2)).await,
            Pace::Stalls { stall } => {
                if before / (512 * KIB) != received / (512 * KIB) {
                    tokio::time::sleep(stall).await;
                }
            }
        }
    }
    Ok(())
}

async fn key_update(stream: &mut TlsStream<TcpStream>) -> Result<(), Ended> {
    stream
        .get_mut()
        .1
        .refresh_traffic_keys()
        .map_err(|error| Ended::Tls(format!("{error:?}")))?;
    stream.flush().await.map_err(|error| classify(&error))
}

/// Copies every request body before dispatch and blocks its worker thread
/// briefly, as a synchronous capture sink would.
struct CopyingObserver;

struct CopyingObservation;

impl ExchangeObserver for CopyingObserver {
    fn start(&self, request_body: &[u8]) -> Option<Box<dyn ExchangeObservation>> {
        let copy = request_body.to_vec();
        std::thread::sleep(Duration::from_millis(1));
        drop(copy);
        Some(Box::new(CopyingObservation))
    }
}

impl ExchangeObservation for CopyingObservation {
    fn response_head(&mut self, _status: u16) {}
    fn response_chunk(&mut self, _bytes: &[u8]) {}
    fn terminal(&mut self, _terminal: &ExchangeTerminal) {}
}

fn upload_request(origin: &Origin, seed: u64, length: usize) -> TestResult<Request> {
    let mut headers = HeaderMap::new();
    headers.insert(
        plakous_http::HeaderName::from_static("content-type"),
        plakous_http::HeaderValue::from_static("application/octet-stream"),
    );
    Ok(Request::new(
        origin.clone(),
        Method::POST,
        &format!("/upload/{seed}"),
        headers,
        body(seed, length),
        &Limits::default(),
    )?)
}

/// Uploads one body and reads the whole response, so a pooled connection
/// returns to its pool.
async fn upload(
    client: &Client,
    origin: &Origin,
    seed: u64,
    length: usize,
    control: ExchangeControl,
) -> TestResult<Option<ConnectionUse>> {
    let response = client
        .execute(upload_request(origin, seed, length)?, control)
        .await?;
    if response.status() != StatusCode::OK {
        return Err(io::Error::other(format!("status {}", response.status())).into());
    }
    let connection = response.connection_use();
    let mut body = response.into_body();
    let mut received = Vec::new();
    while let Some(chunk) = body.next_chunk().await {
        received.extend_from_slice(&chunk?);
    }
    if received != b"ok" {
        return Err(io::Error::other("response body differs").into());
    }
    Ok(connection)
}

fn control() -> ExchangeControl {
    ExchangeControl::new(Instant::now() + Duration::from_secs(30))
}

/// Runs every size on an unpooled client, then on one pooled client, so all
/// pooled uploads after the first reuse one connection.
async fn sizes_on_fresh_and_reused(
    peer: Peer,
    observed: bool,
    sizes: &[usize],
    seed: &mut u64,
) -> TestResult<(usize, usize)> {
    let server = Server::start(peer)?;
    let unpooled = server.client(observed)?;
    let pooled = unpooled.pooled();
    let mut reused = 0;
    for &size in sizes {
        *seed += 1;
        upload(&unpooled, &server.origin, *seed, size, control()).await?;
    }
    for &size in sizes {
        *seed += 1;
        if upload(&pooled, &server.origin, *seed, size, control()).await?
            == Some(ConnectionUse::Reused)
        {
            reused += 1;
        }
    }
    drop(pooled);
    drop(unpooled);
    let ended = server.ended(sizes.len() + 1).await?;
    let corruption = server.corruption();
    assert!(
        corruption.is_empty(),
        "{peer:?} observed={observed}: {corruption:?}; all ends {ended:?}"
    );
    assert_eq!(server.served(), 2 * sizes.len(), "{ended:?}");
    assert_eq!(reused, sizes.len() - 1, "{peer:?} observed={observed}");
    Ok((2 * sizes.len(), reused))
}

/// Concurrent pooled uploads, several rounds, so connections are reused
/// while others are mid-body.
async fn concurrent_pooled(peer: Peer, sizes: &[usize], seed: &mut u64) -> TestResult<usize> {
    let server = Server::start(peer)?;
    let pooled = server.client(true)?.pooled();
    let mut uploads = 0;
    for _round in 0..3 {
        let mut tasks = Vec::new();
        for &size in sizes {
            *seed += 1;
            let client = pooled.clone();
            let origin = server.origin.clone();
            let seed = *seed;
            tasks.push(tokio::spawn(async move {
                upload(&client, &origin, seed, size, control()).await
            }));
        }
        for task in tasks {
            task.await??;
            uploads += 1;
        }
    }
    drop(pooled);
    let corruption = server.corruption();
    assert!(corruption.is_empty(), "{peer:?}: {corruption:?}");
    Ok(uploads)
}

/// Cancels, drops or deadlines a pooled upload at many points of its body
/// write, then retries the same request and sends another, as the product's
/// retry paths do. Every connection must stay authentic.
async fn interrupted_then_retried(peer: Peer, seed: &mut u64) -> TestResult<usize> {
    let server = Server::start(peer)?;
    let pooled = server.client(true)?.pooled();
    *seed += 1;
    upload(&pooled, &server.origin, *seed, 64 * KIB, control()).await?;
    let mut interrupted = 0;
    for (index, after) in [0_u64, 1, 3, 7, 15, 30, 60].into_iter().enumerate() {
        *seed += 1;
        let after = Duration::from_millis(after);
        let request = upload_request(&server.origin, *seed, 3 * MIB)?;
        match index % 3 {
            0 => {
                let cancellation = CancellationToken::new();
                let cancel = cancellation.clone();
                let canceller = tokio::spawn(async move {
                    tokio::time::sleep(after).await;
                    cancel.cancel();
                });
                let control = ExchangeControl::with_cancellation(
                    Instant::now() + Duration::from_secs(30),
                    cancellation,
                );
                let result = pooled.execute(request, control).await;
                canceller.await?;
                if let Err(failure) = result {
                    assert_eq!(failure.kind(), ErrorKind::Cancelled, "{failure:?}");
                    interrupted += 1;
                }
            }
            1 => {
                if tokio::time::timeout(after, pooled.execute(request, control()))
                    .await
                    .is_err()
                {
                    interrupted += 1;
                }
            }
            _ => {
                let control = ExchangeControl::new(Instant::now() + after);
                if let Err(failure) = pooled.execute(request, control).await {
                    assert!(
                        matches!(
                            failure.kind(),
                            ErrorKind::DeadlineExceeded | ErrorKind::Cancelled
                        ),
                        "{failure:?}"
                    );
                    interrupted += 1;
                }
            }
        }
        // The retry replays the same bytes, then the next request follows.
        upload(&pooled, &server.origin, *seed, 3 * MIB, control()).await?;
        *seed += 1;
        upload(&pooled, &server.origin, *seed, MIB, control()).await?;
    }
    drop(pooled);
    let corruption = server.corruption();
    assert!(corruption.is_empty(), "{peer:?}: {corruption:?}");
    Ok(interrupted)
}

const PACES: [Pace; 3] = [
    Pace::Trickle,
    Pace::Bursts,
    Pace::Stalls {
        stall: Duration::from_millis(40),
    },
];

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn large_bodies_stay_authentic_through_a_slow_tls_reader() -> TestResult {
    let mut seed = 0;
    for pace in PACES {
        for key_update in [false, true] {
            let peer = Peer { pace, key_update };
            sizes_on_fresh_and_reused(peer, key_update, &[100 * KIB, MIB, 3 * MIB], &mut seed)
                .await?;
        }
    }
    concurrent_pooled(
        Peer {
            pace: Pace::Trickle,
            key_update: true,
        },
        &[512 * KIB, MIB, 2 * MIB, 3 * MIB],
        &mut seed,
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupted_uploads_leave_later_connections_authentic() -> TestResult {
    let mut seed = 1_000;
    let interrupted = interrupted_then_retried(
        Peer {
            pace: Pace::Stalls {
                stall: Duration::from_millis(40),
            },
            key_update: true,
        },
        &mut seed,
    )
    .await?;
    assert!(interrupted > 0, "no upload was interrupted mid-write");
    Ok(())
}

/// The full reproducer matrix: every size, pace, key-update mode and
/// observer setting, TLS 1.3 and 1.2, plus concurrency and interruption.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "reproducer matrix: about a gigabyte of TLS; run explicitly"]
async fn tls_integrity_reproducer_matrix() -> TestResult {
    let sizes = [100 * KIB, 512 * KIB, MIB, 2 * MIB, 3 * MIB];
    let mut seed = 10_000;
    let mut exchanges = 0;
    let mut reused = 0;
    let mut interrupted = 0;
    for pace in PACES {
        for key_update in [false, true] {
            for observed in [false, true] {
                let peer = Peer { pace, key_update };
                let (count, again) =
                    sizes_on_fresh_and_reused(peer, observed, &sizes, &mut seed).await?;
                exchanges += count;
                reused += again;
            }
            let peer = Peer { pace, key_update };
            exchanges += concurrent_pooled(peer, &sizes, &mut seed).await?;
            interrupted += interrupted_then_retried(peer, &mut seed).await?;
            exchanges += 1 + 2 * 7;
        }
    }
    for pace in PACES {
        let server = Server::start_with(
            Peer {
                pace,
                key_update: false,
            },
            &[&rustls::version::TLS12],
        )?;
        let pooled = server.client(true)?.pooled();
        for &size in &sizes {
            seed += 1;
            upload(&pooled, &server.origin, seed, size, control()).await?;
            exchanges += 1;
        }
        drop(pooled);
        let corruption = server.corruption();
        assert!(corruption.is_empty(), "TLS 1.2 {pace:?}: {corruption:?}");
    }
    eprintln!(
        "reproducer matrix: {exchanges} complete exchanges, {reused} on reused connections, \
         {interrupted} interrupted mid-write, 0 corrupt"
    );
    Ok(())
}

/// Flips one ciphertext byte after `at` bytes, as a corrupting write path
/// would.
struct FlipOneByte {
    inner: TcpStream,
    written: usize,
    at: usize,
}

impl AsyncRead for FlipOneByte {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for FlipOneByte {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let start = self.written;
        let result = if (start..start + buf.len()).contains(&self.at) {
            let mut copy = buf.to_vec();
            copy[self.at - start] ^= 0x01;
            Pin::new(&mut self.inner).poll_write(cx, &copy)
        } else {
            Pin::new(&mut self.inner).poll_write(cx, buf)
        };
        if let Poll::Ready(Ok(written)) = result {
            self.written += written;
        }
        result
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Positive control: one flipped ciphertext bit mid-body makes this peer
/// reject the record with `bad_record_mac`, which the writer receives as
/// alert 20, the production signature.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_peer_rejects_a_corrupted_record_with_bad_record_mac() -> TestResult {
    let server = Server::start(Peer {
        pace: Pace::Bursts,
        key_update: false,
    })?;
    let address = server.origin.binding_authority().to_owned();
    let tcp = TcpStream::connect(address.as_str()).await?;
    let corrupting = FlipOneByte {
        inner: tcp,
        written: 0,
        at: 300 * KIB,
    };
    let mut tls = TlsConnector::from(Arc::new(server.client.clone()))
        .connect(ServerName::try_from("127.0.0.1")?, corrupting)
        .await?;
    let length = MIB;
    let head =
        format!("POST /upload/7 HTTP/1.1\r\nhost: {address}\r\ncontent-length: {length}\r\n\r\n");
    let written = async {
        tls.write_all(head.as_bytes()).await?;
        tls.write_all(&body(7, length)).await?;
        tls.flush().await?;
        let mut response = Vec::new();
        tls.read_to_end(&mut response).await
    }
    .await;
    let error = written.err().ok_or("the corrupted upload succeeded")?;
    let alert = error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<rustls::Error>());
    assert!(
        matches!(
            alert,
            Some(rustls::Error::AlertReceived(
                rustls::AlertDescription::BadRecordMac
            ))
        ),
        "{error:?}"
    );
    let ended = server.ended(1).await?;
    assert_eq!(ended, vec!["Tls(\"DecryptError\")".to_owned()]);
    Ok(())
}
