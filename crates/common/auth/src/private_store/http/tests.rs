//! Claimed HTTP refresh and quarantine regressions.

use crate::private_store::ClaimedHttpDisposition;
use crate::private_store::ClaimedHttpResponse;
use crate::private_store::PrivateCredentialStore;
use crate::private_store::PrivateStoreError;
use crate::private_store::platform;
use crate::private_store::{RotatedDirectory, create_private_file, load_raw_for_test};
use crate::{
    AccountBinding, CredentialGeneration, CredentialRecord, CredentialSlotId, CredentialStatus,
    SecretBytes, SubscriptionIntegrationProfileId,
};
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::sync::mpsc::RecvTimeoutError;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use plakous_http::{
    Bytes, CancellationToken, Client, ExchangeControl, Failure as HttpFailure, HeaderMap, Limits,
    Method, Origin, Request, StatusCode, TransmissionFact,
};
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::time::timeout;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

type ServerError = Box<dyn std::error::Error + Send + Sync>;
type ServerResult<T = ()> = Result<T, ServerError>;

struct HttpFixture {
    directory: TempDir,
    path: PathBuf,
    profile: SubscriptionIntegrationProfileId,
    slot: CredentialSlotId,
    account: AccountBinding,
}

impl HttpFixture {
    fn new() -> ServerResult<Self> {
        let directory = tempfile::tempdir()?;
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))?;
        Ok(Self {
            path: directory.path().join("credentials.json"),
            directory,
            profile: SubscriptionIntegrationProfileId::new("profile")?,
            slot: CredentialSlotId::new("primary")?,
            account: AccountBinding::new("opaque-account")?,
        })
    }

    fn store(&self) -> Result<PrivateCredentialStore, PrivateStoreError> {
        PrivateCredentialStore::open_slot(&self.path, self.profile.clone(), self.slot.clone())
    }

    fn record(
        &self,
        generation: u64,
        status: CredentialStatus,
        payload: &[u8],
    ) -> ServerResult<CredentialRecord> {
        Ok(CredentialRecord::new(
            self.profile.clone(),
            self.account.clone(),
            CredentialGeneration::new(generation)?,
            status,
            SecretBytes::new(payload.to_vec()),
        )?)
    }
}

struct Endpoint {
    listener: TcpListener,
    acceptor: TlsAcceptor,
    client_config: ClientConfig,
    origin: Origin,
}

async fn endpoint() -> ServerResult<Endpoint> {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])?;
    let certificate = cert.der().clone();
    let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut server_config = ServerConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(vec![certificate.clone()], private_key)?;
    server_config.alpn_protocols = vec![b"http/1.1".to_vec()];

    let mut roots = RootCertStore::empty();
    roots.add(CertificateDer::from(certificate.to_vec()))?;
    let client_config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    Ok(Endpoint {
        listener,
        acceptor: TlsAcceptor::from(Arc::new(server_config)),
        client_config,
        origin: Origin::parse(&format!("https://{address}"))?,
    })
}

fn client(config: ClientConfig, limits: Limits) -> ServerResult<Client> {
    Ok(Client::with_tls_config(limits, config)?)
}

fn request(origin: Origin, limits: &Limits, target: &str) -> ServerResult<Request> {
    Ok(Request::new(
        origin,
        Method::POST,
        target,
        HeaderMap::new(),
        Bytes::from_static(b"refresh-body"),
        limits,
    )?)
}

fn control() -> ExchangeControl {
    // Loopback TLS exchanges run concurrently with filesystem and
    // subprocess tests. Leave enough absolute-deadline headroom for a
    // loaded test host without changing any production timeout.
    ExchangeControl::new(Instant::now() + Duration::from_secs(30))
}

async fn read_request(stream: &mut TlsStream<TcpStream>) -> ServerResult<Vec<u8>> {
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

fn spawn_response_server(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    response: &'static [u8],
) -> tokio::task::JoinHandle<ServerResult<Vec<u8>>> {
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut stream = acceptor.accept(stream).await?;
        let captured = read_request(&mut stream).await?;
        stream.write_all(response).await?;
        stream.shutdown().await?;
        Ok(captured)
    })
}

async fn read_body(response: &mut ClaimedHttpResponse) -> ServerResult<Vec<u8>> {
    let mut collected = Vec::new();
    while let Some(chunk) = response.next_chunk().await {
        collected.extend_from_slice(&chunk?);
    }
    Ok(collected)
}

async fn successful_claimed_response(
    fixture: &HttpFixture,
    store: &PrivateCredentialStore,
    endpoint: Endpoint,
    target: &str,
) -> ServerResult<(
    ClaimedHttpResponse,
    tokio::task::JoinHandle<ServerResult<Vec<u8>>>,
)> {
    successful_claimed_response_with_control(fixture, store, endpoint, target, control()).await
}

async fn successful_claimed_response_with_control(
    fixture: &HttpFixture,
    store: &PrivateCredentialStore,
    endpoint: Endpoint,
    target: &str,
    control: ExchangeControl,
) -> ServerResult<(
    ClaimedHttpResponse,
    tokio::task::JoinHandle<ServerResult<Vec<u8>>>,
)> {
    let Endpoint {
        listener,
        acceptor,
        client_config,
        origin,
    } = endpoint;
    let server = spawn_response_server(
        listener,
        acceptor,
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
    );
    store.initialize(&fixture.record(1, CredentialStatus::Ready, b"old")?)?;
    let claim = store.begin_refresh(&store.load()?.ok_or("missing record")?)?;
    let limits = Limits::default();
    let client = client(client_config, limits.clone())?;
    let response = store
        .execute_claimed_http(claim, &client, request(origin, &limits, target)?, control)
        .await?;
    Ok((response, server))
}

#[tokio::test]
async fn successful_response_returns_exact_started_claim() -> ServerResult {
    let Endpoint {
        listener,
        acceptor,
        client_config,
        origin,
    } = endpoint().await?;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut stream = acceptor.accept(stream).await?;
        let captured = read_request(&mut stream).await?;
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nX-Test-Secret: response-secret\r\n\r\nok",
            )
            .await?;
        stream.shutdown().await?;
        Ok::<_, ServerError>(captured)
    });
    let fixture = HttpFixture::new()?;
    let store = fixture.store()?;
    store.initialize(&fixture.record(
        1,
        CredentialStatus::RefreshRequired,
        b"old-success-secret",
    )?)?;
    let fence = store.load()?.ok_or("missing initial record")?;
    let claim = store.begin_refresh(&fence)?;
    let claimed_fence = claim.fence();
    let limits = Limits::default();
    let client = client(client_config, limits.clone())?;
    let mut response = store
        .execute_claimed_http(
            claim,
            &client,
            request(origin, &limits, "/success?secret=request-secret")?,
            control(),
        )
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.refresh_fence(), claimed_fence);
    assert!(!response.body_completed_cleanly());
    let response_debug = format!("{response:?}");
    for secret in [
        "old-success-secret",
        "request-secret",
        "response-secret",
        fixture.path.to_string_lossy().as_ref(),
    ] {
        assert!(!response_debug.contains(secret));
    }

    assert_eq!(read_body(&mut response).await?, b"ok");
    assert!(response.body_completed_cleanly());
    let completed = store.complete_claimed_http(
        response,
        fixture.record(3, CredentialStatus::Ready, b"new-success-secret")?,
    )?;
    assert_eq!(completed.generation(), CredentialGeneration::new(3)?);
    assert!(String::from_utf8(server.await??)?.contains("refresh-body"));
    Ok(())
}

#[tokio::test]
async fn incomplete_success_response_cannot_complete_refresh() -> ServerResult {
    let fixture = HttpFixture::new()?;
    let store = fixture.store()?;
    let (response, server) =
        successful_claimed_response(&fixture, &store, endpoint().await?, "/incomplete").await?;
    let unknown = response.refresh_fence();
    assert_eq!(
        store.complete_claimed_http(
            response,
            fixture.record(3, CredentialStatus::Ready, b"must-not-install")?,
        ),
        Err(PrivateStoreError::HttpResponseBodyNotComplete)
    );
    assert_eq!(store.load()?.ok_or("quarantine was lost")?, unknown);
    assert!(String::from_utf8(server.await??)?.contains("refresh-body"));
    Ok(())
}

#[tokio::test]
async fn body_failure_never_becomes_clean_eof_or_completable() -> ServerResult {
    let Endpoint {
        listener,
        acceptor,
        client_config,
        origin,
    } = endpoint().await?;
    let server = spawn_response_server(
        listener,
        acceptor,
        b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nx",
    );
    let fixture = HttpFixture::new()?;
    let store = fixture.store()?;
    store.initialize(&fixture.record(1, CredentialStatus::Ready, b"old")?)?;
    let claim = store.begin_refresh(&store.load()?.ok_or("missing record")?)?;
    let unknown = claim.fence();
    let limits = Limits::default();
    let client = client(client_config, limits.clone())?;
    let mut response = store
        .execute_claimed_http(
            claim,
            &client,
            request(origin, &limits, "/truncated")?,
            control(),
        )
        .await?;
    let mut observed_failure = false;
    for _ in 0..3 {
        match response.next_chunk().await {
            Some(Ok(_)) => {}
            Some(Err(failure)) => {
                assert_eq!(failure.transmission(), TransmissionFact::MayHaveTransmitted);
                observed_failure = true;
                break;
            }
            None => break,
        }
    }
    assert!(observed_failure, "truncated response did not fail");
    assert_eq!(
        response.transport_failure().map(HttpFailure::phase),
        Some(plakous_http::Phase::ResponseBody)
    );
    assert!(response.next_chunk().await.is_none());
    assert!(!response.body_completed_cleanly());
    assert_eq!(
        store.complete_claimed_http(
            response,
            fixture.record(3, CredentialStatus::Ready, b"must-not-install")?,
        ),
        Err(PrivateStoreError::HttpResponseBodyNotComplete)
    );
    assert_eq!(store.load()?.ok_or("quarantine was lost")?, unknown);
    assert!(String::from_utf8(server.await??)?.contains("refresh-body"));
    Ok(())
}

#[tokio::test]
async fn cancelled_body_pull_permanently_prevents_completion() -> ServerResult {
    let Endpoint {
        listener,
        acceptor,
        client_config,
        origin,
    } = endpoint().await?;
    let (release_sender, release_receiver) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut stream = acceptor.accept(stream).await?;
        let captured = read_request(&mut stream).await?;
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n")
            .await?;
        let _ = release_receiver.await;
        drop(stream);
        Ok::<_, ServerError>(captured)
    });
    let fixture = HttpFixture::new()?;
    let store = fixture.store()?;
    store.initialize(&fixture.record(1, CredentialStatus::Ready, b"old")?)?;
    let claim = store.begin_refresh(&store.load()?.ok_or("missing record")?)?;
    let unknown = claim.fence();
    let limits = Limits::default();
    let client = client(client_config, limits.clone())?;
    let mut response = store
        .execute_claimed_http(
            claim,
            &client,
            request(origin, &limits, "/cancel-pull")?,
            control(),
        )
        .await?;
    {
        let pull = response.next_chunk();
        tokio::pin!(pull);
        tokio::select! {
            result = &mut pull => {
                return Err(io::Error::other(format!(
                    "held body pull completed unexpectedly: {result:?}"
                )).into());
            }
            () = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }
    assert!(!response.body_completed_cleanly());
    assert_eq!(
        store.complete_claimed_http(
            response,
            fixture.record(3, CredentialStatus::Ready, b"must-not-install")?,
        ),
        Err(PrivateStoreError::HttpResponseBodyNotComplete)
    );
    assert_eq!(store.load()?.ok_or("quarantine was lost")?, unknown);
    let _ = release_sender.send(());
    assert!(String::from_utf8(server.await??)?.contains("refresh-body"));
    Ok(())
}

#[tokio::test]
async fn successful_response_has_consuming_quarantine_terminal() -> ServerResult {
    let fixture = HttpFixture::new()?;
    let store = fixture.store()?;
    let (response, server) =
        successful_claimed_response(&fixture, &store, endpoint().await?, "/quarantine").await?;
    let unknown = response.refresh_fence();
    assert_eq!(store.quarantine_claimed_http(response)?, unknown);
    assert_eq!(store.load()?.ok_or("quarantine was lost")?, unknown);
    assert!(String::from_utf8(server.await??)?.contains("refresh-body"));
    Ok(())
}

#[tokio::test]
async fn retained_response_lock_blocks_mutation_until_terminal_consumption() -> ServerResult {
    let fixture = HttpFixture::new()?;
    let store = fixture.store()?;
    let (response, server) =
        successful_claimed_response(&fixture, &store, endpoint().await?, "/held-lock").await?;
    let unknown = response.refresh_fence();
    let unknown_generation = unknown.generation();
    let contender = fixture.store()?;
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let _ = sender.send(contender.logout(unknown_generation));
    });
    assert!(matches!(
        receiver.recv_timeout(Duration::from_millis(50)),
        Err(RecvTimeoutError::Timeout)
    ));
    assert_eq!(store.quarantine_claimed_http(response)?, unknown);
    assert_eq!(
        receiver.recv_timeout(Duration::from_secs(5))??,
        CredentialGeneration::new(3)?
    );
    worker.join().map_err(|_| "mutation worker panicked")?;
    assert_eq!(
        load_raw_for_test(&store)?
            .ok_or("missing tombstone")?
            .status(),
        CredentialStatus::Revoked
    );
    assert!(String::from_utf8(server.await??)?.contains("refresh-body"));
    Ok(())
}

#[tokio::test]
async fn replaced_lock_inode_cannot_authorize_terminal_write() -> ServerResult {
    let fixture = HttpFixture::new()?;
    let store = fixture.store()?;
    let (mut response, server) =
        successful_claimed_response(&fixture, &store, endpoint().await?, "/replaced-lock").await?;
    let unknown = response.refresh_fence();
    assert_eq!(read_body(&mut response).await?, b"ok");
    let displaced = fixture.directory.path().join("displaced-http-lock");
    fs::rename(store.lock_path(), &displaced)?;
    create_private_file(&store.lock_path())?;
    assert_eq!(
        store.complete_claimed_http(
            response,
            fixture.record(3, CredentialStatus::Ready, b"must-not-install")?,
        ),
        Err(PrivateStoreError::UnsafeLock)
    );
    assert_eq!(store.load()?.ok_or("quarantine was lost")?, unknown);
    assert!(String::from_utf8(server.await??)?.contains("refresh-body"));
    Ok(())
}

#[tokio::test]
async fn successful_response_has_consuming_revoke_terminal() -> ServerResult {
    let fixture = HttpFixture::new()?;
    let store = fixture.store()?;
    let (response, server) =
        successful_claimed_response(&fixture, &store, endpoint().await?, "/revoke").await?;
    assert_eq!(
        store.revoke_claimed_http(response)?,
        CredentialGeneration::new(3)?
    );
    let tombstone = load_raw_for_test(&store)?.ok_or("missing tombstone")?;
    assert_eq!(tombstone.generation(), CredentialGeneration::new(3)?);
    assert_eq!(tombstone.status(), CredentialStatus::Revoked);
    assert!(tombstone.payload().is_empty());
    assert!(String::from_utf8(server.await??)?.contains("refresh-body"));
    Ok(())
}

#[tokio::test]
async fn two_successes_cannot_mix_response_claim_or_store_authority() -> ServerResult {
    let first = HttpFixture::new()?;
    let second = HttpFixture::new()?;
    let first_store = first.store()?;
    let second_store = second.store()?;
    // Both response owners must coexist for the cross-store test. Give
    // the first absolute exchange enough headroom for the second
    // fixture's TLS and durable-store setup under a loaded test host.
    let (mut first_response, first_server) = successful_claimed_response_with_control(
        &first,
        &first_store,
        endpoint().await?,
        "/first",
        ExchangeControl::new(Instant::now() + Duration::from_secs(30)),
    )
    .await?;
    let (mut second_response, second_server) = successful_claimed_response_with_control(
        &second,
        &second_store,
        endpoint().await?,
        "/second",
        ExchangeControl::new(Instant::now() + Duration::from_secs(30)),
    )
    .await?;
    let first_unknown = first_response.refresh_fence();
    let second_unknown = second_response.refresh_fence();
    assert_eq!(read_body(&mut first_response).await?, b"ok");
    assert_eq!(read_body(&mut second_response).await?, b"ok");

    assert_eq!(
        first_store.complete_claimed_http(
            second_response,
            first.record(3, CredentialStatus::Ready, b"mixed-second")?,
        ),
        Err(PrivateStoreError::RefreshClaimMismatch)
    );
    assert_eq!(
        second_store.complete_claimed_http(
            first_response,
            second.record(3, CredentialStatus::Ready, b"mixed-first")?,
        ),
        Err(PrivateStoreError::RefreshClaimMismatch)
    );
    assert_eq!(
        first_store.load()?.ok_or("first quarantine was lost")?,
        first_unknown
    );
    assert_eq!(
        second_store.load()?.ok_or("second quarantine was lost")?,
        second_unknown
    );
    assert!(String::from_utf8(first_server.await??)?.contains("refresh-body"));
    assert!(String::from_utf8(second_server.await??)?.contains("refresh-body"));
    Ok(())
}

#[tokio::test]
async fn pre_cancelled_execute_never_accepts_and_restores_exact_record() -> ServerResult {
    let endpoint = endpoint().await?;
    let fixture = HttpFixture::new()?;
    let store = fixture.store()?;
    store.initialize(&fixture.record(
        7,
        CredentialStatus::RefreshRequired,
        b"exact-old-payload",
    )?)?;
    let fence = store.load()?.ok_or("missing initial record")?;
    let claim = store.begin_refresh(&fence)?;
    assert_eq!(claim.fence().generation(), CredentialGeneration::new(8)?);
    let limits = Limits::default();
    let client = client(endpoint.client_config, limits.clone())?;
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let error = store
        .execute_claimed_http(
            claim,
            &client,
            request(endpoint.origin, &limits, "/pre-cancelled")?,
            ExchangeControl::with_cancellation(
                Instant::now() + Duration::from_secs(1),
                cancellation,
            ),
        )
        .await
        .err()
        .ok_or("pre-cancelled exchange unexpectedly succeeded")?;

    assert_eq!(
        error.disposition(),
        ClaimedHttpDisposition::RestoredNotStarted
    );
    assert_eq!(
        error
            .transport_failure()
            .ok_or("missing transport failure")?
            .transmission(),
        TransmissionFact::NotStarted
    );
    assert_eq!(error.store_error(), None);
    assert!(
        timeout(Duration::from_millis(100), endpoint.listener.accept())
            .await
            .is_err()
    );
    let restored = load_raw_for_test(&store)?.ok_or("missing restored record")?;
    assert_eq!(restored.generation(), CredentialGeneration::new(9)?);
    assert_eq!(restored.status(), CredentialStatus::RefreshRequired);
    assert_eq!(restored.payload().expose_secret(), b"exact-old-payload");
    Ok(())
}

#[tokio::test]
async fn post_write_disconnect_is_verified_quarantine() -> ServerResult {
    let Endpoint {
        listener,
        acceptor,
        client_config,
        origin,
    } = endpoint().await?;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut stream = acceptor.accept(stream).await?;
        let captured = read_request(&mut stream).await?;
        drop(stream);
        Ok::<_, ServerError>(captured)
    });
    let fixture = HttpFixture::new()?;
    let store = fixture.store()?;
    store.initialize(&fixture.record(1, CredentialStatus::Ready, b"disconnect-secret")?)?;
    let claim = store.begin_refresh(&store.load()?.ok_or("missing record")?)?;
    let claimed_fence = claim.fence();
    let limits = Limits::default();
    let client = client(client_config, limits.clone())?;
    let error = store
        .execute_claimed_http(
            claim,
            &client,
            request(origin, &limits, "/disconnect?token=wire-secret")?,
            control(),
        )
        .await
        .err()
        .ok_or("disconnected exchange unexpectedly succeeded")?;

    assert_eq!(
        error.disposition(),
        ClaimedHttpDisposition::QuarantinedMayHaveTransmitted
    );
    assert_eq!(
        error
            .transport_failure()
            .ok_or("missing transport failure")?
            .transmission(),
        TransmissionFact::MayHaveTransmitted
    );
    assert_eq!(error.store_error(), None);
    let error_debug = format!("{error:?}");
    let error_display = error.to_string();
    for secret in [
        "disconnect-secret",
        "wire-secret",
        fixture.path.to_string_lossy().as_ref(),
    ] {
        assert!(!error_debug.contains(secret));
        assert!(!error_display.contains(secret));
    }
    let unknown = load_raw_for_test(&store)?.ok_or("missing quarantine")?;
    assert_eq!(unknown.fence(), claimed_fence);
    assert_eq!(unknown.status(), CredentialStatus::RefreshOutcomeUnknown);
    assert_eq!(unknown.payload().expose_secret(), b"disconnect-secret");
    assert!(String::from_utf8(server.await??)?.contains("refresh-body"));
    Ok(())
}

#[tokio::test]
async fn dropping_in_flight_future_after_start_leaves_unknown() -> ServerResult {
    let Endpoint {
        listener,
        acceptor,
        client_config,
        origin,
    } = endpoint().await?;
    let (request_seen_sender, request_seen_receiver) = oneshot::channel();
    let (release_sender, release_receiver) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut stream = acceptor.accept(stream).await?;
        let captured = read_request(&mut stream).await?;
        request_seen_sender
            .send(())
            .map_err(|()| io::Error::other("request observer dropped"))?;
        let _ = release_receiver.await;
        drop(stream);
        Ok::<_, ServerError>(captured)
    });
    let fixture = HttpFixture::new()?;
    let store = Arc::new(fixture.store()?);
    store.initialize(&fixture.record(1, CredentialStatus::Ready, b"in-flight-secret")?)?;
    let claim = store.begin_refresh(&store.load()?.ok_or("missing record")?)?;
    let claimed_fence = claim.fence();
    let limits = Limits::default();
    let client = Arc::new(client(client_config, limits.clone())?);
    let transaction = {
        let store = Arc::clone(&store);
        let client = Arc::clone(&client);
        tokio::spawn(async move {
            store
                .execute_claimed_http(
                    claim,
                    &client,
                    request(origin, &limits, "/held")?,
                    control(),
                )
                .await
                .map_err(ServerError::from)
        })
    };

    timeout(Duration::from_secs(2), request_seen_receiver).await??;
    transaction.abort();
    let join_error = transaction
        .await
        .err()
        .ok_or("aborted transaction unexpectedly completed")?;
    assert!(join_error.is_cancelled());
    let unknown = load_raw_for_test(&store)?.ok_or("missing quarantine")?;
    assert_eq!(unknown.fence(), claimed_fence);
    assert_eq!(unknown.status(), CredentialStatus::RefreshOutcomeUnknown);
    assert_eq!(unknown.payload().expose_secret(), b"in-flight-secret");
    let _ = release_sender.send(());
    assert!(String::from_utf8(server.await??)?.contains("refresh-body"));
    Ok(())
}

#[tokio::test]
async fn wrong_store_cannot_clear_unknown() -> ServerResult {
    let endpoint = endpoint().await?;
    let limits = Limits::default();
    let client = client(endpoint.client_config, limits.clone())?;
    let first = HttpFixture::new()?;
    let wrong = HttpFixture::new()?;
    let first_store = first.store()?;
    let wrong_store = wrong.store()?;
    first_store.initialize(&first.record(1, CredentialStatus::Ready, b"first-old")?)?;
    wrong_store.initialize(&wrong.record(1, CredentialStatus::Ready, b"wrong-ready")?)?;
    let first_claim =
        first_store.begin_refresh(&first_store.load()?.ok_or("missing first record")?)?;
    let rejected = wrong_store
        .execute_claimed_http(
            first_claim,
            &client,
            request(endpoint.origin, &limits, "/wrong-store")?,
            control(),
        )
        .await
        .err()
        .ok_or("wrong-store transaction unexpectedly succeeded")?;
    assert_eq!(
        rejected.disposition(),
        ClaimedHttpDisposition::ClaimRejected
    );
    assert_eq!(
        rejected.store_error(),
        Some(PrivateStoreError::RefreshClaimMismatch)
    );
    assert_eq!(rejected.transport_failure(), None);
    // The rejected error still owns the unstarted claim and therefore
    // the first store's execution lock; release it before the raw
    // (writer-locked) inspection.
    drop(rejected);
    let first_unknown = load_raw_for_test(&first_store)?.ok_or("missing first quarantine")?;
    assert_eq!(
        first_unknown.status(),
        CredentialStatus::RefreshOutcomeUnknown
    );
    assert_eq!(first_unknown.payload().expose_secret(), b"first-old");
    let wrong_ready = load_raw_for_test(&wrong_store)?.ok_or("missing wrong record")?;
    assert_eq!(wrong_ready.status(), CredentialStatus::Ready);
    assert_eq!(wrong_ready.payload().expose_secret(), b"wrong-ready");
    assert!(
        timeout(Duration::from_millis(100), endpoint.listener.accept())
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn same_path_cross_slot_claim_never_reaches_listener() -> ServerResult {
    let endpoint = endpoint().await?;
    let limits = Limits::default();
    let client = client(endpoint.client_config, limits.clone())?;

    let cross_slot = HttpFixture::new()?;
    let primary = PrivateCredentialStore::open_slot(
        &cross_slot.path,
        cross_slot.profile.clone(),
        CredentialSlotId::new("primary")?,
    )?;
    primary.initialize_selected(&cross_slot.record(
        1,
        CredentialStatus::Ready,
        b"cross-slot-old",
    )?)?;
    let claim = primary.begin_refresh(&primary.load()?.ok_or("missing primary")?)?;
    let secondary = PrivateCredentialStore::open_slot(
        &cross_slot.path,
        cross_slot.profile.clone(),
        CredentialSlotId::new("secondary")?,
    )?;
    let cross_slot_error = secondary
        .execute_claimed_http(
            claim,
            &client,
            request(endpoint.origin, &limits, "/cross-slot")?,
            control(),
        )
        .await
        .err()
        .ok_or("cross-slot exchange unexpectedly succeeded")?;
    assert_eq!(
        cross_slot_error.disposition(),
        ClaimedHttpDisposition::ClaimRejected
    );
    assert_eq!(
        cross_slot_error.store_error(),
        Some(PrivateStoreError::RefreshClaimMismatch)
    );
    assert_eq!(cross_slot_error.transport_failure(), None);
    drop(cross_slot_error);
    assert_eq!(
        load_raw_for_test(&primary)?
            .ok_or("missing cross-slot quarantine")?
            .status(),
        CredentialStatus::RefreshOutcomeUnknown
    );
    assert!(
        timeout(Duration::from_millis(100), endpoint.listener.accept())
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn stale_claim_cannot_restore_over_changed_unknown() -> ServerResult {
    let endpoint = endpoint().await?;
    let limits = Limits::default();
    let client = client(endpoint.client_config, limits.clone())?;
    let stale = HttpFixture::new()?;
    let stale_store = stale.store()?;
    stale_store.initialize(&stale.record(1, CredentialStatus::Ready, b"stale-old")?)?;
    let stale_claim =
        stale_store.begin_refresh(&stale_store.load()?.ok_or("missing stale record")?)?;
    // A live claim owns the private directory, so the API cannot
    // replace the record underneath it; inject the change directly to
    // exercise the claim's exact record verification.
    platform::write_record(
        &stale_store,
        &stale.record(
            3,
            CredentialStatus::RefreshOutcomeUnknown,
            b"concurrent-unknown",
        )?,
    )?;
    let rejected = stale_store
        .execute_claimed_http(
            stale_claim,
            &client,
            request(endpoint.origin, &limits, "/stale-claim")?,
            control(),
        )
        .await
        .err()
        .ok_or("stale transaction unexpectedly succeeded")?;
    assert_eq!(
        rejected.disposition(),
        ClaimedHttpDisposition::ClaimRejected
    );
    assert_eq!(
        rejected.store_error(),
        Some(PrivateStoreError::RefreshClaimMismatch)
    );
    assert_eq!(rejected.transport_failure(), None);
    drop(rejected);
    let still_unknown = load_raw_for_test(&stale_store)?.ok_or("missing stale quarantine")?;
    assert_eq!(still_unknown.generation(), CredentialGeneration::new(3)?);
    assert_eq!(
        still_unknown.status(),
        CredentialStatus::RefreshOutcomeUnknown
    );
    assert_eq!(
        still_unknown.payload().expose_secret(),
        b"concurrent-unknown"
    );
    assert!(
        timeout(Duration::from_millis(100), endpoint.listener.accept())
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn renamed_store_is_rejected_before_listener_accept() -> ServerResult {
    let endpoint = endpoint().await?;
    let fixture = HttpFixture::new()?;
    let store = fixture.store()?;
    store.initialize(&fixture.record(1, CredentialStatus::Ready, b"rename-old")?)?;
    let claim = store.begin_refresh(&store.load()?.ok_or("missing record")?)?;
    let unknown = claim.fence();
    let rotation = RotatedDirectory::replace(fixture.directory.path())?;
    let limits = Limits::default();
    let client = client(endpoint.client_config, limits.clone())?;
    let rejected = store
        .execute_claimed_http(
            claim,
            &client,
            request(endpoint.origin, &limits, "/renamed")?,
            control(),
        )
        .await
        .err()
        .ok_or("renamed-store exchange unexpectedly succeeded")?;
    assert_eq!(
        rejected.disposition(),
        ClaimedHttpDisposition::ClaimRejected
    );
    assert_eq!(
        rejected.store_error(),
        Some(PrivateStoreError::UnsafeDirectory)
    );
    assert_eq!(rejected.transport_failure(), None);
    assert!(
        timeout(Duration::from_millis(100), endpoint.listener.accept())
            .await
            .is_err()
    );
    drop(rotation);
    assert_eq!(store.load()?.ok_or("quarantine was lost")?, unknown);
    Ok(())
}

#[tokio::test]
async fn busy_execution_lock_fails_without_waiting_or_accepting() -> ServerResult {
    // The execution lock is taken by the claim itself, before the
    // durable Unknown write. A store owned by another writer therefore
    // refuses the claim immediately with a local contention fact and
    // leaves the Ready record untouched; nothing reaches the listener.
    let endpoint = endpoint().await?;
    let fixture = HttpFixture::new()?;
    let store = fixture.store()?;
    store.initialize(&fixture.record(1, CredentialStatus::Ready, b"busy-old")?)?;
    let ready = store.load()?.ok_or("missing record")?;
    let blocker = platform::lock(&store)?;
    let started = Instant::now();
    assert_eq!(
        store.begin_refresh(&ready).err(),
        Some(PrivateStoreError::StoreBusy)
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(
        timeout(Duration::from_millis(100), endpoint.listener.accept())
            .await
            .is_err()
    );
    drop(blocker);
    let record = load_raw_for_test(&store)?.ok_or("record was lost")?;
    assert_eq!(record.generation(), CredentialGeneration::new(1)?);
    assert_eq!(record.status(), CredentialStatus::Ready);
    assert_eq!(record.payload().expose_secret(), b"busy-old");
    // Once the writer releases the store, the same fence claims normally.
    let claim = store.begin_refresh(&ready)?;
    assert_eq!(claim.fence().generation(), CredentialGeneration::new(2)?);
    let restored = store.restore_refresh_definitely_not_started(claim)?;
    assert_eq!(restored.generation(), CredentialGeneration::new(3)?);
    assert_eq!(restored.status(), CredentialStatus::Ready);
    Ok(())
}

#[tokio::test]
async fn status_read_does_not_block_on_an_in_flight_refresh() -> ServerResult {
    // A claimed exchange owns the execution lock and the directory but
    // not the record lock, so a peer's fence read returns immediately
    // with the durable Unknown fence instead of waiting behind the
    // provider round-trip.
    let fixture = HttpFixture::new()?;
    let store = fixture.store()?;
    let (mut response, server) =
        successful_claimed_response(&fixture, &store, endpoint().await?, "/in-flight").await?;
    let unknown = response.refresh_fence();
    let reader = fixture.store()?;
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let _ = sender.send(reader.status());
    });
    let observed = receiver.recv_timeout(Duration::from_secs(2))??;
    assert_eq!(observed, Some(CredentialStatus::RefreshOutcomeUnknown));
    let fence = fixture.store()?.load()?.ok_or("fence vanished")?;
    assert_eq!(fence, unknown);
    // A writer still waits for the claim: it must not replace the
    // record under the exchange.
    let writer = fixture.store()?;
    let (write_sender, write_receiver) = mpsc::channel();
    let writer_thread = thread::spawn(move || {
        let _ = write_sender.send(writer.logout(unknown.generation()));
    });
    assert!(matches!(
        write_receiver.recv_timeout(Duration::from_millis(100)),
        Err(RecvTimeoutError::Timeout)
    ));
    assert_eq!(read_body(&mut response).await?, b"ok");
    let completed = store.complete_claimed_http(
        response,
        fixture.record(3, CredentialStatus::Ready, b"new")?,
    )?;
    assert_eq!(completed.generation(), CredentialGeneration::new(3)?);
    // The writer proceeds only after completion and sees the new generation.
    assert_eq!(
        write_receiver.recv_timeout(Duration::from_secs(2))?,
        Err(PrivateStoreError::StaleWriter)
    );
    worker.join().map_err(|_| "status reader panicked")?;
    writer_thread.join().map_err(|_| "writer panicked")?;
    assert!(String::from_utf8(server.await??)?.contains("refresh-body"));
    Ok(())
}

#[tokio::test]
async fn sibling_response_contention_returns_unstarted_claim_without_accepting() -> ServerResult {
    let fixture = HttpFixture::new()?;
    let sibling_account = AccountBinding::new("sibling-account")?;
    let sibling_store = PrivateCredentialStore::open_slot(
        fixture.directory.path().join("sibling.json"),
        fixture.profile.clone(),
        fixture.slot.clone(),
    )?;
    let sibling_record = CredentialRecord::new(
        fixture.profile.clone(),
        sibling_account,
        CredentialGeneration::new(1)?,
        CredentialStatus::Ready,
        SecretBytes::new(b"sibling-old".to_vec()),
    )?;
    sibling_store.initialize(&sibling_record)?;
    let sibling_fence = sibling_store.load()?.ok_or("missing sibling record")?;

    let store = fixture.store()?;
    let (held_response, held_server) =
        successful_claimed_response(&fixture, &store, endpoint().await?, "/held-sibling").await?;
    let Endpoint { listener, .. } = endpoint().await?;
    // The held response owns the private directory. A sibling record in
    // the same directory cannot even claim: the contention surfaces
    // before any durable write, so the sibling stays Ready and never
    // reaches its listener.
    assert_eq!(
        sibling_store.begin_refresh(&sibling_fence).err(),
        Some(PrivateStoreError::StoreBusy)
    );
    assert!(
        timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
    assert_eq!(
        sibling_store.load()?.ok_or("sibling fence was lost")?,
        sibling_fence
    );
    store.quarantine_claimed_http(held_response)?;
    let sibling_claim = sibling_store.begin_refresh(&sibling_fence)?;
    assert_eq!(
        sibling_claim.fence().generation(),
        CredentialGeneration::new(2)?
    );
    let restored = sibling_store.restore_refresh_definitely_not_started(sibling_claim)?;
    assert_eq!(restored.generation(), CredentialGeneration::new(3)?);
    let sibling = load_raw_for_test(&sibling_store)?.ok_or("restored sibling record was lost")?;
    assert_eq!(sibling.status(), CredentialStatus::Ready);
    assert_eq!(sibling.payload().expose_secret(), b"sibling-old");
    assert!(String::from_utf8(held_server.await??)?.contains("refresh-body"));
    Ok(())
}
