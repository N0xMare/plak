use super::*;

#[test]
fn replay_required_history_refusals_preserve_the_protected_history_remedy() {
    use crate::ResponsesRequestContentError as ContentError;
    use plakous_model::ModelPreparationErrorKind;

    for content in [
        ContentError::CompactionWithoutReplay,
        ContentError::AssistantRefusalWithoutReplay,
        ContentError::OpaqueOnlyWithoutCompatibleReplay,
        ContentError::ProviderHostedCallWithoutReplay,
    ] {
        let error = ResponsesAttemptBuildError::Request(ResponsesRequestError::Content(content));
        assert_eq!(
            error.preparation_error_kind(),
            ModelPreparationErrorKind::ProtectedHistory
        );
    }
    let error = ResponsesAttemptBuildError::Request(ResponsesRequestError::Content(
        ContentError::StandaloneCitation,
    ));
    assert_eq!(
        error.preparation_error_kind(),
        ModelPreparationErrorKind::Projection
    );
}

use crate::{ResponsesProviderErrorClass, ResponsesStoragePolicy};

#[tokio::test(start_paused = true)]
async fn responses_unreachable_ride_out_excludes_incomplete_and_honors_hard_deadline() -> TestResult
{
    for workflow in [false, true] {
        for (fault, hard_seconds, expected) in [
            ("connect", None, 12),
            ("connect", Some(1), 1),
            ("write-reset", None, 4),
            ("record-rejected", None, 4),
        ] {
            let client = Client::new(Limits::default())?.with_faults(Some(
                plakous_http::FaultPlan::parse(&format!("fault={fault}"))?,
            ));
            let dialect = wire_dialect(
                Origin::parse("https://127.0.0.1:9")?,
                openai_generation_fingerprint(b"ride-out"),
            );
            let authorization =
                ResponsesAuthorizationAdapter::new(1, 1, wire_preflight, wire_revalidate)?;
            let data = attempt_data(&client, &authorization)?;
            let started = monotonic_now();
            let hard = hard_seconds.map(|seconds| started + Duration::from_secs(seconds));
            let attempt = if workflow {
                let metadata = RequestMetadata::for_session_workflow(
                    typed_id::<RunId>(10)?,
                    SessionWorkflowRootId::new([7; 32])?,
                    typed_id::<AttemptId>(12)?,
                    Timestamp::new(1_700_000_000, 0)?,
                );
                let (request, authority) = exact_request_authority_with_metadata(false, metadata)?;
                ResponsesAttempt::from_prepared(
                    prepare_responses_attempt(authority, &request, None, &data)?,
                    typed_id::<AttemptId>(14)?,
                    CancellationToken::new(),
                    started + Duration::from_secs(5),
                )
            } else {
                executable_attempt(&data, CancellationToken::new())?
            }
            .with_dispatch_deadlines(Duration::from_secs(5), hard);
            let service =
                ResponsesAttemptService::new(client, Arc::clone(&dialect), data, authorization)?;
            let Err(error) = service.execute(attempt).await else {
                panic!("injected outage must fail");
            };
            assert_eq!(error.facts().physical_attempts(), expected, "{fault}");
            assert_eq!(
                error.facts().summary().ride_out_physical,
                if fault == "connect" { expected } else { 0 }
            );
            assert_eq!(dialect.preflights.load(Ordering::SeqCst), expected as usize);
            assert_eq!(
                dialect.final_authorizations.load(Ordering::SeqCst),
                expected as usize
            );
            assert_eq!(
                dialect.guards_dropped.load(Ordering::SeqCst),
                expected as usize
            );
            assert!(monotonic_now().duration_since(started) <= Duration::from_secs(240));
            if let Some(hard) = hard {
                assert!(monotonic_now() <= hard);
            }
        }
    }
    Ok(())
}

use std::collections::BTreeMap;
use std::error::Error;
use std::io;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use plakous_attempt::ProviderAcceptance;
use plakous_http::{HeaderMap, HeaderValue, Limits, Method, Origin, TlsConfigIdentity};
use plakous_model::{
    Actor, AttemptId, Capability, CapabilityConstraints, CapabilitySet, ContentBlock,
    ConversationItem, ConversationView, EntryId, FactProvenance, FactSource, FeatureSupport,
    FinishReason, FixedClock, ModelAdapterProfileId, ModelCallContext, ModelError,
    ModelErrorContext, ModelErrorKind, ModelEvent, ModelId, ModelRequest, ModelRetryRisk,
    ModelRetryability, ProtocolId, Provenance, ReplayKindId, RequestMetadata,
    ResolvedModelDescriptor, RouteId, RunId, SessionWorkflowRootId, Support, TextBlock, Timestamp,
    ToolConcurrency, ToolDefinition, ToolEffect, ToolInputContract, ToolName, ToolOutputContract,
    TurnId, Usage, ValidationMode,
};
use plakous_responses::{
    CanonicalIdSources, LifecycleDialectPolicy, ResponsesDialectProfile,
    ResponsesSemanticRevisions, ResponsesStreamLimits,
};
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;
use uuid::Uuid;

use crate::{
    CredentialProviderTag, PreparedResponsesModelExecution, ResponsesCanonicalDialectData,
    ResponsesCanonicalTool, ResponsesHostedProgramContext, ResponsesModelErrorHook,
    start_prepared_responses_model_call,
};

fn openai_generation_fingerprint(binding: &[u8]) -> CredentialGenerationFingerprint {
    match CredentialGenerationFingerprint::mint_exact(
        CredentialProviderTag::OpenAiPlatform,
        binding,
    ) {
        Ok(fingerprint) => fingerprint,
        Err(error) => panic!("test credential-generation binding: {error}"),
    }
}

type TestError = Box<dyn Error + Send + Sync>;
type TestResult<T = ()> = Result<T, TestError>;

fn model_binding(label: &'static [u8]) -> Result<Digest, TestError> {
    Ok(Digest::sha256(DigestDomain::ModelBinding, 1, &[label])?)
}

fn cache_affinity_binding() -> Result<Digest, TestError> {
    Ok(Digest::sha256(
        DigestDomain::CacheAffinity,
        1,
        &[b"attempt-host-cache-affinity"],
    )?)
}

fn attempt_binding(
    attempt: u128,
    generation: u32,
) -> Result<ResponsesPhysicalRequestBinding, TestError> {
    attempt_binding_for_slots(attempt, generation, ResponsesAuthSlots::Secondary)
}

fn attempt_binding_for_slots(
    attempt: u128,
    generation: u32,
    auth_slots: ResponsesAuthSlots,
) -> Result<ResponsesPhysicalRequestBinding, TestError> {
    let authority = model_binding(b"authority")?;
    let execution = model_binding(b"execution")?;
    let request_body = Digest::sha256(
        DigestDomain::ProviderOperation,
        REQUEST_DIGEST_REVISION,
        &[b"body"],
    )?;
    let operation = ResponsesOperationBinding {
        request_authority: authority,
        execution,
        request_body,
        ledger_policy: LedgerPolicy::OPENAI_STRICT,
        auth_slots,
        replay_safety: ReplaySafety::CostOnly,
    };
    let attempt_id = AttemptId::try_from(Uuid::from_u128(attempt))?;
    Ok(PhysicalRequestBinding::new(
        AttemptBinding::new(operation, attempt_id),
        generation,
    ))
}

fn request(body: &'static [u8], authorization: &'static str) -> Result<Request, TestError> {
    request_at(
        "https://example.test",
        Method::POST,
        "/responses",
        body,
        authorization,
        "application/json",
    )
}

fn request_at(
    origin: &str,
    method: Method,
    path: &str,
    body: &'static [u8],
    authorization: &'static str,
    content_type: &'static str,
) -> Result<Request, TestError> {
    let mut headers = HeaderMap::new();
    headers.insert("authorization", HeaderValue::from_static(authorization));
    headers.insert("content-type", HeaderValue::from_static(content_type));
    Ok(Request::new(
        Origin::parse(origin)?,
        method,
        path,
        headers,
        Bytes::from_static(body),
        &Limits::default(),
    )?)
}

fn dual_request(
    primary_credential: &'static str,
    secondary_credential: &'static str,
    primary_account: &'static str,
    secondary_account: &'static str,
    reverse: bool,
) -> Result<Request, TestError> {
    let values = [
        ("x-primary-authorization", primary_credential),
        ("authorization", secondary_credential),
        ("x-primary-account", primary_account),
        ("x-secondary-account", secondary_account),
        ("content-type", "application/json"),
    ];
    let mut headers = HeaderMap::new();
    if reverse {
        for (name, value) in values.into_iter().rev() {
            headers.insert(name, HeaderValue::from_static(value));
        }
    } else {
        for (name, value) in values {
            headers.insert(name, HeaderValue::from_static(value));
        }
    }
    Ok(Request::new(
        Origin::parse("https://example.test")?,
        Method::POST,
        "/responses",
        headers,
        Bytes::from_static(b"{}"),
        &Limits::default(),
    )?)
}

fn header_bindings_for(content_type: &'static [u8]) -> Vec<ResponsesHeaderBinding> {
    vec![
        ResponsesHeaderBinding::CredentialSecret {
            name: HeaderName::from_static("authorization"),
            slot: ResponsesAuthSlot::Secondary,
        },
        ResponsesHeaderBinding::PublicExact {
            name: HeaderName::from_static("content-type"),
            value: content_type.to_vec(),
        },
    ]
}

fn header_bindings() -> Vec<ResponsesHeaderBinding> {
    header_bindings_for(b"application/json")
}

#[derive(Debug)]
struct TestDialect;

#[derive(Debug)]
struct TestGuard;

fn test_preflight<'a>(
    _dialect: &'a TestDialect,
    _request: ResponsesPreflightRequest<'a>,
) -> ResponsesPreflightFuture<'a, TestGuard> {
    Box::pin(async { Err(ResponsesDispatchRejection::Unavailable) })
}

#[allow(
    clippy::unnecessary_wraps,
    reason = "fixture must satisfy the closed revalidation function ABI"
)]
fn test_revalidate(
    _dialect: &TestDialect,
    _request: ResponsesRevalidationRequest<'_, TestGuard>,
) -> Result<(), ResponsesDispatchRejection> {
    Ok(())
}

#[derive(Debug)]
struct LifecycleDialect {
    generation: CredentialGenerationFingerprint,
    cancel_after_authorization: bool,
    preflights: AtomicUsize,
    final_authorizations: AtomicUsize,
}

#[derive(Debug)]
struct LifecycleGuard {
    generation: CredentialGenerationFingerprint,
}

fn lifecycle_preflight<'a>(
    dialect: &'a LifecycleDialect,
    request: ResponsesPreflightRequest<'a>,
) -> ResponsesPreflightFuture<'a, LifecycleGuard> {
    dialect.preflights.fetch_add(1, Ordering::SeqCst);
    Box::pin(async move {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("Bearer secret"),
        );
        headers.insert(
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("application/json"),
        );
        let wire = Request::new(
            Origin::parse("https://example.test")
                .map_err(|_| ResponsesDispatchRejection::Binding)?,
            Method::POST,
            "/responses",
            headers,
            request.body(),
            &Limits::default(),
        )
        .map_err(|_| ResponsesDispatchRejection::Binding)?;
        Ok(ResponsesPreparedPhysicalRequest::new(
            request.physical().clone(),
            wire,
            ResponsesCredentialAuthority::secondary(dialect.generation),
            header_bindings(),
            LifecycleGuard {
                generation: dialect.generation,
            },
        ))
    })
}

fn lifecycle_revalidate(
    dialect: &LifecycleDialect,
    mut request: ResponsesRevalidationRequest<'_, LifecycleGuard>,
) -> Result<(), ResponsesDispatchRejection> {
    dialect.final_authorizations.fetch_add(1, Ordering::SeqCst);
    if request
        .binding()
        .credential_authority()
        .generation(ResponsesAuthSlot::Secondary)
        != Some(request.response_head_guard_mut().generation)
    {
        return Err(ResponsesDispatchRejection::Binding);
    }
    if dialect.cancel_after_authorization {
        request.cancellation().cancel();
    }
    Ok(())
}

struct WireEndpoint {
    listener: TcpListener,
    acceptor: TlsAcceptor,
    client_config: ClientConfig,
    tls_identity: TlsConfigIdentity,
    origin: Origin,
}

async fn wire_endpoint() -> TestResult<WireEndpoint> {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])?;
    let certificate = cert.der().clone();
    let tls_identity =
        TlsConfigIdentity::from_parts(1, &[b"responses-attempt-test-tls", certificate.as_ref()])?;
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
    let origin = Origin::parse(&format!("https://{}", listener.local_addr()?))?;
    Ok(WireEndpoint {
        listener,
        acceptor: TlsAcceptor::from(Arc::new(server_config)),
        client_config,
        tls_identity,
        origin,
    })
}

async fn read_wire_request(stream: &mut TlsStream<TcpStream>) -> TestResult<Vec<u8>> {
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

async fn responding_wire_peer(
    response: Vec<u8>,
) -> TestResult<(
    ClientConfig,
    TlsConfigIdentity,
    Origin,
    JoinHandle<TestResult<(Vec<u8>, usize)>>,
)> {
    let endpoint = wire_endpoint().await?;
    let WireEndpoint {
        listener,
        acceptor,
        client_config,
        tls_identity,
        origin,
    } = endpoint;
    let peer = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut stream = acceptor.accept(stream).await?;
        let request = read_wire_request(&mut stream).await?;
        stream.write_all(&response).await?;
        stream.shutdown().await?;
        let accepts = if timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_ok()
        {
            2
        } else {
            1
        };
        Ok((request, accepts))
    });
    Ok((client_config, tls_identity, origin, peer))
}

async fn dropping_wire_peer_after_request() -> TestResult<(
    ClientConfig,
    TlsConfigIdentity,
    Origin,
    JoinHandle<TestResult<Vec<u8>>>,
)> {
    let endpoint = wire_endpoint().await?;
    let WireEndpoint {
        listener,
        acceptor,
        client_config,
        tls_identity,
        origin,
    } = endpoint;
    let peer = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut stream = acceptor.accept(stream).await?;
        read_wire_request(&mut stream).await
    });
    Ok((client_config, tls_identity, origin, peer))
}

fn complete_sse_body() -> Vec<u8> {
    concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"in_progress\",\"model\":\"gpt-test\"}}\n\n",
        "event: response.in_progress\n",
        "data: {\"type\":\"response.in_progress\",\"sequence_number\":1,\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-test\"}}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"sequence_number\":2,\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"completed\",\"model\":\"gpt-test\",\"output\":[]}}\n\n"
    )
    .as_bytes()
    .to_vec()
}

fn http_response(status: &str, content_type: Option<&str>, body: &[u8]) -> Vec<u8> {
    http_response_with(status, content_type, "", body)
}

fn http_response_with(
    status: &str,
    content_type: Option<&str>,
    headers: &str,
    body: &[u8],
) -> Vec<u8> {
    let content_type =
        content_type.map_or_else(String::new, |value| format!("Content-Type: {value}\r\n"));
    format!(
        "HTTP/1.1 {status}\r\n{content_type}{headers}X-Request-ID: req_attempt_host_exact\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes()
    .into_iter()
    .chain(body.iter().copied())
    .collect()
}

#[derive(Debug)]
struct WireDialect {
    origin: Origin,
    generation: CredentialGenerationFingerprint,
    current_generation: CredentialGenerationFingerprint,
    cancel_after_authorization: bool,
    preflights: AtomicUsize,
    final_authorizations: AtomicUsize,
    credential_rejections: AtomicUsize,
    guards_dropped: Arc<AtomicUsize>,
}

#[derive(Debug)]
struct WireGuard {
    generation: CredentialGenerationFingerprint,
    drops: Arc<AtomicUsize>,
}

impl Drop for WireGuard {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

fn wire_preflight<'a>(
    dialect: &'a WireDialect,
    request: ResponsesPreflightRequest<'a>,
) -> ResponsesPreflightFuture<'a, WireGuard> {
    Box::pin(async move {
        dialect.preflights.fetch_add(1, Ordering::SeqCst);
        let mut headers = HeaderMap::new();
        let authorization = if dialect.credential_rejections.load(Ordering::SeqCst) == 0 {
            "Bearer exact-test-secret"
        } else {
            "Bearer renewed-test-secret"
        };
        headers.insert(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static(authorization),
        );
        headers.insert(
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("application/json"),
        );
        let wire = Request::new(
            dialect.origin.clone(),
            Method::POST,
            "/responses?attempt=exact",
            headers,
            request.body(),
            &Limits::default(),
        )
        .map_err(|_| ResponsesDispatchRejection::Binding)?;
        Ok(ResponsesPreparedPhysicalRequest::new(
            request.physical().clone(),
            wire,
            ResponsesCredentialAuthority::secondary(dialect.generation),
            header_bindings(),
            WireGuard {
                generation: dialect.generation,
                drops: Arc::clone(&dialect.guards_dropped),
            },
        ))
    })
}

fn wire_credential_rejected(
    dialect: &WireDialect,
) -> Pin<Box<dyn Future<Output = Result<(), ResponsesDispatchRejection>> + Send + '_>> {
    Box::pin(async move {
        dialect.credential_rejections.fetch_add(1, Ordering::SeqCst);
        Ok(())
    })
}

fn pending_wire_credential_rejected(
    dialect: &WireDialect,
) -> Pin<Box<dyn Future<Output = Result<(), ResponsesDispatchRejection>> + Send + '_>> {
    Box::pin(async move {
        dialect.credential_rejections.fetch_add(1, Ordering::SeqCst);
        std::future::pending().await
    })
}

fn failing_wire_credential_rejected(
    dialect: &WireDialect,
) -> Pin<Box<dyn Future<Output = Result<(), ResponsesDispatchRejection>> + Send + '_>> {
    Box::pin(async move {
        dialect.credential_rejections.fetch_add(1, Ordering::SeqCst);
        Err(ResponsesDispatchRejection::Binding)
    })
}

/// A wire peer that answers one connection per queued response and returns
/// each request for exact authorization assertions.
async fn responding_wire_peer_sequence(
    responses: Vec<Vec<u8>>,
) -> TestResult<(
    ClientConfig,
    TlsConfigIdentity,
    Origin,
    JoinHandle<TestResult<Vec<Vec<u8>>>>,
)> {
    let endpoint = wire_endpoint().await?;
    let WireEndpoint {
        listener,
        acceptor,
        client_config,
        tls_identity,
        origin,
    } = endpoint;
    let peer = tokio::spawn(async move {
        let mut requests = Vec::new();
        for response in responses {
            let Ok(Ok((stream, _))) = timeout(Duration::from_secs(5), listener.accept()).await
            else {
                break;
            };
            let mut stream = acceptor.accept(stream).await?;
            let request = read_wire_request(&mut stream).await?;
            stream.write_all(&response).await?;
            stream.shutdown().await?;
            requests.push(request);
        }
        Ok(requests)
    });
    Ok((client_config, tls_identity, origin, peer))
}

/// [`responding_wire_peer_sequence`] whose first response withholds its last
/// bytes for `hold`, so that body is still open when `hold` has passed.
///
/// A caller that drops the exchange during the hold (its hard deadline) still
/// yields the request it sent. Later responses are written whole. The accept
/// wait is only a liveness bound for the replay's reconnect after `hold`.
async fn responding_wire_peer_sequence_holding_first(
    responses: Vec<Vec<u8>>,
    hold: Duration,
) -> TestResult<(
    ClientConfig,
    TlsConfigIdentity,
    Origin,
    JoinHandle<TestResult<Vec<Vec<u8>>>>,
)> {
    let WireEndpoint {
        listener,
        acceptor,
        client_config,
        tls_identity,
        origin,
    } = wire_endpoint().await?;
    let peer = tokio::spawn(async move {
        let mut requests = Vec::new();
        for (index, response) in responses.into_iter().enumerate() {
            let Ok(Ok((stream, _))) = timeout(Duration::from_secs(15), listener.accept()).await
            else {
                break;
            };
            let mut stream = acceptor.accept(stream).await?;
            let request = read_wire_request(&mut stream).await?;
            let pieces: Vec<&[u8]> = response.chunks(7).collect();
            let last = pieces.len().saturating_sub(1);
            for (sent, chunk) in pieces.into_iter().enumerate() {
                if index == 0 && sent == last {
                    tokio::time::sleep(hold).await;
                    if stream.write_all(chunk).await.is_err() {
                        break;
                    }
                } else {
                    stream.write_all(chunk).await?;
                }
                tokio::task::yield_now().await;
            }
            let _ = stream.shutdown().await;
            requests.push(request);
        }
        Ok(requests)
    });
    Ok((client_config, tls_identity, origin, peer))
}

fn wire_revalidate(
    dialect: &WireDialect,
    mut request: ResponsesRevalidationRequest<'_, WireGuard>,
) -> Result<(), ResponsesDispatchRejection> {
    dialect.final_authorizations.fetch_add(1, Ordering::SeqCst);
    let bound = request
        .binding()
        .credential_authority()
        .generation(ResponsesAuthSlot::Secondary);
    if bound != Some(dialect.current_generation)
        || request.response_head_guard_mut().generation != dialect.current_generation
    {
        return Err(ResponsesDispatchRejection::Binding);
    }
    if dialect.cancel_after_authorization {
        request.cancellation().cancel();
    }
    Ok(())
}

fn wire_service_and_attempt(
    client: Client,
    dialect: Arc<WireDialect>,
    cancellation: CancellationToken,
    deadline: Instant,
) -> Result<
    (
        ResponsesAttemptService<WireDialect, WireGuard>,
        ResponsesAttempt,
    ),
    TestError,
> {
    wire_service_and_attempt_with_error_policy(
        client,
        dialect,
        cancellation,
        deadline,
        ResponsesProviderErrorPolicy::OpenAiCompatible,
    )
}

fn wire_service_and_attempt_with_error_policy(
    client: Client,
    dialect: Arc<WireDialect>,
    cancellation: CancellationToken,
    deadline: Instant,
    provider_error_policy: ResponsesProviderErrorPolicy,
) -> Result<
    (
        ResponsesAttemptService<WireDialect, WireGuard>,
        ResponsesAttempt,
    ),
    TestError,
> {
    let authorization = ResponsesAuthorizationAdapter::new(1, 1, wire_preflight, wire_revalidate)?;
    let data = attempt_data_with_error_policy(&client, &authorization, provider_error_policy)?;
    let mut attempt = executable_attempt(&data, cancellation)?;
    attempt.deadline = deadline;
    let service = ResponsesAttemptService::new(client, dialect, data, authorization)?;
    Ok((service, attempt))
}

fn wire_dialect(origin: Origin, generation: CredentialGenerationFingerprint) -> Arc<WireDialect> {
    Arc::new(WireDialect {
        origin,
        generation,
        current_generation: generation,
        cancel_after_authorization: false,
        preflights: AtomicUsize::new(0),
        final_authorizations: AtomicUsize::new(0),
        credential_rejections: AtomicUsize::new(0),
        guards_dropped: Arc::new(AtomicUsize::new(0)),
    })
}

fn attempt_data<D, G>(
    client: &Client,
    authorization: &ResponsesAuthorizationAdapter<D, G>,
) -> Result<ResponsesAttemptDialectData, TestError> {
    attempt_data_with_error_policy(
        client,
        authorization,
        ResponsesProviderErrorPolicy::OpenAiCompatible,
    )
}

fn attempt_data_with_error_policy<D, G>(
    client: &Client,
    authorization: &ResponsesAuthorizationAdapter<D, G>,
    provider_error_policy: ResponsesProviderErrorPolicy,
) -> Result<ResponsesAttemptDialectData, TestError> {
    attempt_data_with_policies(client, authorization, provider_error_policy, "gpt-test")
}

fn attempt_data_with_policies<D, G>(
    client: &Client,
    authorization: &ResponsesAuthorizationAdapter<D, G>,
    provider_error_policy: ResponsesProviderErrorPolicy,
    stream_model: &str,
) -> Result<ResponsesAttemptDialectData, TestError> {
    attempt_data_with_stream(
        client,
        authorization,
        provider_error_policy,
        ResponsesStreamDialectData::codex(stream_model),
    )
}

fn attempt_data_with_stream<D, G>(
    client: &Client,
    authorization: &ResponsesAuthorizationAdapter<D, G>,
    provider_error_policy: ResponsesProviderErrorPolicy,
    stream: ResponsesStreamDialectData,
) -> Result<ResponsesAttemptDialectData, TestError> {
    let descriptor = test_descriptor()?;
    let revisions = ResponsesSemanticRevisions::new(1, 1, 1)?;
    let target = ResponsesAttemptTarget::from_admitted_target(
        &descriptor,
        model_binding(b"test account")?,
        revisions,
    )?;
    let replay = ResponsesReplayProjection::new(
        ProtocolId::new("openai.responses")?,
        None,
        ModelId::new("gpt-test")?,
        ReplayKindId::new("responses.output_item")?,
        1,
    )?;
    Ok(ResponsesAttemptDialectData::new(
        1,
        target,
        client,
        authorization,
        LedgerPolicy::OPENAI_STRICT,
        ResponsesAuthSlots::Secondary,
        ResponsesRequestPolicy::openai_codex(false, false, "reasoning.encrypted_content"),
        stream,
        ResponsesStreamLimits::default(),
        replay,
        ResponsesStorageMode::Stateless,
        provider_error_policy,
        ResponsesResponseLimits::default(),
        Some(HeaderName::from_static("x-request-id")),
        false,
    )?)
}

fn typed_id<T>(value: u128) -> Result<T, plakous_model::IdentityError>
where
    T: TryFrom<Uuid, Error = plakous_model::IdentityError>,
{
    T::try_from(Uuid::from_u128(value))
}

fn test_descriptor() -> Result<ResolvedModelDescriptor, TestError> {
    let protocol = ProtocolId::new("openai.responses")?;
    let model = ModelId::new("gpt-test")?;
    let mut capabilities = BTreeMap::new();
    for capability in [
        Capability::Streaming,
        Capability::Tools,
        Capability::Reasoning,
    ] {
        capabilities.insert(
            capability,
            FeatureSupport::new(
                Support::Native,
                CapabilityConstraints::default(),
                FactProvenance::new(
                    FactSource::TestFixture,
                    Timestamp::new(1_700_000_000, 0)?,
                    None,
                ),
            ),
        );
    }
    Ok(ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        protocol,
        RouteId::new("test.responses")?,
        model,
        None,
        None,
        ModelAdapterProfileId::new("responses-test-adapter")?,
        CapabilitySet::new(capabilities, BTreeMap::new()),
    ))
}

fn exact_request_authority() -> Result<(ModelRequest, ResponsesRequestAuthority), TestError> {
    exact_request_authority_with_tools(false)
}

fn exact_request_authority_with_tools(
    include_tools: bool,
) -> Result<(ModelRequest, ResponsesRequestAuthority), TestError> {
    let metadata = RequestMetadata::for_turn(
        typed_id::<RunId>(10)?,
        typed_id::<TurnId>(11)?,
        typed_id::<AttemptId>(12)?,
        Timestamp::new(1_700_000_000, 0)?,
    );
    exact_request_authority_with_metadata(include_tools, metadata)
}

fn exact_request_authority_with_metadata(
    include_tools: bool,
    metadata: RequestMetadata,
) -> Result<(ModelRequest, ResponsesRequestAuthority), TestError> {
    let descriptor = test_descriptor()?;
    let item = ConversationItem::new(
        typed_id::<EntryId>(13)?,
        Actor::User,
        vec![ContentBlock::Text(TextBlock::new("hello", None)?)],
        Provenance::UserInput,
    )?;
    let harness_plan = Digest::sha256(
        DigestDomain::ResolvedHarnessPlan,
        1,
        &[b"attempt-host-test-plan"],
    )?;
    let mut builder =
        crate::test_support::model_request_builder(ConversationView::new(vec![item]), metadata)
            .resolved_harness_plan_identity(harness_plan);
    if include_tools {
        builder = builder.tools(vec![plakous_model::ConfiguredTool::new(ToolDefinition::new(
            ToolName::new("lookup")?,
            "1.0.0",
            "lookup",
            ToolInputContract::structured(serde_json::json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{},"required":[],"additionalProperties":false}))?,
            ToolOutputContract::portable_model_content(),
            ToolConcurrency::Serial,
            ToolEffect::ReadOnly,
        )?, plakous_model::ToolExecutionDisposition::Local, plakous_model::ToolCallerDisposition::DirectOnly)]);
    }
    let request = builder.build()?;
    let plan = descriptor.validate_request(request.clone(), ValidationMode::Strict)?;
    let authority = ResponsesRequestAuthority::from_admitted_operation(
        &descriptor,
        &plan,
        &request,
        model_binding(b"test account")?,
        ResponsesSemanticRevisions::new(1, 1, 1)?,
        Some(cache_affinity_binding()?),
        ResponsesStoragePolicy::Stateless,
    )?;
    Ok((request, authority))
}

fn prepared_model_host(
    client: Client,
    dialect: Arc<WireDialect>,
    observation_capacity: usize,
    include_tools: bool,
    error_hook: Option<ResponsesModelErrorHook>,
    hosted_programs: ResponsesHostedProgramContext,
    openai_platform: bool,
) -> Result<PreparedModelHostFixture, TestError> {
    prepared_model_host_with_grok(
        client,
        dialect,
        observation_capacity,
        include_tools,
        error_hook,
        hosted_programs,
        openai_platform,
        false,
        true,
    )
}

/// A model host whose attempt is not turn-rooted and so is never retried.
fn prepared_unretried_model_host(
    client: Client,
    dialect: Arc<WireDialect>,
) -> Result<PreparedModelHostFixture, TestError> {
    prepared_model_host_with_grok(
        client,
        dialect,
        4,
        false,
        None,
        ResponsesHostedProgramContext::default(),
        false,
        false,
        false,
    )
}

#[allow(
    clippy::too_many_arguments,
    clippy::fn_params_excessive_bools,
    reason = "test fixture binds the exact route policy"
)]
fn prepared_model_host_with_grok(
    client: Client,
    dialect: Arc<WireDialect>,
    observation_capacity: usize,
    include_tools: bool,
    error_hook: Option<ResponsesModelErrorHook>,
    hosted_programs: ResponsesHostedProgramContext,
    openai_platform: bool,
    grok: bool,
    retry_permitted: bool,
) -> Result<PreparedModelHostFixture, TestError> {
    let authorization = ResponsesAuthorizationAdapter::new(1, 1, wire_preflight, wire_revalidate)?;
    let data = if grok {
        attempt_data_with_stream(
            &client,
            &authorization,
            ResponsesProviderErrorPolicy::Grok,
            ResponsesStreamDialectData::grok("gpt-test"),
        )?
    } else {
        attempt_data(&client, &authorization)?
    };
    let (request, authority) = exact_request_authority_with_tools(include_tools)?;
    let descriptor = test_descriptor()?;
    let plan = descriptor.validate_request(request.clone(), ValidationMode::Strict)?;
    let artifact_binding = authority.artifact_binding().clone();
    let artifact_binding_identity = artifact_binding.identity();
    let mut attempt = prepare_responses_attempt(authority, &request, None, &data)?;
    attempt.retry_permitted = retry_permitted;
    let canonical_tools = if include_tools {
        vec![ResponsesCanonicalTool::local_function(ToolName::new(
            "lookup",
        )?)]
    } else {
        Vec::new()
    };
    let canonical = if grok {
        ResponsesCanonicalDialectData::grok(ModelId::new("gpt-test")?, canonical_tools)?
    } else if openai_platform {
        ResponsesCanonicalDialectData::openai_platform(ModelId::new("gpt-test")?, canonical_tools)?
    } else {
        ResponsesCanonicalDialectData::codex(ModelId::new("gpt-test")?, canonical_tools)?
    };
    let clock: Arc<dyn plakous_model::Clock> =
        Arc::new(FixedClock::new(Timestamp::new(1_700_000_001, 0)?));
    let execution = PreparedResponsesModelExecution::new(
        attempt,
        canonical,
        CanonicalIdSources::with_clock(Arc::clone(&clock)),
        CanonicalIdSources::with_clock(Arc::clone(&clock)),
        hosted_programs,
        plan,
        artifact_binding,
        clock,
        descriptor,
        observation_capacity,
        error_hook,
    )?;
    let service = Arc::new(ResponsesAttemptService::new(
        client,
        dialect,
        data,
        authorization,
    )?);
    let cancellation = CancellationToken::new();
    let context = ModelCallContext::new(typed_id::<AttemptId>(14)?, cancellation.clone(), None);
    Ok((
        execution,
        service,
        context,
        cancellation,
        artifact_binding_identity,
    ))
}

type PreparedModelHostFixture = (
    PreparedResponsesModelExecution,
    Arc<ResponsesAttemptService<WireDialect, WireGuard>>,
    ModelCallContext,
    CancellationToken,
    Digest,
);

fn prepared_attempt(
    data: &ResponsesAttemptDialectData,
) -> Result<PreparedResponsesAttempt, TestError> {
    let (request, authority) = exact_request_authority()?;
    Ok(prepare_responses_attempt(authority, &request, None, data)?)
}

fn executable_attempt(
    data: &ResponsesAttemptDialectData,
    cancellation: CancellationToken,
) -> Result<ResponsesAttempt, TestError> {
    Ok(ResponsesAttempt::from_prepared(
        prepared_attempt(data)?,
        typed_id::<AttemptId>(14)?,
        cancellation,
        Instant::now() + Duration::from_secs(5),
    ))
}

fn attempt_with_safety(
    data: &ResponsesAttemptDialectData,
    cancellation: CancellationToken,
    safety: ReplaySafety,
) -> Result<ResponsesAttempt, TestError> {
    let mut prepared = prepared_attempt(data)?;
    prepared.operation.replay_safety = safety;
    Ok(ResponsesAttempt::from_prepared(
        prepared,
        typed_id::<AttemptId>(14)?,
        cancellation,
        Instant::now() + Duration::from_secs(5),
    ))
}

#[derive(Debug, Default)]
struct CountingPoisonLoweringFixture {
    lowerings: AtomicUsize,
    serializations: AtomicUsize,
}

impl CountingPoisonLoweringFixture {
    fn lower(
        &self,
        request: &ModelRequest,
        config: ResponsesRequestConfig<'_>,
    ) -> Result<LoweredResponsesRequest, ResponsesRequestError> {
        assert_eq!(
            self.lowerings.fetch_add(1, Ordering::SeqCst),
            0,
            "a second lowering is poisoned"
        );
        lower_responses_request(request, config)
    }

    fn serialize(
        &self,
        lowered: &LoweredResponsesRequest,
    ) -> Result<Vec<u8>, ResponsesRequestError> {
        assert_eq!(
            self.serializations.fetch_add(1, Ordering::SeqCst),
            0,
            "a second serialization is poisoned"
        );
        lowered.body_bytes()
    }
}

#[test]
fn preparation_lowers_and_serializes_exactly_once() -> Result<(), TestError> {
    let authorization = ResponsesAuthorizationAdapter::new(1, 1, test_preflight, test_revalidate)?;
    let client = Client::new(Limits::default())?;
    let data = attempt_data(&client, &authorization)?;
    let (request, authority) = exact_request_authority()?;
    let fixture = CountingPoisonLoweringFixture::default();

    let prepared = prepare_responses_attempt_with(
        authority,
        &request,
        None,
        &data,
        |request, config| fixture.lower(request, config),
        |lowered| fixture.serialize(lowered),
    )?;
    assert_eq!(fixture.lowerings.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.serializations.load(Ordering::SeqCst), 1);

    let attempt = ResponsesAttempt::from_prepared(
        prepared,
        typed_id::<AttemptId>(14)?,
        CancellationToken::new(),
        Instant::now() + Duration::from_secs(1),
    );
    assert!(!attempt.body.is_empty());
    assert_eq!(fixture.lowerings.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.serializations.load(Ordering::SeqCst), 1);
    Ok(())
}

#[test]
fn dropping_prepared_request_before_attempt_has_zero_dispatch() -> Result<(), TestError> {
    let generation = openai_generation_fingerprint(b"drop-before-attempt");
    let dialect = Arc::new(LifecycleDialect {
        generation,
        cancel_after_authorization: false,
        preflights: AtomicUsize::new(0),
        final_authorizations: AtomicUsize::new(0),
    });
    let client = Client::new(Limits::default())?;
    let authorization =
        ResponsesAuthorizationAdapter::new(1, 1, lifecycle_preflight, lifecycle_revalidate)?;
    let data = attempt_data(&client, &authorization)?;
    let prepared = prepared_attempt(&data)?;
    let _service = ResponsesAttemptService::new(client, Arc::clone(&dialect), data, authorization)?;

    drop(prepared);

    assert_eq!(dialect.preflights.load(Ordering::SeqCst), 0);
    assert_eq!(dialect.final_authorizations.load(Ordering::SeqCst), 0);
    Ok(())
}

#[test]
fn attempt_controls_do_not_change_prepared_body_or_operation_binding() -> Result<(), TestError> {
    let authorization = ResponsesAuthorizationAdapter::new(1, 1, test_preflight, test_revalidate)?;
    let client = Client::new(Limits::default())?;
    let data = attempt_data(&client, &authorization)?;
    let first_prepared = prepared_attempt(&data)?;
    let second_prepared = prepared_attempt(&data)?;
    let attempt_id = typed_id::<AttemptId>(14)?;
    let first_cancellation = CancellationToken::new();
    let second_cancellation = CancellationToken::new();
    second_cancellation.cancel();

    let first = ResponsesAttempt::from_prepared(
        first_prepared,
        attempt_id,
        first_cancellation,
        Instant::now() + Duration::from_secs(1),
    );
    let second = ResponsesAttempt::from_prepared(
        second_prepared,
        attempt_id,
        second_cancellation,
        Instant::now() + Duration::from_secs(9),
    );

    assert_eq!(first.binding, second.binding);
    assert_eq!(first.body, second.body);
    assert!(!first.cancellation.is_cancelled());
    assert!(second.cancellation.is_cancelled());
    assert_ne!(first.deadline, second.deadline);
    Ok(())
}

#[test]
fn execution_identity_binds_transport_policy_limits_and_authorization_revisions()
-> Result<(), TestError> {
    let authorization = ResponsesAuthorizationAdapter::new(1, 1, test_preflight, test_revalidate)?;
    let revised = ResponsesAuthorizationAdapter::new(2, 1, test_preflight, test_revalidate)?;
    let client = Client::new(Limits::default())?;
    let http1 = Client::new_http1(Limits::default())?;
    let mut changed_limits = Limits::default();
    changed_limits.max_response_chunk_bytes /= 2;
    let changed = Client::new(changed_limits)?;

    let base = attempt_data(&client, &authorization)?;
    assert_eq!(base.execution().version(), 3);
    let gateway = base
        .clone()
        .with_router_policy(ResponsesRouterPolicy::gateway_managed(None, None)?)?;
    let strict = base
        .clone()
        .with_router_policy(ResponsesRouterPolicy::strict(
            "anthropic/claude",
            "deny",
            true,
        )?)?;
    assert_ne!(base.execution(), gateway.execution());
    assert_ne!(strict.execution(), gateway.execution());
    let direct = prepared_attempt(&base)?;
    let routed = prepared_attempt(&gateway)?;
    let routed_body: serde_json::Value = serde_json::from_slice(&routed.body())?;
    assert_eq!(
        routed_body["provider"],
        serde_json::json!({
            "allow_fallbacks": false,
            "require_parameters": true,
        })
    );
    assert_ne!(
        direct.operation().request_body(),
        routed.operation().request_body()
    );
    assert_eq!(routed.operation().execution(), gateway.execution());
    let descriptor = test_descriptor()?;
    let revisions = ResponsesSemanticRevisions::new(1, 1, 1)?;
    let account_v1 = Digest::from_sha256_bytes(DigestDomain::ModelBinding, 1, [0x37; 32])?;
    let account_v2 = Digest::from_sha256_bytes(DigestDomain::ModelBinding, 2, [0x37; 32])?;
    assert_ne!(
        ResponsesAttemptTarget::from_admitted_target(&descriptor, account_v1, revisions)?,
        ResponsesAttemptTarget::from_admitted_target(&descriptor, account_v2, revisions)?,
        "target identity includes nested digest version as well as raw hash bytes"
    );
    let mut mismatched_replay = base.clone();
    mismatched_replay.replay = ResponsesReplayProjection::new(
        ProtocolId::new("openai.responses")?,
        None,
        ModelId::new("gpt-other")?,
        ReplayKindId::new("responses.output_item")?,
        1,
    )?;
    assert!(
        executable_attempt(&mismatched_replay, CancellationToken::new()).is_err(),
        "replay projection coordinates cannot cross-product another admitted target"
    );
    assert_ne!(
        base.execution(),
        attempt_data(&http1, &authorization)?.execution()
    );
    assert_ne!(
        base.execution(),
        attempt_data(&changed, &authorization)?.execution()
    );
    assert_ne!(
        base.execution(),
        attempt_data(&client, &revised)?.execution()
    );
    assert!(
        ResponsesAttemptService::new(changed, Arc::new(TestDialect), base.clone(), authorization,)
            .is_err()
    );
    assert!(ResponsesAttemptService::new(client, Arc::new(TestDialect), base, revised,).is_err());
    Ok(())
}

#[tokio::test]
async fn caller_tls_config_requires_and_binds_an_exact_immutable_identity() -> TestResult {
    let authorization = ResponsesAuthorizationAdapter::new(1, 1, test_preflight, test_revalidate)?;
    let first = wire_endpoint().await?;
    let second = wire_endpoint().await?;
    let first_client = Client::with_tls_config_and_identity(
        Limits::default(),
        first.client_config,
        first.tls_identity,
    )?;
    let second_client = Client::with_tls_config_and_identity(
        Limits::default(),
        second.client_config,
        second.tls_identity,
    )?;
    assert_ne!(
        attempt_data(&first_client, &authorization)?.execution(),
        attempt_data(&second_client, &authorization)?.execution(),
        "different caller trust configurations cannot share admitted execution identity"
    );

    let unsealed = wire_endpoint().await?;
    let unsealed_client = Client::with_tls_config(Limits::default(), unsealed.client_config)?;
    assert!(attempt_data(&unsealed_client, &authorization).is_err());
    Ok(())
}

#[tokio::test]
async fn openai_platform_stream_model_cannot_cross_product_another_admitted_target() -> TestResult {
    let mismatched_body = String::from_utf8(complete_sse_body())?
        .replace("gpt-test", "gpt-other")
        .into_bytes();
    let (client_config, tls_identity, origin, peer) = responding_wire_peer(http_response(
        "200 OK",
        Some("text/event-stream"),
        &mismatched_body,
    ))
    .await?;
    let client =
        Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
    let authorization = ResponsesAuthorizationAdapter::new(1, 1, wire_preflight, wire_revalidate)?;
    let stream = ResponsesStreamDialectData::new(
        ResponsesDialectProfile::openai_platform(),
        LifecycleDialectPolicy::openai_platform_api(),
        "gpt-other",
        crate::ResponsesUsagePolicy::DeriveTotalWhenOmitted,
        ResponsesResponseLimits::default(),
        crate::ResponsesStreamCapabilities::codex(),
    );
    let data = attempt_data_with_stream(
        &client,
        &authorization,
        ResponsesProviderErrorPolicy::OpenAiCompatible,
        stream,
    )?;
    let attempt = executable_attempt(&data, CancellationToken::new())?;
    let generation = openai_generation_fingerprint(b"target-model-generation");
    let dialect = wire_dialect(origin, generation);
    let service = ResponsesAttemptService::new(client, dialect, data, authorization)?;
    let error = service
        .execute(attempt)
        .await
        .err()
        .ok_or_else(|| io::Error::other("mismatched stream target unexpectedly succeeded"))?;
    assert!(matches!(
        error,
        ResponsesAttemptError::Stream {
            source: ResponsesFamilyStreamError::InvalidField("response.model"),
            ..
        }
    ));
    assert_eq!(
        error.facts().provider_acceptance(),
        ProviderAcceptance::Accepted
    );
    assert_eq!(peer.await??.1, 1);
    Ok(())
}

#[tokio::test]
async fn openai_codex_preserves_provider_reported_stream_model_distinct_from_admitted_target()
-> TestResult {
    let mismatched_body = String::from_utf8(complete_sse_body())?
        .replace("gpt-test", "gpt-other")
        .into_bytes();
    let (client_config, tls_identity, origin, peer) = responding_wire_peer(http_response(
        "200 OK",
        Some("text/event-stream"),
        &mismatched_body,
    ))
    .await?;
    let client =
        Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
    let authorization = ResponsesAuthorizationAdapter::new(1, 1, wire_preflight, wire_revalidate)?;
    let data = attempt_data_with_policies(
        &client,
        &authorization,
        ResponsesProviderErrorPolicy::OpenAiCompatible,
        "gpt-other",
    )?;
    let attempt = executable_attempt(&data, CancellationToken::new())?;
    let generation = openai_generation_fingerprint(b"codex-reported-model-generation");
    let dialect = wire_dialect(origin, generation);
    let service = ResponsesAttemptService::new(client, dialect, data, authorization)?;
    let response = service.execute(attempt).await.map_err(|error| {
        io::Error::other(format!("Codex ProviderReported must complete: {error:?}"))
    })?;
    assert!(response.terminal().is_completed());
    assert_eq!(peer.await??.1, 1);
    Ok(())
}

#[tokio::test]
async fn cancellation_after_final_authorization_starts_transport_and_stays_ambiguous()
-> Result<(), TestError> {
    let generation = openai_generation_fingerprint(b"lifecycle-generation");
    let dialect = Arc::new(LifecycleDialect {
        generation,
        cancel_after_authorization: true,
        preflights: AtomicUsize::new(0),
        final_authorizations: AtomicUsize::new(0),
    });
    let client = Client::new(Limits::default())?;
    let authorization =
        ResponsesAuthorizationAdapter::new(1, 1, lifecycle_preflight, lifecycle_revalidate)?;
    let data = attempt_data(&client, &authorization)?;
    let cancellation = CancellationToken::new();
    let attempt = executable_attempt(&data, cancellation)?;
    let service = ResponsesAttemptService::new(client, Arc::clone(&dialect), data, authorization)?;

    let Err(error) = service.execute(attempt).await else {
        panic!("authorization-triggered cancellation must terminalize");
    };
    assert!(matches!(error, ResponsesAttemptError::Cancelled { .. }));
    assert_eq!(error.facts().physical_attempts(), 1);
    assert_eq!(
        error.facts().provider_acceptance(),
        ProviderAcceptance::PossiblyAccepted
    );
    assert_eq!(error.retry_class(), RetryClass::Never);
    assert_eq!(dialect.preflights.load(Ordering::SeqCst), 1);
    assert_eq!(dialect.final_authorizations.load(Ordering::SeqCst), 1);
    let Some(physical) = error.physical() else {
        panic!("physical authority must survive");
    };
    assert_eq!(
        physical
            .credential_authority()
            .generation(ResponsesAuthSlot::Secondary),
        Some(generation)
    );
    Ok(())
}

#[tokio::test]
async fn post_authorization_local_limit_cannot_invent_no_dispatch_or_retry() -> Result<(), TestError>
{
    let generation = openai_generation_fingerprint(b"invalid-wire-generation");
    let dialect = Arc::new(LifecycleDialect {
        generation,
        cancel_after_authorization: false,
        preflights: AtomicUsize::new(0),
        final_authorizations: AtomicUsize::new(0),
    });
    let limits = Limits {
        max_request_body_bytes: 1,
        ..Limits::default()
    };
    let client = Client::new(limits)?;
    let authorization =
        ResponsesAuthorizationAdapter::new(1, 1, lifecycle_preflight, lifecycle_revalidate)?;
    let data = attempt_data(&client, &authorization)?;
    let attempt = executable_attempt(&data, CancellationToken::new())?;
    let service = ResponsesAttemptService::new(client, Arc::clone(&dialect), data, authorization)?;

    let Err(error) = service.execute(attempt).await else {
        panic!("client-side request bound must reject the physical exchange");
    };
    assert!(matches!(
        &error,
        ResponsesAttemptError::Transport { source, .. }
            if source.transmission() == plakous_http::TransmissionFact::NotStarted
    ));
    assert_eq!(
        error.facts().provider_acceptance(),
        ProviderAcceptance::PossiblyAccepted,
        "irreversible final authorization forbids converting transport telemetry into no-dispatch"
    );
    assert_eq!(
        error.facts().retry_class(),
        RetryClass::OperationOwnedDuplicateRisk,
        "acceptance facts independently retain post-authorization uncertainty"
    );
    assert_eq!(
        error.retry_class(),
        RetryClass::Never,
        "the deterministic local limit is permanent as well as acceptance-ambiguous"
    );
    assert_eq!(dialect.preflights.load(Ordering::SeqCst), 1);
    assert_eq!(dialect.final_authorizations.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn physical_success_is_one_exact_write_and_terminal_ignores_full_observations()
-> Result<(), TestError> {
    let response = http_response("200 OK", Some("text/event-stream"), &complete_sse_body());
    let (client_config, tls_identity, origin, peer) = responding_wire_peer(response).await?;
    let client =
        Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
    let generation = openai_generation_fingerprint(b"wire-generation");
    let dialect = wire_dialect(origin, generation);
    let (service, attempt) = wire_service_and_attempt(
        client,
        Arc::clone(&dialect),
        CancellationToken::new(),
        Instant::now() + Duration::from_secs(5),
    )?;
    let retained_body = attempt.body.clone();
    let retained_body_binding = attempt.binding().operation().request_body();
    let (observations, _receiver) = mpsc::channel(1);
    let response = service
        .execute(attempt.with_observations(observations))
        .await?;
    assert!(response.terminal().is_completed());
    let ResponsesAttemptTerminal::Completed(terminal) = response.terminal() else {
        panic!("completed stream must project a completed terminal");
    };
    assert_eq!(
        terminal
            .provider_request()
            .canonical_provider_request_id()
            .as_ref()
            .map(ModelProviderRequestId::as_str),
        Some("req_attempt_host_exact")
    );
    assert_eq!(response.observations_delivered(), 1);
    assert_eq!(response.observations_dropped(), 1);
    assert_eq!(response.facts().physical_attempts(), 1);
    assert_eq!(dialect.preflights.load(Ordering::SeqCst), 1);
    assert_eq!(dialect.final_authorizations.load(Ordering::SeqCst), 1);
    assert_eq!(
        response
            .physical()
            .credential_authority()
            .generation(ResponsesAuthSlot::Secondary),
        Some(generation)
    );
    let (captured, accepts) = peer.await??;
    let body_start = captured
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| io::Error::other("captured request omitted header terminator"))?
        + 4;
    let sent_body = &captured[body_start..];
    assert_eq!(sent_body, retained_body.as_ref());
    assert_eq!(request_body_digest(sent_body)?, retained_body_binding);
    let captured = String::from_utf8(captured)?;
    assert!(captured.starts_with("POST /responses?attempt=exact HTTP/1.1\r\n"));
    assert!(captured.contains("authorization: Bearer exact-test-secret\r\n"));
    assert!(captured.contains("\"model\":\"gpt-test\""));
    assert_eq!(accepts, 1, "one service execute is one physical exchange");
    Ok(())
}

#[tokio::test]
async fn redirect_and_status_are_returned_once_without_fallback_or_retry() -> Result<(), TestError>
{
    let redirect_target = TcpListener::bind("127.0.0.1:0").await?;
    let redirect = format!(
        "HTTP/1.1 302 Found\r\nLocation: https://{}/other\r\nContent-Length: 0\r\n\r\n",
        redirect_target.local_addr()?
    )
    .into_bytes();
    let (client_config, tls_identity, origin, peer) = responding_wire_peer(redirect).await?;
    let client =
        Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
    let generation = openai_generation_fingerprint(b"redirect-generation");
    let dialect = wire_dialect(origin, generation);
    let (service, attempt) = wire_service_and_attempt(
        client,
        dialect,
        CancellationToken::new(),
        Instant::now() + Duration::from_secs(5),
    )?;
    let Err(error) = service.execute(attempt).await else {
        panic!("redirect must be a terminal status error");
    };
    assert!(matches!(
        error,
        ResponsesAttemptError::HttpStatus {
            status: StatusCode::FOUND,
            class: ResponsesHttpStatusClass::Unknown,
            ..
        }
    ));
    assert_eq!(
        error.facts().provider_acceptance(),
        ProviderAcceptance::PossiblyAccepted
    );
    assert_eq!(error.facts().physical_attempts(), 1);
    assert!(
        timeout(Duration::from_millis(150), redirect_target.accept())
            .await
            .is_err(),
        "the host must not follow the redirect"
    );
    assert_eq!(peer.await??.1, 1);

    let body = br#"{"error":{"message":"conflict"}}"#;
    let (client_config, tls_identity, origin, peer) = responding_wire_peer(http_response(
        "409 Conflict",
        Some("application/json"),
        body,
    ))
    .await?;
    let client =
        Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
    let dialect = wire_dialect(origin, generation);
    let (service, attempt) = wire_service_and_attempt(
        client,
        dialect,
        CancellationToken::new(),
        Instant::now() + Duration::from_secs(5),
    )?;
    let Err(error) = service.execute(attempt).await else {
        panic!("409 must remain a terminal ambiguous status error");
    };
    assert!(matches!(
        error,
        ResponsesAttemptError::HttpStatus {
            status: StatusCode::CONFLICT,
            class: ResponsesHttpStatusClass::Rejected,
            ..
        }
    ));
    assert_eq!(
        error.facts().provider_acceptance(),
        ProviderAcceptance::PossiblyAccepted
    );
    assert_eq!(peer.await??.1, 1);

    let grok_body = br#"{"error":{"type":"unauthorized","message":"not classified"}}"#;
    let (client_config, tls_identity, origin, peer) = responding_wire_peer(http_response(
        "401 Unauthorized",
        Some("application/json"),
        grok_body,
    ))
    .await?;
    let client =
        Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
    let dialect = wire_dialect(origin, generation);
    let (service, attempt) = wire_service_and_attempt_with_error_policy(
        client,
        dialect,
        CancellationToken::new(),
        Instant::now() + Duration::from_secs(5),
        ResponsesProviderErrorPolicy::Grok,
    )?;
    let Err(error) = service.execute(attempt).await else {
        panic!("Grok status error must remain terminal");
    };
    assert!(matches!(
        error,
        ResponsesAttemptError::HttpStatus {
            provider: Some(ref provider),
            ..
        } if provider.class() == ResponsesProviderErrorClass::Unknown
    ));
    assert_eq!(peer.await??.1, 1);
    Ok(())
}

#[tokio::test]
async fn a_401_triggers_one_refresh_and_retry() -> Result<(), TestError> {
    // With the dialect's credential-rejection hook installed, an HTTP 401
    // head forces one refresh and exactly one re-dispatch of the same logical
    // operation; the renewed grant then completes normally.
    let unauthorized = http_response(
        "401 Unauthorized",
        Some("application/json"),
        br#"{"error":{"message":"token expired"}}"#,
    );
    let accepted = http_response("200 OK", Some("text/event-stream"), &complete_sse_body());
    let (client_config, tls_identity, origin, peer) =
        responding_wire_peer_sequence(vec![unauthorized.clone(), accepted]).await?;
    let client =
        Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
    let generation = openai_generation_fingerprint(b"retry-generation");
    let dialect = wire_dialect(origin, generation);
    let authorization = ResponsesAuthorizationAdapter::new(1, 1, wire_preflight, wire_revalidate)?
        .with_credential_rejected(wire_credential_rejected);
    let data = attempt_data(&client, &authorization)?;
    let attempt = executable_attempt(&data, CancellationToken::new())?;
    let ledger = attempt.core.observations();
    let service = ResponsesAttemptService::new(client, Arc::clone(&dialect), data, authorization)?;
    let response = service.execute(attempt).await?;
    assert!(!ledger.is_service_admitted());
    assert!(ledger.is_logically_terminal());
    assert!(response.facts().provider_accepted());
    assert_eq!(dialect.credential_rejections.load(Ordering::SeqCst), 1);
    assert_eq!(dialect.preflights.load(Ordering::SeqCst), 2);
    assert_eq!(dialect.final_authorizations.load(Ordering::SeqCst), 2);
    assert_eq!(response.facts().physical_attempts(), 2);
    assert_eq!(response.physical().physical().generation(), 2);
    assert_eq!(response.binding().attempt_id(), typed_id::<AttemptId>(14)?);
    let requests = peer.await??;
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0]
            .windows(b"Bearer exact-test-secret".len())
            .any(|v| v == b"Bearer exact-test-secret")
    );
    assert!(
        requests[1]
            .windows(b"Bearer renewed-test-secret".len())
            .any(|v| v == b"Bearer renewed-test-secret")
    );

    // A second 401 is returned as-is: the hook runs once and there is no
    // further re-dispatch.
    let (client_config, tls_identity, origin, peer) =
        responding_wire_peer_sequence(vec![unauthorized.clone(), unauthorized]).await?;
    let client =
        Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
    let dialect = wire_dialect(origin, generation);
    let authorization = ResponsesAuthorizationAdapter::new(1, 1, wire_preflight, wire_revalidate)?
        .with_credential_rejected(wire_credential_rejected);
    let data = attempt_data(&client, &authorization)?;
    let attempt = executable_attempt(&data, CancellationToken::new())?;
    let ledger = attempt.core.observations();
    let service = ResponsesAttemptService::new(client, Arc::clone(&dialect), data, authorization)?;
    let Err(error) = service.execute(attempt).await else {
        panic!("a repeated 401 must remain a terminal status error");
    };
    assert!(!ledger.is_service_admitted());
    assert!(ledger.is_logically_terminal());
    assert!(matches!(
        error,
        ResponsesAttemptError::HttpStatus {
            status: StatusCode::UNAUTHORIZED,
            class: ResponsesHttpStatusClass::Authentication,
            ..
        }
    ));
    assert_eq!(dialect.credential_rejections.load(Ordering::SeqCst), 1);
    assert_eq!(dialect.preflights.load(Ordering::SeqCst), 2);
    assert_eq!(error.facts().physical_attempts(), 2);
    assert_eq!(
        error
            .physical()
            .map(|physical| physical.physical().generation()),
        Some(2)
    );
    assert_eq!(
        error
            .physical()
            .map(|physical| physical.physical().attempt().attempt_id()),
        Some(typed_id::<AttemptId>(14)?)
    );
    let requests = peer.await??;
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0]
            .windows(b"Bearer exact-test-secret".len())
            .any(|v| v == b"Bearer exact-test-secret")
    );
    assert!(
        requests[1]
            .windows(b"Bearer renewed-test-secret".len())
            .any(|v| v == b"Bearer renewed-test-secret")
    );
    Ok(())
}

fn refusal(status: &str, retry_after: &str) -> Vec<u8> {
    http_response_with(
        status,
        Some("application/json"),
        &format!("Retry-After: {retry_after}\r\n"),
        br#"{"error":{"message":"busy"}}"#,
    )
}

struct TerminalFailingObserver {
    required: bool,
    terminals: Arc<AtomicUsize>,
}

impl plakous_http::ExchangeObserver for TerminalFailingObserver {
    fn required(&self) -> bool {
        self.required
    }

    fn start(&self, _: &[u8]) -> Option<Box<dyn plakous_http::ExchangeObservation>> {
        Some(Box::new(TerminalFailingObservation {
            failed: false,
            terminals: Arc::clone(&self.terminals),
        }))
    }
}

struct TerminalFailingObservation {
    failed: bool,
    terminals: Arc<AtomicUsize>,
}

impl plakous_http::ExchangeObservation for TerminalFailingObservation {
    fn failed(&self) -> bool {
        self.failed
    }

    fn response_head(&mut self, _: u16) {}
    fn response_chunk(&mut self, _: &[u8]) {}

    fn terminal(&mut self, terminal: &plakous_http::ExchangeTerminal) {
        assert_ne!(*terminal, plakous_http::ExchangeTerminal::Abandoned);
        self.terminals.fetch_add(1, Ordering::SeqCst);
        self.failed = true;
    }
}

#[tokio::test]
async fn required_capture_terminal_failure_prevents_typed_terminal_success_without_eof()
-> TestResult {
    for required in [false, true] {
        let WireEndpoint {
            listener,
            acceptor,
            client_config,
            tls_identity,
            origin,
        } = wire_endpoint().await?;
        let peer = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let mut stream = acceptor.accept(stream).await?;
            read_wire_request(&mut stream).await?;
            let body = complete_sse_body();
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n",
                body.len() + 1_024
            );
            stream.write_all(head.as_bytes()).await?;
            stream.write_all(&body).await?;
            stream.flush().await?;
            // Do not send EOF: the dialect must explicitly close at the typed terminal.
            let mut byte = [0_u8; 1];
            assert!(!matches!(stream.read(&mut byte).await, Ok(1)));
            Ok::<_, TestError>(())
        });
        let terminals = Arc::new(AtomicUsize::new(0));
        let client =
            Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?
                .with_observer(Some(Arc::new(TerminalFailingObserver {
                    required,
                    terminals: Arc::clone(&terminals),
                })));
        let dialect = wire_dialect(origin, openai_generation_fingerprint(b"capture-terminal"));
        let (service, attempt) = wire_service_and_attempt(
            client,
            Arc::clone(&dialect),
            CancellationToken::new(),
            Instant::now() + Duration::from_secs(5),
        )?;
        let ledger = attempt.core.observations();
        let result = timeout(Duration::from_secs(1), service.execute(attempt)).await?;
        if required {
            let Err(error) = result else {
                panic!("required terminal capture must prevent success");
            };
            assert!(
                matches!(error, ResponsesAttemptError::Transport { source, .. }
                if source.kind() == HttpErrorKind::Internal
                    && source.phase() == plakous_http::Phase::Cleanup)
            );
            assert_eq!(error.facts().physical_attempts(), 1);
        } else {
            assert!(result?.terminal().is_completed());
        }
        assert!(ledger.is_logically_terminal());
        assert_eq!(dialect.preflights.load(Ordering::SeqCst), 1);
        assert_eq!(terminals.load(Ordering::SeqCst), 1);
        timeout(Duration::from_secs(1), peer).await???;
    }
    Ok(())
}

#[tokio::test]
async fn required_capture_terminal_failure_on_refusal_never_retries() -> TestResult {
    // EOF fails in read_error_body; truncated framing fails ordinarily first,
    // with the sticky required sink failure surfaced by explicit close instead.
    for truncated in [false, true] {
        for required in [false, true] {
            let response = if truncated {
                b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 1024\r\nRetry-After: 0\r\n\r\n"
                    .to_vec()
            } else {
                refusal("429 Too Many Requests", "0")
            };
            let sends = if required { 1 } else { 4 };
            let (client_config, tls_identity, origin, peer) =
                responding_wire_peer_sequence(vec![response; sends]).await?;
            let terminals = Arc::new(AtomicUsize::new(0));
            let client = Client::with_tls_config_and_identity(
                Limits::default(),
                client_config,
                tls_identity,
            )?
            .with_observer(Some(Arc::new(TerminalFailingObserver {
                required,
                terminals: Arc::clone(&terminals),
            })));
            let dialect = wire_dialect(origin, openai_generation_fingerprint(b"capture-refusal"));
            let authorization =
                ResponsesAuthorizationAdapter::new(1, 1, wire_preflight, wire_revalidate)?;
            let data = attempt_data(&client, &authorization)?;
            let (sender, mut observations) = mpsc::channel(4);
            let attempt =
                executable_attempt(&data, CancellationToken::new())?.with_observations(sender);
            let ledger = attempt.core.observations();
            let service =
                ResponsesAttemptService::new(client, Arc::clone(&dialect), data, authorization)?;
            let Err(error) = service.execute(attempt).await else {
                panic!("refusal must fail");
            };
            if required {
                assert!(
                    matches!(error, ResponsesAttemptError::Transport { source, .. }
                    if source.kind() == HttpErrorKind::Internal)
                );
                assert!(observations.try_recv().is_err());
            } else {
                assert!(matches!(error, ResponsesAttemptError::HttpStatus {
                    status: StatusCode::TOO_MANY_REQUESTS, ref provider, ..
                } if provider.is_none() == truncated));
                assert!(matches!(
                    observations.try_recv(),
                    Ok(ResponsesObservation::Retry(_))
                ));
            }
            assert!(ledger.is_logically_terminal());
            assert_eq!(error.facts().physical_attempts() as usize, sends);
            assert_eq!(dialect.preflights.load(Ordering::SeqCst), sends);
            assert_eq!(terminals.load(Ordering::SeqCst), sends);
            assert_eq!(peer.await??.len(), sends);
        }
    }
    Ok(())
}

/// Pre-acceptance refusals are re-sent under the shared policy, each with a
/// fresh preflight and final authorization, and each announced by a typed
/// retry ordered before the accepted stream's events.
#[tokio::test]
async fn refusals_before_acceptance_retry_until_accepted_with_retry_events() -> Result<(), TestError>
{
    let accepted = http_response("200 OK", Some("text/event-stream"), &complete_sse_body());
    let (client_config, tls_identity, origin, peer) = responding_wire_peer_sequence(vec![
        refusal("429 Too Many Requests", "0"),
        refusal("503 Service Unavailable", "0"),
        refusal("529 Overloaded", "0"),
        accepted,
    ])
    .await?;
    let client =
        Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
    let dialect = wire_dialect(origin, openai_generation_fingerprint(b"retry-generation"));
    let authorization = ResponsesAuthorizationAdapter::new(1, 1, wire_preflight, wire_revalidate)?;
    let data = attempt_data(&client, &authorization)?;
    let (sender, mut observations) = mpsc::channel(64);
    let attempt = executable_attempt(&data, CancellationToken::new())?.with_observations(sender);
    let service = ResponsesAttemptService::new(client, Arc::clone(&dialect), data, authorization)?;
    let response = service.execute(attempt).await?;
    assert_eq!(response.facts().physical_attempts(), 4);
    assert_eq!(response.physical().physical().generation(), 4);
    assert_eq!(dialect.preflights.load(Ordering::SeqCst), 4);
    assert_eq!(dialect.final_authorizations.load(Ordering::SeqCst), 4);
    let mut seen = Vec::new();
    let mut events_before_last_retry = 0;
    while let Ok(observation) = observations.try_recv() {
        match observation {
            ResponsesObservation::Retry(retry) if retry.loss().declined_reason.is_none() => {
                assert!(!retry.output_discarded());
                assert_eq!(retry.delay(), Duration::ZERO);
                seen.push((retry.next_physical(), retry.max_physical(), retry.cause()));
            }
            ResponsesObservation::Activity(_) | ResponsesObservation::Retry(_) => {}
            ResponsesObservation::Event(_) => {
                assert_eq!(seen.len(), 3, "events follow every retry");
                events_before_last_retry += 1;
            }
        }
    }
    assert!(events_before_last_retry > 0);
    assert_eq!(
        seen,
        [
            (2, 4, ModelRetryCause::HttpStatus { status: 429 }),
            (3, 4, ModelRetryCause::HttpStatus { status: 503 }),
            (4, 4, ModelRetryCause::HttpStatus { status: 529 }),
        ]
    );
    assert_eq!(peer.await??.len(), 4);
    Ok(())
}

/// Four refusals or post-send 5xx exhaust the budget; an unnamed status, or
/// a post-send status on a server-effect request, is never re-sent.
#[tokio::test]
async fn the_retry_budget_ends_on_the_last_failure_and_server_effects_are_not_replayed()
-> Result<(), TestError> {
    for (responses, sends, status, safety) in [
        (
            vec![refusal("429 Too Many Requests", "0"); 4],
            4,
            StatusCode::TOO_MANY_REQUESTS,
            ReplaySafety::CostOnly,
        ),
        (
            vec![refusal("502 Bad Gateway", "0"); 4],
            4,
            StatusCode::BAD_GATEWAY,
            ReplaySafety::CostOnly,
        ),
        (
            vec![refusal("400 Bad Request", "0")],
            1,
            StatusCode::BAD_REQUEST,
            ReplaySafety::CostOnly,
        ),
        (
            vec![refusal("502 Bad Gateway", "0")],
            1,
            StatusCode::BAD_GATEWAY,
            ReplaySafety::ServerEffect,
        ),
    ] {
        let (client_config, tls_identity, origin, peer) =
            responding_wire_peer_sequence(responses).await?;
        let client =
            Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
        let dialect = wire_dialect(origin, openai_generation_fingerprint(b"budget-generation"));
        let authorization =
            ResponsesAuthorizationAdapter::new(1, 1, wire_preflight, wire_revalidate)?;
        let data = attempt_data(&client, &authorization)?;
        let attempt = attempt_with_safety(&data, CancellationToken::new(), safety)?;
        let ledger = attempt.core.observations();
        let service =
            ResponsesAttemptService::new(client, Arc::clone(&dialect), data, authorization)?;
        let Err(error) = service.execute(attempt).await else {
            panic!("HTTP {status} must end the attempt");
        };
        assert!(
            matches!(error, ResponsesAttemptError::HttpStatus { status: got, .. } if got == status)
        );
        assert_eq!(error.facts().physical_attempts(), sends, "HTTP {status}");
        assert!(ledger.is_logically_terminal());
        assert_eq!(peer.await??.len(), sends as usize, "HTTP {status}");
    }
    Ok(())
}

/// Cancellation during a backoff ends the attempt with no late dispatch.
#[tokio::test]
async fn cancellation_during_backoff_sends_nothing_more() -> Result<(), TestError> {
    let (client_config, tls_identity, origin, peer) = responding_wire_peer_sequence(vec![
        refusal("429 Too Many Requests", "30"),
        refusal("429 Too Many Requests", "0"),
    ])
    .await?;
    let client =
        Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
    let dialect = wire_dialect(origin, openai_generation_fingerprint(b"cancel-generation"));
    let authorization = ResponsesAuthorizationAdapter::new(1, 1, wire_preflight, wire_revalidate)?;
    let data = attempt_data(&client, &authorization)?;
    let cancellation = CancellationToken::new();
    // The default fixture deadline is too short for a thirty-second wait.
    let attempt = ResponsesAttempt::from_prepared(
        prepared_attempt(&data)?,
        typed_id::<AttemptId>(14)?,
        cancellation.clone(),
        Instant::now() + Duration::from_secs(120),
    );
    let service = ResponsesAttemptService::new(client, Arc::clone(&dialect), data, authorization)?;
    let operation = tokio::spawn(async move { service.execute(attempt).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    cancellation.cancel();
    let result = timeout(Duration::from_secs(2), operation).await??;
    assert!(matches!(
        result,
        Err(ResponsesAttemptError::Cancelled { .. })
    ));
    assert_eq!(dialect.preflights.load(Ordering::SeqCst), 1);
    peer.abort();
    Ok(())
}

#[tokio::test]
async fn session_workflow_401_never_refreshes_or_retries() -> Result<(), TestError> {
    for error_policy in [
        ResponsesProviderErrorPolicy::OpenAiCompatible,
        ResponsesProviderErrorPolicy::Grok,
    ] {
        let unauthorized = http_response(
            "401 Unauthorized",
            Some("application/json"),
            br#"{"error":{"message":"token expired"}}"#,
        );
        let (client_config, tls_identity, origin, peer) =
            responding_wire_peer(unauthorized).await?;
        let client =
            Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
        let dialect = wire_dialect(
            origin,
            openai_generation_fingerprint(b"workflow-generation"),
        );
        let authorization =
            ResponsesAuthorizationAdapter::new(1, 1, wire_preflight, wire_revalidate)?
                .with_credential_rejected(wire_credential_rejected);
        let data = attempt_data_with_error_policy(&client, &authorization, error_policy)?;
        let metadata = RequestMetadata::for_session_workflow(
            typed_id::<RunId>(10)?,
            SessionWorkflowRootId::new([7; 32])?,
            typed_id::<AttemptId>(12)?,
            Timestamp::new(1_700_000_000, 0)?,
        );
        let (request, authority) = exact_request_authority_with_metadata(false, metadata)?;
        let prepared = prepare_responses_attempt(authority, &request, None, &data)?;
        assert!(!prepared.retry_permitted);
        let attempt = ResponsesAttempt::from_prepared(
            prepared,
            typed_id::<AttemptId>(14)?,
            CancellationToken::new(),
            Instant::now() + Duration::from_secs(5),
        );
        let service =
            ResponsesAttemptService::new(client, Arc::clone(&dialect), data, authorization)?;
        let Err(error) = service.execute(attempt).await else {
            panic!("workflow 401 must remain terminal");
        };
        assert!(matches!(
            error,
            ResponsesAttemptError::HttpStatus {
                status: StatusCode::UNAUTHORIZED,
                ..
            }
        ));
        assert_eq!(error.facts().physical_attempts(), 1);
        assert_eq!(dialect.credential_rejections.load(Ordering::SeqCst), 0);
        assert_eq!(dialect.preflights.load(Ordering::SeqCst), 1);
        assert_eq!(peer.await??.1, 1);
    }
    Ok(())
}

#[tokio::test]
async fn rejected_refresh_returns_original_401_without_second_send() -> Result<(), TestError> {
    let unauthorized = http_response(
        "401 Unauthorized",
        Some("application/json"),
        br#"{"error":{"message":"token expired"}}"#,
    );
    let (client_config, tls_identity, origin, peer) = responding_wire_peer(unauthorized).await?;
    let client =
        Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
    let dialect = wire_dialect(origin, openai_generation_fingerprint(b"failed-refresh"));
    let authorization = ResponsesAuthorizationAdapter::new(1, 1, wire_preflight, wire_revalidate)?
        .with_credential_rejected(failing_wire_credential_rejected);
    let data = attempt_data(&client, &authorization)?;
    let attempt = executable_attempt(&data, CancellationToken::new())?;
    let service = ResponsesAttemptService::new(client, Arc::clone(&dialect), data, authorization)?;
    let Err(error) = service.execute(attempt).await else {
        panic!("refresh refusal must preserve the original 401");
    };
    assert!(matches!(
        error,
        ResponsesAttemptError::HttpStatus {
            status: StatusCode::UNAUTHORIZED,
            class: ResponsesHttpStatusClass::Authentication,
            ..
        }
    ));
    assert_eq!(error.facts().physical_attempts(), 1);
    assert_eq!(dialect.credential_rejections.load(Ordering::SeqCst), 1);
    assert_eq!(dialect.preflights.load(Ordering::SeqCst), 1);
    assert_eq!(peer.await??.1, 1);
    Ok(())
}

#[tokio::test]
async fn pending_401_refresh_obeys_cancellation_deadline_and_drop() -> Result<(), TestError> {
    for stop in 0..3 {
        let unauthorized = http_response(
            "401 Unauthorized",
            Some("application/json"),
            br#"{"error":{"message":"token expired"}}"#,
        );
        let (client_config, tls_identity, origin, peer) =
            responding_wire_peer_sequence(vec![unauthorized]).await?;
        let client =
            Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
        let dialect = wire_dialect(origin, openai_generation_fingerprint(b"pending-refresh"));
        let authorization =
            ResponsesAuthorizationAdapter::new(1, 1, wire_preflight, wire_revalidate)?
                .with_credential_rejected(pending_wire_credential_rejected);
        let data = attempt_data(&client, &authorization)?;
        let cancellation = CancellationToken::new();
        let mut attempt = executable_attempt(&data, cancellation.clone())?;
        attempt.deadline = Instant::now() + Duration::from_millis(500);
        let ledger = attempt.core.observations();
        let service =
            ResponsesAttemptService::new(client, Arc::clone(&dialect), data, authorization)?;
        let task = tokio::spawn(service.execute(attempt));
        timeout(Duration::from_secs(2), async {
            while dialect.credential_rejections.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert!(ledger.is_service_admitted());
        assert!(!ledger.has_active_physical_request());
        assert_eq!(ledger.snapshot().physical_attempts, 1);
        match stop {
            0 => {
                cancellation.cancel();
                let Err(error) = task.await? else {
                    panic!("cancelled refresh must stop");
                };
                assert!(matches!(error, ResponsesAttemptError::Cancelled { .. }));
                assert_eq!(error.facts().physical_attempts(), 1);
            }
            1 => {
                let Err(error) = task.await? else {
                    panic!("expired refresh must stop");
                };
                assert!(matches!(error, ResponsesAttemptError::Deadline { .. }));
                assert_eq!(error.facts().physical_attempts(), 1);
            }
            _ => {
                task.abort();
                let _ = task.await;
            }
        }
        assert!(!ledger.is_service_admitted());
        assert!(ledger.is_logically_terminal());
        assert_eq!(ledger.snapshot().physical_attempts, 1);
        assert_eq!(peer.await??.len(), 1);
        assert_eq!(dialect.preflights.load(Ordering::SeqCst), 1);
    }
    Ok(())
}

#[tokio::test]
async fn preexisting_cancellation_or_deadline_never_calls_refresh_hook() -> Result<(), TestError> {
    for expired in [false, true] {
        let client = Client::new(Limits::default())?;
        let dialect = wire_dialect(
            Origin::parse("https://example.test")?,
            openai_generation_fingerprint(b"preexisting-stop"),
        );
        let authorization =
            ResponsesAuthorizationAdapter::new(1, 1, wire_preflight, wire_revalidate)?
                .with_credential_rejected(wire_credential_rejected);
        let data = attempt_data(&client, &authorization)?;
        let cancellation = CancellationToken::new();
        let mut attempt = executable_attempt(&data, cancellation.clone())?;
        if expired {
            attempt.deadline = Instant::now()
                .checked_sub(Duration::from_millis(1))
                .ok_or_else(|| io::Error::other("monotonic time too early for expired deadline"))?;
            attempt.hard_deadline = Some(attempt.deadline);
        } else {
            cancellation.cancel();
        }
        let service =
            ResponsesAttemptService::new(client, Arc::clone(&dialect), data, authorization)?;
        let Err(error) = service.execute(attempt).await else {
            panic!("preexisting stop must reject before dispatch");
        };
        assert!(matches!(
            error,
            ResponsesAttemptError::Cancelled { .. } | ResponsesAttemptError::Deadline { .. }
        ));
        assert_eq!(error.facts().physical_attempts(), 0);
        assert_eq!(dialect.credential_rejections.load(Ordering::SeqCst), 0);
        assert_eq!(dialect.preflights.load(Ordering::SeqCst), 0);
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn response_head_faults_fail_and_codex_terminal_ends_a_longer_physical_body()
-> Result<(), TestError> {
    // Keep real loopback I/O from auto-advancing the paused clock; retry rows
    // alone advance outage waits after the one-shot malformed-head peer exits.
    let (keep_clock, clock_held) = std::sync::mpsc::channel::<()>();
    let clock_task = tokio::task::spawn_blocking(move || {
        let _ = clock_held.recv();
    });
    let generation = openai_generation_fingerprint(b"fault-generation");
    let (client_config, tls_identity, origin, peer) = responding_wire_peer_sequence(vec![
        b"HTTP/1.1 not-a-status\r\nBroken Header\r\n\r\n".to_vec(),
    ])
    .await?;
    let client =
        Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
    let dialect = wire_dialect(origin, generation);
    let (service, attempt) = wire_service_and_attempt(
        client,
        dialect,
        CancellationToken::new(),
        monotonic_now() + Duration::from_secs(5),
    )?;
    let (sender, mut observations) = mpsc::channel(64);
    let drive_clock = async {
        while let Some(observation) = observations.recv().await {
            if let ResponsesObservation::Retry(retry) = observation
                && retry.loss().declined_reason.is_none()
            {
                // Retry observations round nanosecond jitter down to whole milliseconds.
                tokio::time::advance(retry.delay().saturating_add(Duration::from_millis(1))).await;
            }
        }
    };
    let (result, ()) = tokio::join!(
        service.execute(
            attempt
                .with_observations(sender)
                .with_dispatch_deadlines(Duration::from_secs(5), None)
        ),
        drive_clock
    );
    let Err(error) = result else {
        panic!("malformed response head must fail");
    };
    assert!(matches!(error, ResponsesAttemptError::Transport { .. }));
    assert_eq!(error.facts().physical_attempts(), 13);
    assert_eq!(
        error.facts().provider_acceptance(),
        ProviderAcceptance::PossiblyAccepted
    );
    assert_eq!(peer.await??.len(), 1);

    let body = complete_sse_body();
    let mut truncated = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n",
        body.len() + 17
    )
    .into_bytes();
    truncated.extend_from_slice(&body);
    let (client_config, tls_identity, origin, peer) =
        responding_wire_peer_sequence(vec![truncated]).await?;
    let client =
        Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
    let dialect = wire_dialect(origin, generation);
    let (service, attempt) = wire_service_and_attempt(
        client,
        dialect,
        CancellationToken::new(),
        monotonic_now() + Duration::from_secs(5),
    )?;
    let response = service
        .execute(attempt.with_dispatch_deadlines(Duration::from_secs(5), None))
        .await?;
    assert!(response.terminal().is_completed());
    assert_eq!(
        response.facts().provider_acceptance(),
        ProviderAcceptance::Accepted
    );
    assert_eq!(peer.await??.len(), 1);
    drop(keep_clock);
    clock_task.await?;
    Ok(())
}

#[tokio::test]
async fn malformed_and_missing_streams_fail_while_codex_ignores_post_terminal_tail()
-> Result<(), TestError> {
    let generation = openai_generation_fingerprint(b"stream-fault-generation");
    let malformed = b"event: response.created\ndata: {not-json}\n\n".to_vec();
    let missing = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"in_progress\",\"model\":\"gpt-test\"}}\n\n"
    )
    .as_bytes()
    .to_vec();
    let mut post_terminal = complete_sse_body();
    post_terminal.extend_from_slice(concat!(
        "event: response.in_progress\n",
        "data: {\"type\":\"response.in_progress\",\"sequence_number\":3,\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-test\"}}\n\n"
    ).as_bytes());
    for (body, expected) in [
        (malformed, ResponsesFamilyStreamError::InvalidJson),
        (missing, ResponsesFamilyStreamError::MissingTerminal),
    ] {
        let (client_config, tls_identity, origin, peer) =
            responding_wire_peer(http_response("200 OK", Some("text/event-stream"), &body)).await?;
        let client =
            Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
        let dialect = wire_dialect(origin, generation);
        let (service, mut attempt) = wire_service_and_attempt(
            client,
            dialect,
            CancellationToken::new(),
            Instant::now() + Duration::from_secs(5),
        )?;
        attempt.retry_permitted = false;
        let Err(error) = service.execute(attempt).await else {
            panic!("malformed, missing, and post-terminal streams must fail");
        };
        assert!(matches!(
            error,
            ResponsesAttemptError::Stream { source, .. } if source == expected
        ));
        assert_eq!(error.facts().physical_attempts(), 1);
        assert_eq!(peer.await??.1, 1);
    }
    let (client_config, tls_identity, origin, peer) = responding_wire_peer(http_response(
        "200 OK",
        Some("text/event-stream"),
        &post_terminal,
    ))
    .await?;
    let client =
        Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
    let dialect = wire_dialect(origin, generation);
    let (service, mut attempt) = wire_service_and_attempt(
        client,
        dialect,
        CancellationToken::new(),
        Instant::now() + Duration::from_secs(5),
    )?;
    attempt.retry_permitted = false;
    let response = service.execute(attempt).await?;
    assert!(response.terminal().is_completed());
    assert_eq!(peer.await??.1, 1);
    Ok(())
}

#[tokio::test]
async fn observation_delivery_outcome_cannot_change_post_delta_terminal_facts() -> TestResult {
    let generation = openai_generation_fingerprint(b"observation-terminal-authority-generation");
    let body = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"in_progress\",\"model\":\"gpt-test\"}}\n\n",
        "event: response.output_item.added\n",
        "data: {\"type\":\"response.output_item.added\",\"sequence_number\":1,\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"status\":\"in_progress\",\"role\":\"assistant\",\"content\":[]}}\n\n"
    )
    .as_bytes();
    let mut facts = Vec::new();
    for delivery_mode in 0..3 {
        let (client_config, tls_identity, origin, peer) =
            responding_wire_peer(http_response("200 OK", Some("text/event-stream"), body)).await?;
        let client =
            Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
        let dialect = wire_dialect(origin, generation);
        let (service, mut attempt) = wire_service_and_attempt(
            client,
            dialect,
            CancellationToken::new(),
            Instant::now() + Duration::from_secs(5),
        )?;
        attempt.retry_permitted = false;
        let (attempt, _receiver) = match delivery_mode {
            0 => (attempt, None),
            1 => {
                let (sender, receiver) = mpsc::channel(1);
                (attempt.with_observations(sender), Some(receiver))
            }
            _ => {
                let (sender, receiver) = mpsc::channel(1);
                drop(receiver);
                (attempt.with_observations(sender), None)
            }
        };
        let error = service
            .execute(attempt)
            .await
            .err()
            .ok_or_else(|| io::Error::other("missing terminal unexpectedly succeeded"))?;
        assert!(matches!(
            error,
            ResponsesAttemptError::Stream {
                source: ResponsesFamilyStreamError::MissingTerminal,
                ..
            }
        ));
        facts.push(error.facts());
        assert_eq!(peer.await??.1, 1);
    }
    assert_eq!(facts[0], facts[1]);
    assert_eq!(facts[0], facts[2]);
    assert_eq!(facts[0].provider_acceptance(), ProviderAcceptance::Accepted);
    assert_eq!(facts[0].retry_class(), RetryClass::Never);
    Ok(())
}

#[tokio::test]
async fn closed_observations_and_stale_slots_cannot_change_authority() -> Result<(), TestError> {
    let generation = openai_generation_fingerprint(b"observation-generation");
    let (client_config, tls_identity, origin, peer) = responding_wire_peer(http_response(
        "200 OK",
        Some("text/event-stream"),
        &complete_sse_body(),
    ))
    .await?;
    let client =
        Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
    let dialect = wire_dialect(origin, generation);
    let (service, attempt) = wire_service_and_attempt(
        client,
        dialect,
        CancellationToken::new(),
        Instant::now() + Duration::from_secs(5),
    )?;
    let (observations, receiver) = mpsc::channel(1);
    drop(receiver);
    let response = service
        .execute(attempt.with_observations(observations))
        .await?;
    assert!(response.terminal().is_completed());
    assert_eq!(response.observations_delivered(), 0);
    assert_eq!(response.observations_dropped(), 2);
    assert_eq!(peer.await??.1, 1);

    let endpoint = wire_endpoint().await?;
    let rotated = openai_generation_fingerprint(b"rotated-generation");
    let dialect = Arc::new(WireDialect {
        origin: endpoint.origin,
        generation,
        current_generation: rotated,
        cancel_after_authorization: false,
        preflights: AtomicUsize::new(0),
        final_authorizations: AtomicUsize::new(0),
        credential_rejections: AtomicUsize::new(0),
        guards_dropped: Arc::new(AtomicUsize::new(0)),
    });
    let client = Client::with_tls_config_and_identity(
        Limits::default(),
        endpoint.client_config,
        endpoint.tls_identity,
    )?;
    let (service, attempt) = wire_service_and_attempt(
        client,
        Arc::clone(&dialect),
        CancellationToken::new(),
        Instant::now() + Duration::from_secs(5),
    )?;
    let Err(error) = service.execute(attempt).await else {
        panic!("a stale credential guard must reject before wire dispatch");
    };
    assert!(matches!(
        error,
        ResponsesAttemptError::Revalidation {
            rejection: ResponsesDispatchRejection::Binding,
            ..
        }
    ));
    assert_eq!(
        error.facts().provider_acceptance(),
        ProviderAcceptance::NotAccepted
    );
    assert_eq!(dialect.preflights.load(Ordering::SeqCst), 1);
    assert_eq!(dialect.final_authorizations.load(Ordering::SeqCst), 1);
    assert!(
        timeout(Duration::from_millis(150), endpoint.listener.accept())
            .await
            .is_err(),
        "stale slot authority must not open a connection"
    );
    Ok(())
}

#[tokio::test]
async fn cancellation_and_deadline_phases_are_closed_and_conservative() -> Result<(), TestError> {
    let generation = openai_generation_fingerprint(b"cancel-generation");
    let endpoint = wire_endpoint().await?;
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let dialect = wire_dialect(endpoint.origin, generation);
    let client = Client::with_tls_config_and_identity(
        Limits::default(),
        endpoint.client_config,
        endpoint.tls_identity,
    )?;
    let (service, attempt) = wire_service_and_attempt(
        client,
        Arc::clone(&dialect),
        cancellation,
        Instant::now() + Duration::from_secs(5),
    )?;
    let Err(error) = service.execute(attempt).await else {
        panic!("preflight cancellation must terminate");
    };
    assert!(matches!(error, ResponsesAttemptError::Cancelled { .. }));
    assert_eq!(error.facts().physical_attempts(), 0);
    assert_eq!(dialect.preflights.load(Ordering::SeqCst), 0);
    assert!(
        timeout(Duration::from_millis(100), endpoint.listener.accept())
            .await
            .is_err()
    );

    let endpoint = wire_endpoint().await?;
    let WireEndpoint {
        listener,
        acceptor,
        client_config,
        tls_identity,
        origin,
    } = endpoint;
    let (request_seen_tx, request_seen_rx) = oneshot::channel();
    let peer = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut stream = acceptor.accept(stream).await?;
        let request = read_wire_request(&mut stream).await?;
        request_seen_tx
            .send(())
            .map_err(|()| io::Error::other("request-seen receiver dropped"))?;
        let mut byte = [0_u8; 1];
        let closed = !matches!(stream.read(&mut byte).await, Ok(1));
        Ok::<_, TestError>((request, closed))
    });
    let client =
        Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
    let dialect = wire_dialect(origin, generation);
    let (service, attempt) = wire_service_and_attempt(
        client,
        dialect,
        CancellationToken::new(),
        Instant::now() + Duration::from_secs(1),
    )?;
    let execution = tokio::spawn(async move { service.execute(attempt).await });
    request_seen_rx.await?;
    let error = execution
        .await?
        .err()
        .ok_or_else(|| io::Error::other("response-head deadline unexpectedly succeeded"))?;
    assert!(matches!(error, ResponsesAttemptError::Deadline { .. }));
    assert_eq!(
        error.facts().provider_acceptance(),
        ProviderAcceptance::PossiblyAccepted
    );
    let (_request, closed) = peer.await??;
    assert!(
        closed,
        "deadline cleanup must close and join the connection"
    );
    Ok(())
}

#[tokio::test]
async fn codex_terminal_wins_before_later_body_cancellation() -> Result<(), TestError> {
    let generation = openai_generation_fingerprint(b"body-cancel-generation");
    let endpoint = wire_endpoint().await?;
    let WireEndpoint {
        listener,
        acceptor,
        client_config,
        tls_identity,
        origin,
    } = endpoint;
    let (body_waiting_tx, body_waiting_rx) = oneshot::channel();
    let peer = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut stream = acceptor.accept(stream).await?;
        let request = read_wire_request(&mut stream).await?;
        let buffered_terminal = complete_sse_body();
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n",
            buffered_terminal.len() + 1_024
        );
        stream.write_all(head.as_bytes()).await?;
        stream.write_all(&buffered_terminal).await?;
        stream.flush().await?;
        body_waiting_tx
            .send(())
            .map_err(|()| io::Error::other("body-waiting receiver dropped"))?;
        let mut byte = [0_u8; 1];
        let closed = !matches!(stream.read(&mut byte).await, Ok(1));
        Ok::<_, TestError>((request, closed))
    });
    let client =
        Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
    let dialect = wire_dialect(origin, generation);
    let cancellation = CancellationToken::new();
    let (service, attempt) = wire_service_and_attempt(
        client,
        Arc::clone(&dialect),
        cancellation.clone(),
        Instant::now() + Duration::from_secs(5),
    )?;
    let (observations, mut receiver) = mpsc::channel(4);
    let execution = tokio::spawn(async move {
        service
            .execute(attempt.with_observations(observations))
            .await
    });
    body_waiting_rx.await?;
    timeout(Duration::from_secs(1), receiver.recv())
        .await?
        .ok_or_else(|| io::Error::other("buffered terminal observation was not decoded"))?;
    assert_eq!(
        dialect.guards_dropped.load(Ordering::SeqCst),
        1,
        "the opaque authorization guard lives through response-head resolution and is released before body completion"
    );
    cancellation.cancel();
    let response = execution.await??;
    assert!(response.terminal().is_completed());
    assert_eq!(
        response.facts().provider_acceptance(),
        ProviderAcceptance::Accepted
    );
    assert_eq!(response.facts().physical_attempts(), 1);
    let (_request, closed) = peer.await??;
    assert!(
        closed,
        "body cancellation must close and join the connection"
    );
    Ok(())
}

#[tokio::test]
async fn codex_terminal_wins_before_a_later_body_deadline() -> Result<(), TestError> {
    let generation = openai_generation_fingerprint(b"body-deadline-generation");
    let endpoint = wire_endpoint().await?;
    let WireEndpoint {
        listener,
        acceptor,
        client_config,
        tls_identity,
        origin,
    } = endpoint;
    let (body_waiting_tx, body_waiting_rx) = oneshot::channel();
    let peer = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut stream = acceptor.accept(stream).await?;
        let request = read_wire_request(&mut stream).await?;
        let buffered_terminal = complete_sse_body();
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nX-Request-ID: req_body_deadline\r\nContent-Length: {}\r\n\r\n",
            buffered_terminal.len() + 1_024
        );
        stream.write_all(head.as_bytes()).await?;
        stream.write_all(&buffered_terminal).await?;
        stream.flush().await?;
        body_waiting_tx
            .send(())
            .map_err(|()| io::Error::other("body-waiting receiver dropped"))?;
        let mut byte = [0_u8; 1];
        let closed = !matches!(stream.read(&mut byte).await, Ok(1));
        Ok::<_, TestError>((request, closed))
    });
    let client =
        Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
    let dialect = wire_dialect(origin, generation);
    let (service, attempt) = wire_service_and_attempt(
        client,
        dialect,
        CancellationToken::new(),
        Instant::now() + Duration::from_secs(1),
    )?;
    let (observations, mut receiver) = mpsc::channel(4);
    let execution = tokio::spawn(async move {
        service
            .execute(attempt.with_observations(observations))
            .await
    });
    body_waiting_rx.await?;
    timeout(Duration::from_secs(1), receiver.recv())
        .await?
        .ok_or_else(|| io::Error::other("buffered terminal observation was not decoded"))?;
    let response = execution.await??;
    assert!(response.terminal().is_completed());
    assert_eq!(
        response.facts().provider_acceptance(),
        ProviderAcceptance::Accepted
    );
    assert_eq!(response.facts().physical_attempts(), 1);
    let (_request, closed) = peer.await??;
    assert!(closed, "body deadline must close and join the connection");
    Ok(())
}

/// Serves `body` in `pieces` slices `gap` apart, then `stall`s before the rest.
fn trickle_peer(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    pieces: usize,
    gap: Duration,
    stall_after: Option<usize>,
) -> JoinHandle<Result<(), TestError>> {
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut stream = acceptor.accept(stream).await?;
        let _request = read_wire_request(&mut stream).await?;
        let body = complete_sse_body();
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nX-Request-ID: req_trickle\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).await?;
        stream.flush().await?;
        let size = body.len().div_ceil(pieces);
        for (index, piece) in body.chunks(size).enumerate() {
            if stall_after == Some(index) {
                let mut byte = [0_u8; 1];
                let _ = stream.read(&mut byte).await;
                return Ok(());
            }
            tokio::time::sleep(gap).await;
            stream.write_all(piece).await?;
            stream.flush().await?;
        }
        Ok(())
    })
}

#[tokio::test]
async fn progressing_stream_outlives_the_dispatch_deadline() -> Result<(), TestError> {
    let generation = openai_generation_fingerprint(b"progressing-stream-generation");
    let WireEndpoint {
        listener,
        acceptor,
        client_config,
        tls_identity,
        origin,
    } = wire_endpoint().await?;
    let peer = trickle_peer(listener, acceptor, 10, Duration::from_millis(100), None);
    let mut limits = Limits::for_streaming_inference();
    limits.response_body_timeout = Duration::from_millis(500);
    let client = Client::with_tls_config_and_identity(limits, client_config, tls_identity)?;
    let started = Instant::now();
    let (service, attempt) = wire_service_and_attempt(
        client,
        wire_dialect(origin, generation),
        CancellationToken::new(),
        Instant::now() + Duration::from_millis(400),
    )?;
    let response = service.execute(attempt).await?;
    assert!(response.terminal().is_completed());
    assert!(
        started.elapsed() > Duration::from_millis(800),
        "the stream must run past its dispatch deadline"
    );
    peer.await??;
    Ok(())
}

#[tokio::test]
async fn stalled_stream_fails_at_the_idle_bound() -> Result<(), TestError> {
    let generation = openai_generation_fingerprint(b"stalled-stream-generation");
    let WireEndpoint {
        listener,
        acceptor,
        client_config,
        tls_identity,
        origin,
    } = wire_endpoint().await?;
    let peer = trickle_peer(listener, acceptor, 4, Duration::ZERO, Some(2));
    let mut limits = Limits::for_streaming_inference();
    limits.response_body_timeout = Duration::from_millis(300);
    let client = Client::with_tls_config_and_identity(limits, client_config, tls_identity)?;
    let (service, mut attempt) = wire_service_and_attempt(
        client,
        wire_dialect(origin, generation),
        CancellationToken::new(),
        Instant::now() + Duration::from_secs(30),
    )?;
    attempt.retry_permitted = false;
    let result = timeout(Duration::from_secs(5), service.execute(attempt)).await?;
    assert!(matches!(
        result,
        Err(ResponsesAttemptError::Deadline { .. })
    ));
    peer.await??;
    Ok(())
}

#[test]
fn physical_binding_covers_attempt_generation_headers_path_and_body() -> Result<(), TestError> {
    let physical = attempt_binding(1, 1)?;
    let generation = openai_generation_fingerprint(b"credential generation one");
    let rotated = openai_generation_fingerprint(b"credential generation two");
    let headers = header_bindings();
    let authority = ResponsesCredentialAuthority::secondary(generation);
    let rotated_authority = ResponsesCredentialAuthority::secondary(rotated);
    let base = physical_request_digest(
        &physical,
        &authority,
        &request(b"{}", "Bearer first")?,
        &headers,
    )?;
    assert_ne!(
        base,
        physical_request_digest(
            &attempt_binding(2, 1)?,
            &authority,
            &request(b"{}", "Bearer first")?,
            &headers
        )?
    );
    assert_ne!(
        base,
        physical_request_digest(
            &attempt_binding(1, 2)?,
            &authority,
            &request(b"{}", "Bearer first")?,
            &headers
        )?
    );
    assert_eq!(
        base,
        physical_request_digest(
            &physical,
            &authority,
            &request(b"{}", "Bearer rotated")?,
            &headers
        )?,
        "credential bytes must not become a digest oracle"
    );
    assert_ne!(
        base,
        physical_request_digest(
            &physical,
            &rotated_authority,
            &request(b"{}", "Bearer first")?,
            &headers
        )?,
        "the non-secret credential generation remains exact"
    );
    assert_ne!(
        base,
        physical_request_digest(
            &physical,
            &authority,
            &request(b"{\"x\":1}", "Bearer first")?,
            &headers
        )?
    );
    assert_ne!(
        base,
        physical_request_digest(
            &physical,
            &authority,
            &request_at(
                "https://other.test",
                Method::POST,
                "/responses",
                b"{}",
                "Bearer first",
                "application/json"
            )?,
            &headers
        )?,
        "normalized origin is part of the host-derived route binding"
    );
    assert_ne!(
        base,
        physical_request_digest(
            &physical,
            &authority,
            &request_at(
                "https://example.test",
                Method::POST,
                "/backend-api/codex/responses",
                b"{}",
                "Bearer first",
                "application/json"
            )?,
            &headers
        )?,
        "path is part of the host-derived route binding"
    );
    assert_ne!(
        base,
        physical_request_digest(
            &physical,
            &authority,
            &request_at(
                "https://example.test",
                Method::PUT,
                "/responses",
                b"{}",
                "Bearer first",
                "application/json"
            )?,
            &headers
        )?,
        "method is part of the exact request binding"
    );
    let alternate_public = header_bindings_for(b"application/responses+json");
    assert_ne!(
        base,
        physical_request_digest(
            &physical,
            &authority,
            &request_at(
                "https://example.test",
                Method::POST,
                "/responses",
                b"{}",
                "Bearer first",
                "application/responses+json"
            )?,
            &alternate_public
        )?,
        "pinned public header bytes are bound exactly"
    );
    assert!(matches!(
        physical_request_digest(
            &physical,
            &authority,
            &request_at(
                "https://example.test",
                Method::POST,
                "/responses",
                b"{}",
                "Bearer first",
                "application/responses+json"
            )?,
            &headers
        ),
        Err(ResponsesAttemptBuildError::Binding)
    ));
    Ok(())
}

#[test]
fn dual_slot_binding_is_order_independent_and_rotates_independently() -> Result<(), TestError> {
    let physical = attempt_binding_for_slots(7, 1, ResponsesAuthSlots::PrimaryAndSecondary)?;
    let primary = openai_generation_fingerprint(b"primary-1");
    let primary_rotated = openai_generation_fingerprint(b"primary-2");
    let secondary = openai_generation_fingerprint(b"secondary-1");
    let secondary_rotated = openai_generation_fingerprint(b"secondary-2");
    let primary_account = model_binding(b"primary account")?;
    let secondary_account = model_binding(b"secondary account")?;
    let authority = ResponsesCredentialAuthority::primary_and_secondary(primary, secondary)
        .with_account(ResponsesAccountSlot::Primary, primary_account)?
        .with_account(ResponsesAccountSlot::Secondary, secondary_account)?;
    let rotated_authority =
        ResponsesCredentialAuthority::primary_and_secondary(primary_rotated, secondary)
            .with_account(ResponsesAccountSlot::Primary, primary_account)?
            .with_account(ResponsesAccountSlot::Secondary, secondary_account)?;
    let secondary_rotated_authority =
        ResponsesCredentialAuthority::primary_and_secondary(primary, secondary_rotated)
            .with_account(ResponsesAccountSlot::Primary, primary_account)?
            .with_account(ResponsesAccountSlot::Secondary, secondary_account)?;
    let mut bindings = vec![
        ResponsesHeaderBinding::CredentialSecret {
            name: HeaderName::from_static("x-primary-authorization"),
            slot: ResponsesAuthSlot::Primary,
        },
        ResponsesHeaderBinding::CredentialSecret {
            name: HeaderName::from_static("authorization"),
            slot: ResponsesAuthSlot::Secondary,
        },
        ResponsesHeaderBinding::AccountSecret {
            name: HeaderName::from_static("x-primary-account"),
            slot: ResponsesAccountSlot::Primary,
            authority: primary_account,
        },
        ResponsesHeaderBinding::AccountSecret {
            name: HeaderName::from_static("x-secondary-account"),
            slot: ResponsesAccountSlot::Secondary,
            authority: secondary_account,
        },
        ResponsesHeaderBinding::PublicExact {
            name: HeaderName::from_static("content-type"),
            value: b"application/json".to_vec(),
        },
    ];
    let base = physical_request_digest(
        &physical,
        &authority,
        &dual_request(
            "Bearer primary-a",
            "Bearer secondary-a",
            "account-a",
            "account-b",
            false,
        )?,
        &bindings,
    )?;
    bindings.reverse();
    assert_eq!(
        base,
        physical_request_digest(
            &physical,
            &authority,
            &dual_request(
                "Bearer primary-b",
                "Bearer secondary-b",
                "account-c",
                "account-d",
                true,
            )?,
            &bindings,
        )?,
        "header order and all secret bytes remain outside the digest"
    );
    assert_ne!(
        base,
        physical_request_digest(
            &physical,
            &rotated_authority,
            &dual_request(
                "Bearer primary-a",
                "Bearer secondary-a",
                "account-a",
                "account-b",
                false,
            )?,
            &bindings,
        )?,
        "each slot generation is bound independently"
    );
    assert_ne!(
        base,
        physical_request_digest(
            &physical,
            &secondary_rotated_authority,
            &dual_request(
                "Bearer primary-a",
                "Bearer secondary-a",
                "account-a",
                "account-b",
                false,
            )?,
            &bindings,
        )?,
        "secondary generation rotation is independently identity-affecting"
    );
    Ok(())
}

#[test]
fn credential_and_account_slots_are_complete_exact_and_nonduplicated() -> Result<(), TestError> {
    let primary_generation = openai_generation_fingerprint(b"primary generation");
    let secondary_generation = openai_generation_fingerprint(b"secondary generation");
    let primary_account = model_binding(b"primary account")?;
    let secondary_account = model_binding(b"secondary account")?;
    let content = ResponsesHeaderBinding::PublicExact {
        name: HeaderName::from_static("content-type"),
        value: b"application/json".to_vec(),
    };
    let secondary_authority = ResponsesCredentialAuthority::secondary(secondary_generation);
    assert!(matches!(
        validate_header_authority(std::slice::from_ref(&content), &secondary_authority),
        Err(ResponsesAttemptBuildError::AuthSlots)
    ));
    let secondary = ResponsesHeaderBinding::CredentialSecret {
        name: HeaderName::from_static("authorization"),
        slot: ResponsesAuthSlot::Secondary,
    };
    assert!(matches!(
        validate_header_authority(
            &[secondary.clone(), secondary.clone(), content.clone()],
            &secondary_authority
        ),
        Err(ResponsesAttemptBuildError::AuthSlots)
    ));
    let primary = ResponsesHeaderBinding::CredentialSecret {
        name: HeaderName::from_static("x-primary-authorization"),
        slot: ResponsesAuthSlot::Primary,
    };
    let dual_authority = ResponsesCredentialAuthority::primary_and_secondary(
        primary_generation,
        secondary_generation,
    )
    .with_account(ResponsesAccountSlot::Primary, primary_account)?
    .with_account(ResponsesAccountSlot::Secondary, secondary_account)?;
    assert!(matches!(
        validate_header_authority(&[primary.clone(), content.clone()], &dual_authority),
        Err(ResponsesAttemptBuildError::AuthSlots)
    ));
    let primary_account_binding = ResponsesHeaderBinding::AccountSecret {
        name: HeaderName::from_static("x-primary-account"),
        slot: ResponsesAccountSlot::Primary,
        authority: primary_account,
    };
    let secondary_account_binding = ResponsesHeaderBinding::AccountSecret {
        name: HeaderName::from_static("x-secondary-account"),
        slot: ResponsesAccountSlot::Secondary,
        authority: secondary_account,
    };
    let exact = vec![
        secondary_account_binding.clone(),
        primary.clone(),
        content.clone(),
        secondary.clone(),
        primary_account_binding.clone(),
    ];
    validate_header_authority(&exact, &dual_authority)?;
    let mut reversed = exact.clone();
    reversed.reverse();
    validate_header_authority(&reversed, &dual_authority)?;

    assert!(matches!(
        validate_header_authority(
            &[
                primary.clone(),
                secondary.clone(),
                primary_account_binding.clone(),
                content.clone(),
            ],
            &dual_authority,
        ),
        Err(ResponsesAttemptBuildError::AuthSlots)
    ));
    assert!(matches!(
        validate_header_authority(
            &[
                primary.clone(),
                secondary.clone(),
                primary_account_binding.clone(),
                secondary_account_binding.clone(),
                ResponsesHeaderBinding::AccountSecret {
                    name: HeaderName::from_static("x-secondary-account-copy"),
                    slot: ResponsesAccountSlot::Secondary,
                    authority: secondary_account,
                },
                content.clone(),
            ],
            &dual_authority,
        ),
        Err(ResponsesAttemptBuildError::AuthSlots)
    ));
    assert!(matches!(
        validate_header_authority(
            &[
                primary,
                secondary,
                primary_account_binding,
                ResponsesHeaderBinding::AccountSecret {
                    name: HeaderName::from_static("x-secondary-account"),
                    slot: ResponsesAccountSlot::Secondary,
                    authority: model_binding(b"wrong secondary account")?,
                },
                content,
            ],
            &dual_authority,
        ),
        Err(ResponsesAttemptBuildError::Binding)
    ));
    Ok(())
}

#[test]
fn response_head_guard_retains_opaque_lease_until_explicit_release() -> Result<(), TestError> {
    #[derive(Debug)]
    struct DropProbe(Arc<AtomicBool>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    let dropped = Arc::new(AtomicBool::new(false));
    let prepared = ResponsesPreparedPhysicalRequest::new(
        attempt_binding(1, 1)?,
        request(b"{}", "Bearer secret")?,
        ResponsesCredentialAuthority::secondary(openai_generation_fingerprint(b"generation")),
        header_bindings(),
        DropProbe(Arc::clone(&dropped)),
    );
    assert!(!dropped.load(Ordering::SeqCst));

    let ResponsesPreparedPhysicalRequest {
        response_head_guard,
        ..
    } = prepared;
    assert!(!dropped.load(Ordering::SeqCst));
    drop(response_head_guard);
    assert!(dropped.load(Ordering::SeqCst));
    Ok(())
}

#[test]
fn status_acceptance_is_closed_and_conservative() -> Result<(), TestError> {
    assert_eq!(
        classify_status(StatusCode::UNAUTHORIZED),
        ResponsesHttpStatusClass::Authentication
    );
    assert_eq!(
        classify_status(StatusCode::TOO_MANY_REQUESTS),
        ResponsesHttpStatusClass::RateLimited
    );
    for status in [
        StatusCode::UNAUTHORIZED,
        StatusCode::TOO_MANY_REQUESTS,
        StatusCode::SERVICE_UNAVAILABLE,
        StatusCode::from_u16(529)?,
    ] {
        assert_eq!(
            status_outcome(status),
            PhysicalRequestOutcome::DefiniteRejection,
            "status {status} refuses before acceptance"
        );
    }
    for status in [
        StatusCode::BAD_REQUEST,
        StatusCode::FORBIDDEN,
        StatusCode::CONFLICT,
        StatusCode::REQUEST_TIMEOUT,
        StatusCode::TOO_EARLY,
        StatusCode::INTERNAL_SERVER_ERROR,
        StatusCode::BAD_GATEWAY,
        StatusCode::GATEWAY_TIMEOUT,
    ] {
        assert_eq!(
            status_outcome(status),
            PhysicalRequestOutcome::AmbiguousPostSend,
            "status {status} cannot prove pre-acceptance rejection"
        );
    }
    Ok(())
}

#[test]
fn codex_completed_sse_preserves_explicit_end_turn_for_projection() {
    let data = ResponsesStreamDialectData::codex("gpt-test");
    let mut collector = StreamCollector::new(data, ResponsesStreamLimits::default());
    let bytes = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"in_progress\",\"model\":\"gpt-test\"}}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"sequence_number\":1,\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"completed\",\"model\":\"gpt-test\",\"output\":[],\"end_turn\":false,\"usage\":{\"input_tokens\":7,\"output_tokens\":5}}}\n\n"
    );
    assert!(matches!(collector.push(bytes.as_bytes()), Ok(events) if events.len() == 1));
    let Ok((residual, terminal)) = collector.finish() else {
        panic!("valid completed SSE expected");
    };
    assert_eq!(
        residual,
        [] as [ResponsesStreamEventKind<ResponsesResponse, ResponsesProviderError>; 0]
    );
    let RawTerminal::Completed(response) = terminal else {
        panic!("completed terminal expected");
    };
    assert_eq!(response.end_turn(), Some(false));
    assert_eq!(
        response
            .usage()
            .map(|usage| (usage.input_tokens(), usage.output_tokens())),
        Some((Some(7), Some(5)))
    );
}

#[test]
fn codex_auto_dispatches_terminal_from_residual_eof() {
    let data = ResponsesStreamDialectData::codex("gpt-test");
    let mut collector = StreamCollector::new(data, ResponsesStreamLimits::default());
    let bytes = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"in_progress\",\"model\":\"gpt-test\"}}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"sequence_number\":1,\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"completed\",\"model\":\"gpt-test\",\"output\":[]}}"
    );
    assert!(matches!(collector.push(bytes.as_bytes()), Ok(events) if events.len() == 1));
    match collector.finish() {
        Ok((residual, terminal)) => {
            assert_eq!(
                residual,
                [] as [ResponsesStreamEventKind<ResponsesResponse, ResponsesProviderError>; 0]
            );
            assert!(matches!(&terminal, RawTerminal::Completed(_)));
        }
        Err(error) => panic!("terminal residual fixture failed: {error:?}"),
    }
}

#[test]
fn codex_terminal_owns_the_boundary_before_buffered_provider_trailers() {
    let data = ResponsesStreamDialectData::codex("gpt-test");
    let mut collector = StreamCollector::new(data, ResponsesStreamLimits::default());
    let bytes = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"in_progress\",\"model\":\"gpt-test\"}}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"sequence_number\":1,\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"completed\",\"model\":\"gpt-test\",\"output\":[]}}\n\n",
        "event: codex.response.metadata\n",
        "data: {\"type\":\"codex.response.metadata\",\"sequence_number\":2}\n\n",
        "data: [DONE]\n\n"
    );
    assert!(matches!(collector.push(bytes.as_bytes()), Ok(events) if events.len() == 1));
    match collector.finish() {
        Ok((residual, terminal)) => {
            assert_eq!(
                residual,
                [] as [ResponsesStreamEventKind<ResponsesResponse, ResponsesProviderError>; 0]
            );
            assert!(matches!(terminal, RawTerminal::Completed(_)));
        }
        Err(error) => panic!("terminal boundary fixture failed: {error:?}"),
    }
}

#[test]
fn secondary_slots_fail_before_attempt_execution_when_ledger_is_single_slot() {
    assert!(matches!(
        validate_auth_slots(
            LedgerPolicy::RESPONSES_FAMILY,
            ResponsesAuthSlots::Secondary
        ),
        Err(ResponsesAttemptBuildError::AuthSlots)
    ));
    assert!(matches!(
        validate_auth_slots(
            LedgerPolicy::RESPONSES_FAMILY,
            ResponsesAuthSlots::PrimaryAndSecondary
        ),
        Err(ResponsesAttemptBuildError::AuthSlots)
    ));
    assert!(
        validate_auth_slots(LedgerPolicy::OPENAI_STRICT, ResponsesAuthSlots::Secondary).is_ok()
    );
}

#[test]
fn delayed_body_delivers_available_observation_and_saturation_preserves_terminal() {
    let (observations, mut receiver) = mpsc::channel(1);
    let mut collector = StreamCollector::new(
        ResponsesStreamDialectData::codex("gpt-test"),
        ResponsesStreamLimits::default(),
    );
    let mut delivery = ObservationDelivery::new(Some(observations));
    let first = collector.push(concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"in_progress\",\"model\":\"gpt-test\"}}\n\n"
    ).as_bytes());
    let Ok(first) = first else {
        panic!("first body chunk must decode");
    };
    delivery.deliver(first);
    assert!(
        matches!(
            receiver.try_recv(),
            Ok(ResponsesObservation::Event(
                ResponsesStreamEventKind::ResponseCreated { .. }
            ))
        ),
        "the first body chunk is observable before the terminal exists"
    );

    let next = collector.push(concat!(
        "event: response.in_progress\n",
        "data: {\"type\":\"response.in_progress\",\"sequence_number\":1,\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-test\"}}\n\n",
        "event: response.in_progress\n",
        "data: {\"type\":\"response.in_progress\",\"sequence_number\":2,\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-test\"}}\n\n"
    ).as_bytes());
    let Ok(next) = next else {
        panic!("delayed body chunk must decode");
    };
    delivery.deliver(next);

    assert!(matches!(collector.push(concat!(
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"sequence_number\":3,\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"completed\",\"model\":\"gpt-test\",\"output\":[]}}\n\n"
    ).as_bytes()), Ok(events) if events.is_empty()));

    match collector.finish() {
        Ok((residual, terminal)) => {
            assert_eq!(
                residual,
                [] as [ResponsesStreamEventKind<ResponsesResponse, ResponsesProviderError>; 0]
            );
            assert_eq!(delivery.observations_delivered, 2);
            assert_eq!(delivery.observations_dropped, 1);
            assert!(matches!(terminal, RawTerminal::Completed(_)));
        }
        Err(error) => panic!("observation saturation changed the terminal result: {error:?}"),
    }
}

#[test]
fn closed_observation_channel_records_gap_without_affecting_decoder() {
    let (observations, receiver) = mpsc::channel(1);
    drop(receiver);
    let mut collector = StreamCollector::new(
        ResponsesStreamDialectData::codex("gpt-test"),
        ResponsesStreamLimits::default(),
    );
    let decoded = collector.push(
        concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"in_progress\",\"model\":\"gpt-test\"}}\n\n"
        )
        .as_bytes(),
    );
    let Ok(decoded) = decoded else {
        panic!("fixture must decode independently of observations");
    };
    let mut delivery = ObservationDelivery::new(Some(observations));
    delivery.deliver(decoded);
    assert_eq!(delivery.observations_delivered, 0);
    assert_eq!(delivery.observations_dropped, 1);
}

#[test]
fn strict_secondary_and_dual_slots_are_reserved_and_revalidated() -> Result<(), TestError> {
    for slots in [
        ResponsesAuthSlots::Secondary,
        ResponsesAuthSlots::PrimaryAndSecondary,
    ] {
        let core = FamilyAttemptCore::new_with_policy(LedgerPolicy::OPENAI_STRICT);
        core.mark_admitted()?;
        core.mark_service_admitted()?;
        core.physical_request_started()?;
        let generation = core.observations_ref().reserve_auth(slots.mask())?;
        assert_eq!(generation, 1);
        core.observations_ref()
            .validate_auth(generation, slots.mask())?;
    }

    let family = FamilyAttemptCore::new_with_policy(LedgerPolicy::RESPONSES_FAMILY);
    family.mark_admitted()?;
    family.mark_service_admitted()?;
    family.physical_request_started()?;
    assert!(
        family
            .observations_ref()
            .reserve_auth(ResponsesAuthSlots::Secondary.mask())
            .is_err()
    );
    Ok(())
}

#[test]
fn blank_line_closed_dialect_rejects_residual_eof() {
    let data = ResponsesStreamDialectData::new(
        ResponsesDialectProfile::xai(),
        LifecycleDialectPolicy::xai(),
        "grok-test",
        crate::response_host::ResponsesUsagePolicy::RequireTotal,
        ResponsesResponseLimits::default(),
        crate::stream_host::ResponsesStreamCapabilities::minimal(),
    );
    let mut collector = StreamCollector::new(data, ResponsesStreamLimits::default());
    assert!(
        collector
            .push(b"data: {\"type\":\"response.created\"}")
            .is_ok()
    );
    assert!(matches!(
        collector.finish(),
        Err(CollectorFailure::Stream(
            ResponsesFamilyStreamError::Truncated
        ))
    ));
}

#[test]
fn stream_event_bound_is_enforced_before_accumulation() -> Result<(), TestError> {
    let limits = ResponsesStreamLimits::new(4_096, 16_384, 1)?;
    let mut collector = StreamCollector::new(ResponsesStreamDialectData::codex("gpt-test"), limits);
    let bytes = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"in_progress\",\"model\":\"gpt-test\"}}\n\n",
        "event: response.in_progress\n",
        "data: {\"type\":\"response.in_progress\",\"sequence_number\":1,\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-test\"}}\n\n"
    );
    assert!(matches!(
        collector.push(bytes.as_bytes()),
        Err(CollectorFailure::Stream(
            ResponsesFamilyStreamError::ResourceLimit(_)
        ))
    ));
    Ok(())
}

#[test]
fn long_function_arguments_cross_the_old_budget_and_count_the_terminal() {
    const DELTAS: usize = 16_385;
    const EVENTS: usize = DELTAS + 7; // prefix/suffix deltas and five lifecycle events

    fn feed(
        collector: &mut StreamCollector,
        event: serde_json::Value,
    ) -> Result<(), CollectorFailure> {
        // One small SSE frame per push keeps the fixture's transport chunks bounded.
        let frame = format!("data: {event}\n\n");
        collector.push(frame.as_bytes()).map(|_| ())
    }

    let defaults = ResponsesStreamLimits::default();
    assert_eq!(defaults.max_events(), 100_000);
    assert!(defaults.max_events() >= EVENTS);
    assert_eq!(
        plakous_responses::ResponsesDecodeLimits::default()
            .stream_limits()
            .max_events(),
        defaults.max_events()
    );

    for allowed_events in [None, Some(EVENTS), Some(EVENTS - 1)] {
        let limits = allowed_events.map_or(defaults, |count| {
            ResponsesStreamLimits::new(
                defaults.max_frame_bytes(),
                defaults.max_total_bytes(),
                count,
            )
            .unwrap_or_else(|error| panic!("limits: {error}"))
        });
        let mut collector =
            StreamCollector::new(ResponsesStreamDialectData::codex("gpt-test"), limits);
        feed(&mut collector, serde_json::json!({
            "type":"response.created", "sequence_number":0,
            "response":{"id":"resp_long","object":"response","status":"in_progress","model":"gpt-test"}
        }))
        .unwrap_or_else(|error| panic!("created: {error:?}"));
        feed(&mut collector, serde_json::json!({
            "type":"response.output_item.added", "sequence_number":1, "output_index":0,
            "item":{"id":"fc_long","type":"function_call","call_id":"call_long","name":"lookup","arguments":""}
        }))
        .unwrap_or_else(|error| panic!("item added: {error:?}"));

        let mut arguments = String::from("{\"values\":[");
        feed(
            &mut collector,
            serde_json::json!({
                "type":"response.function_call_arguments.delta", "sequence_number":2,
                "output_index":0, "item_id":"fc_long", "delta":arguments
            }),
        )
        .unwrap_or_else(|error| panic!("argument prefix: {error:?}"));
        for index in 0..DELTAS {
            let delta = if index + 1 == DELTAS {
                index.to_string()
            } else {
                format!("{index},")
            };
            arguments.push_str(&delta);
            feed(
                &mut collector,
                serde_json::json!({
                    "type":"response.function_call_arguments.delta", "sequence_number":index + 3,
                    "output_index":0, "item_id":"fc_long", "delta":delta
                }),
            )
            .unwrap_or_else(|error| panic!("argument delta {index}: {error:?}"));
        }
        arguments.push_str("]}");
        feed(
            &mut collector,
            serde_json::json!({
                "type":"response.function_call_arguments.delta", "sequence_number":DELTAS + 3,
                "output_index":0, "item_id":"fc_long", "delta":"]}"
            }),
        )
        .unwrap_or_else(|error| panic!("argument suffix: {error:?}"));
        feed(
            &mut collector,
            serde_json::json!({
                "type":"response.function_call_arguments.done", "sequence_number":DELTAS + 4,
                "output_index":0, "item_id":"fc_long", "arguments":arguments
            }),
        )
        .unwrap_or_else(|error| panic!("arguments done: {error:?}"));
        let item = serde_json::json!({
            "id":"fc_long","type":"function_call","call_id":"call_long",
            "name":"lookup","arguments":arguments,"status":"completed"
        });
        feed(
            &mut collector,
            serde_json::json!({
                "type":"response.output_item.done", "sequence_number":DELTAS + 5,
                "output_index":0, "item":item
            }),
        )
        .unwrap_or_else(|error| panic!("item done: {error:?}"));
        let terminal = serde_json::json!({
            "type":"response.completed", "sequence_number":DELTAS + 6,
            "response":{"id":"resp_long","object":"response","status":"completed",
                "model":"gpt-test","output":[item],
                "usage":{"input_tokens":12,"output_tokens":8,"total_tokens":20}}
        });
        let result = feed(&mut collector, terminal);
        if allowed_events == Some(EVENTS - 1) {
            assert!(matches!(
                result,
                Err(CollectorFailure::Stream(
                    ResponsesFamilyStreamError::ResourceLimit(_)
                ))
            ));
            assert!(matches!(
                collector.finish(),
                Err(CollectorFailure::Stream(
                    ResponsesFamilyStreamError::DecoderFailed
                ))
            ));
        } else {
            result.unwrap_or_else(|error| panic!("terminal at cap: {error:?}"));
            let (_, terminal) = collector
                .finish()
                .unwrap_or_else(|error| panic!("finish: {error:?}"));
            assert!(matches!(terminal, RawTerminal::Completed(_)));
            assert_eq!(
                terminal.native()["output"][0]["arguments"].as_str(),
                Some(arguments.as_str())
            );
            assert_eq!(terminal.native()["usage"]["total_tokens"], 20);
            let assembled: serde_json::Value = serde_json::from_str(&arguments)
                .unwrap_or_else(|error| panic!("assembled arguments: {error}"));
            assert_eq!(assembled["values"].as_array().map(Vec::len), Some(DELTAS));
            assert_eq!(assembled["values"][0], 0);
            assert_eq!(assembled["values"][DELTAS - 1].as_u64(), Some(16_384));
        }
    }
}

fn mixed_model_sse() -> Vec<u8> {
    include_str!("../../../providers/openai/tests/fixtures/response_mixed.sse")
        .replace("gpt-5.4", "gpt-test")
        .into_bytes()
}

fn grok_length_text_sse() -> Vec<u8> {
    let events = [
        serde_json::json!({"type":"response.created","sequence_number":0,
            "response":{"id":"resp-grok-length","model":"gpt-test"}}),
        serde_json::json!({"type":"response.output_item.added","sequence_number":1,
            "output_index":0,"item":{"id":"message","type":"message","role":"assistant","content":[]}}),
        serde_json::json!({"type":"response.content_part.added","sequence_number":2,
            "item_id":"message","output_index":0,"content_index":0,
            "part":{"type":"output_text","text":"","annotations":[]}}),
        serde_json::json!({"type":"response.output_text.delta","sequence_number":3,
            "item_id":"message","output_index":0,"content_index":0,"delta":"summary"}),
        serde_json::json!({"type":"response.incomplete","sequence_number":4,
            "response":{"id":"resp-grok-length","object":"response","status":"incomplete",
                "model":"gpt-test","output":[{"id":"message","type":"message",
                    "role":"assistant","status":"incomplete",
                    "content":[{"type":"output_text","text":"summary","annotations":[]}]}],
                "usage":{"input_tokens":2,"output_tokens":7,"total_tokens":9},
                "incomplete_details":{"reason":"max_output_tokens"}}}),
    ];
    let mut stream = String::new();
    for event in events {
        stream.push_str("data: ");
        stream.push_str(&event.to_string());
        stream.push_str("\n\n");
    }
    stream.into_bytes()
}

#[tokio::test]
async fn shared_grok_length_text_crosses_joined_model_boundary_once() -> TestResult {
    let response = http_response("200 OK", Some("text/event-stream"), &grok_length_text_sse());
    let (client_config, tls_identity, origin, peer) = responding_wire_peer(response).await?;
    let client =
        Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
    let dialect = wire_dialect(
        origin,
        openai_generation_fingerprint(b"grok-length-text-test"),
    );
    let (execution, service, context, _, _) = prepared_model_host_with_grok(
        client,
        Arc::clone(&dialect),
        64,
        false,
        None,
        ResponsesHostedProgramContext::default(),
        false,
        true,
        true,
    )?;
    let stream = start_prepared_responses_model_call(
        execution,
        service,
        context,
        Instant::now() + Duration::from_secs(5),
    )
    .await?;
    let events = collect_model_events(stream)
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(dialect.preflights.load(Ordering::SeqCst), 1);
    assert_eq!(dialect.final_authorizations.load(Ordering::SeqCst), 1);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, ModelEvent::ResponseCompleted(_)))
            .count(),
        1
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, ModelEvent::ToolInputDelta(_)))
    );
    let completed = events.iter().find_map(|event| match event {
        ModelEvent::ResponseCompleted(response) => Some(response),
        _ => None,
    });
    let completed = completed.ok_or_else(|| io::Error::other("missing length terminal"))?;
    assert_eq!(completed.finish().reason(), FinishReason::Length);
    assert_eq!(completed.usage(), Some(Usage::new(2, 7, 0, 0, 0)));
    peer.await??;
    Ok(())
}

/// A write whose arguments the output bound cut off: the call item never
/// completes, and the terminal is `incomplete` for `max_output_tokens`.
fn truncated_call_sse() -> Vec<u8> {
    let events = [
        serde_json::json!({"type":"response.created","sequence_number":0,
            "response":{"id":"resp-cut","model":"gpt-test"}}),
        serde_json::json!({"type":"response.output_item.added","sequence_number":1,
            "output_index":0,"item":{"id":"call-item","type":"function_call",
                "call_id":"call-1","name":"write","arguments":""}}),
        serde_json::json!({"type":"response.function_call_arguments.delta","sequence_number":2,
            "item_id":"call-item","output_index":0,"delta":"{\"path\":\"big.txt\",\"content\":\"aaa"}),
        serde_json::json!({"type":"response.incomplete","sequence_number":3,
            "response":{"id":"resp-cut","object":"response","status":"incomplete",
                "model":"gpt-test","output":[{"id":"call-item","type":"function_call",
                    "status":"incomplete","call_id":"call-1","name":"write",
                    "arguments":"{\"path\":\"big.txt\",\"content\":\"aaa"}],
                "usage":{"input_tokens":2,"output_tokens":7,"total_tokens":9},
                "incomplete_details":{"reason":"max_output_tokens"}}}),
    ];
    let mut stream = String::new();
    for event in events {
        stream.push_str("data: ");
        stream.push_str(&event.to_string());
        stream.push_str("\n\n");
    }
    stream.into_bytes()
}

/// A call cut off by the output bound fails the operation as `OutputLimit`,
/// the one kind the runtime answers by telling the model and retrying, and
/// never completes a response that could dispatch it.
#[tokio::test]
async fn a_call_cut_off_by_the_output_bound_fails_as_output_limit() -> TestResult {
    let response = http_response("200 OK", Some("text/event-stream"), &truncated_call_sse());
    let (client_config, tls_identity, origin, peer) = responding_wire_peer(response).await?;
    let client =
        Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
    let dialect = wire_dialect(
        origin,
        openai_generation_fingerprint(b"truncated-call-test"),
    );
    let (execution, service, context, _, _) = prepared_model_host_with_grok(
        client,
        Arc::clone(&dialect),
        64,
        false,
        None,
        ResponsesHostedProgramContext::default(),
        false,
        true,
        true,
    )?;
    let stream = start_prepared_responses_model_call(
        execution,
        service,
        context,
        Instant::now() + Duration::from_secs(5),
    )
    .await?;
    let events = collect_model_events(stream).await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Ok(ModelEvent::ResponseCompleted(_))))
    );
    let error = events
        .into_iter()
        .find_map(Result::err)
        .ok_or_else(|| io::Error::other("a truncated call must fail the operation"))?;
    assert_eq!(error.kind(), ModelErrorKind::OutputLimit, "{error}");
    peer.await??;
    Ok(())
}

async fn collect_model_events(
    mut stream: plakous_model::ModelStream,
) -> Vec<Result<ModelEvent, ModelError>> {
    let mut events = Vec::new();
    while let Some(event) = poll_fn(|context| stream.as_mut().poll_next(context)).await {
        events.push(event);
    }
    events
}

#[tokio::test]
async fn shared_model_host_incomplete_arguments_deadline_is_diagnostic_and_joined() -> TestResult {
    for capacity in [64, 1] {
        let WireEndpoint {
            listener,
            acceptor,
            client_config,
            tls_identity,
            origin,
        } = wire_endpoint().await?;
        let peer = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let mut stream = acceptor.accept(stream).await?;
            let _request = read_wire_request(&mut stream).await?;
            let body = String::from_utf8(mixed_model_sse())?;
            let prefix = body
                .split("event: response.function_call_arguments.done")
                .next()
                .ok_or_else(|| io::Error::other("missing fixture prefix"))?;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n",
                prefix.len() + 1024
            );
            stream.write_all(head.as_bytes()).await?;
            stream.write_all(prefix.as_bytes()).await?;
            stream.flush().await?;
            let mut byte = [0_u8; 1];
            Ok::<_, TestError>(!matches!(stream.read(&mut byte).await, Ok(1)))
        });
        let client =
            Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
        let dialect = wire_dialect(
            origin,
            openai_generation_fingerprint(b"unfinished-arguments"),
        );
        let (execution, service, context, _, _) = prepared_model_host(
            client,
            Arc::clone(&dialect),
            capacity,
            true,
            None,
            ResponsesHostedProgramContext::default(),
            false,
        )?;
        let stream = start_prepared_responses_model_call(
            execution,
            service,
            ModelCallContext::new(
                context.attempt_id(),
                context.cancellation().clone(),
                Some(Timestamp::new(1_700_000_002, 0)?),
            ),
            Instant::now() + Duration::from_secs(1),
        )
        .await?;
        let events = collect_model_events(stream).await;
        assert_eq!(dialect.preflights.load(Ordering::SeqCst), 1);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Ok(ModelEvent::ResponseCompleted(_))))
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Ok(ModelEvent::ToolInputDelta(_))))
                .count(),
            if capacity == 64 { 2 } else { 0 }
        );
        let errors = events
            .iter()
            .filter_map(|event| event.as_ref().err())
            .collect::<Vec<_>>();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].kind(), ModelErrorKind::Timeout);
        let message = errors[0].message();
        if capacity == 64 {
            assert!(message.contains("incomplete tool argument stream observed"));
            assert!(message.contains("observed_argument_deltas=2 observed_argument_completions=0"));
        } else {
            assert!(message.contains("observed_argument_deltas=0 observed_argument_completions=0"));
            assert!(!message.contains("incomplete tool argument stream observed"));
        }
        assert!(message.contains("observations may be dropped"));
        assert!(message.contains("authoritative_completion=false"));
        assert!(!message.contains("call_remote"));
        assert!(!message.contains("rust"));
        assert!(errors[0].message().contains("cleanup=joined"));
        assert!(timeout(Duration::from_secs(1), peer).await???);
    }
    Ok(())
}

#[tokio::test]
async fn unreachable_ride_out_changes_shared_host_behavior_identity() -> TestResult {
    let endpoint = wire_endpoint().await?;
    let client = Client::with_tls_config_and_identity(
        Limits::default(),
        endpoint.client_config,
        endpoint.tls_identity,
    )?;
    let authorization = ResponsesAuthorizationAdapter::new(1, 1, wire_preflight, wire_revalidate)?;
    let current = host_behavior_identity(&client, &authorization)?;
    let transport = client
        .behavior_identity()
        .ok_or_else(|| io::Error::other("missing TLS behavior identity"))?;
    let prior = Digest::sha256(
        DigestDomain::ModelBinding,
        10,
        &[
            b"responses-attempt-host-behavior-v10",
            transport.as_bytes().as_slice(),
            1_u32.to_be_bytes().as_slice(),
            1_u32.to_be_bytes().as_slice(),
        ],
    )?;
    assert_ne!(current, prior);
    assert_eq!(HOST_BEHAVIOR_IDENTITY_REVISION, 11);
    Ok(())
}

#[tokio::test]
async fn shared_model_host_completed_sse_projects_assistant_continuation() -> TestResult {
    let body = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"in_progress\",\"model\":\"gpt-test\"}}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"sequence_number\":1,\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"completed\",\"model\":\"gpt-test\",\"output\":[],\"end_turn\":false,\"usage\":{\"input_tokens\":7,\"output_tokens\":5}}}\n\n"
    );
    let response = http_response("200 OK", Some("text/event-stream"), body.as_bytes());
    let (client_config, tls_identity, origin, peer) = responding_wire_peer(response).await?;
    let client =
        Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
    let dialect = wire_dialect(
        origin,
        openai_generation_fingerprint(b"explicit-end-turn-continuation"),
    );
    let (execution, service, context, _, _) = prepared_model_host(
        client,
        dialect,
        64,
        true,
        None,
        ResponsesHostedProgramContext::default(),
        false,
    )?;
    let stream = start_prepared_responses_model_call(
        execution,
        service,
        context,
        Instant::now() + Duration::from_secs(5),
    )
    .await?;
    let events = collect_model_events(stream)
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;
    let completed = events.iter().find_map(|event| match event {
        ModelEvent::ResponseCompleted(response) => Some(response),
        _ => None,
    });
    let completed = completed.ok_or_else(|| io::Error::other("missing model completion"))?;
    assert_eq!(
        completed.finish().reason(),
        FinishReason::AssistantContinuation
    );
    assert_eq!(completed.usage(), Some(Usage::new(7, 5, 0, 0, 0)));
    peer.await??;
    Ok(())
}

#[tokio::test]
async fn shared_model_host_first_poll_admits_and_projects_all_semantic_delta_kinds() -> TestResult {
    let response = http_response("200 OK", Some("text/event-stream"), &mixed_model_sse());
    let (client_config, tls_identity, origin, peer) = responding_wire_peer(response).await?;
    let client =
        Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
    let generation = openai_generation_fingerprint(b"shared-model-host-delta-generation");
    let dialect = wire_dialect(origin, generation);
    let (execution, service, context, _, artifact_binding) = prepared_model_host(
        client,
        Arc::clone(&dialect),
        64,
        true,
        None,
        ResponsesHostedProgramContext::default(),
        false,
    )?;

    let start = start_prepared_responses_model_call(
        execution,
        service,
        context,
        Instant::now() + Duration::from_secs(5),
    );
    assert_eq!(dialect.preflights.load(Ordering::SeqCst), 0);
    let stream = start.await?;
    assert_eq!(dialect.preflights.load(Ordering::SeqCst), 1);
    let events = collect_model_events(stream).await;
    assert!(events.iter().all(Result::is_ok));
    let events = events.into_iter().collect::<Result<Vec<_>, _>>()?;
    let completed = events
        .iter()
        .position(|event| matches!(event, ModelEvent::ResponseCompleted(_)))
        .ok_or_else(|| io::Error::other("shared host omitted terminal completion"))?;
    let text = events
        .iter()
        .position(|event| matches!(event, ModelEvent::TextDelta(_)))
        .ok_or_else(|| io::Error::other("shared host omitted text delta"))?;
    let reasoning = events
        .iter()
        .position(|event| matches!(event, ModelEvent::ReasoningDelta(_)))
        .ok_or_else(|| io::Error::other("shared host omitted reasoning delta"))?;
    let tool = events
        .iter()
        .position(|event| matches!(event, ModelEvent::ToolInputDelta(_)))
        .ok_or_else(|| io::Error::other("shared host omitted tool delta"))?;
    assert!(text < completed && reasoning < completed && tool < completed);
    let ModelEvent::ResponseCompleted(response) = &events[completed] else {
        unreachable!()
    };
    assert_eq!(
        response.provider_request_id(),
        Some("req_attempt_host_exact")
    );
    let candidate = response
        .family_artifact_candidate()
        .ok_or_else(|| io::Error::other("shared host omitted family artifact"))?;
    assert_eq!(candidate.binding(), artifact_binding);
    assert_eq!(
        candidate.producer().cache_affinity(),
        Some(cache_affinity_binding()?)
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, ModelEvent::ResponseCompleted(_)))
            .count(),
        1
    );
    assert_eq!(peer.await??.1, 1);
    Ok(())
}

#[tokio::test]
async fn shared_model_host_terminal_uses_exact_prior_hosted_program_state() -> TestResult {
    let body = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"resp_ptc_later\",\"object\":\"response\",\"status\":\"in_progress\",\"model\":\"gpt-test\"}}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"sequence_number\":1,\"response\":{\"id\":\"resp_ptc_later\",\"object\":\"response\",\"status\":\"completed\",\"model\":\"gpt-test\",\"output\":[{\"type\":\"function_call\",\"id\":\"fc_later\",\"call_id\":\"call_later\",\"name\":\"lookup\",\"arguments\":\"{}\",\"caller\":{\"type\":\"program\",\"caller_id\":\"pc_prior\"}}]}}\n\n"
    );
    let response = http_response("200 OK", Some("text/event-stream"), body.as_bytes());
    let (client_config, tls_identity, origin, peer) = responding_wire_peer(response).await?;
    let client =
        Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
    let generation = openai_generation_fingerprint(b"shared-model-host-prior-program");
    let dialect = wire_dialect(origin, generation);
    let hosted_programs = ResponsesHostedProgramContext::test_openai_platform_live_program(
        "program_prior",
        "pc_prior",
        "fp-prior",
    )?;
    let (execution, service, context, _, _) =
        prepared_model_host(client, dialect, 8, true, None, hosted_programs, true)?;
    let stream = start_prepared_responses_model_call(
        execution,
        service,
        context,
        Instant::now() + Duration::from_secs(5),
    )
    .await?;
    let events = collect_model_events(stream)
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;
    let terminal = events
        .iter()
        .find_map(|event| match event {
            ModelEvent::ResponseCompleted(response) => Some(response),
            _ => None,
        })
        .ok_or_else(|| io::Error::other("prior-program execution omitted terminal"))?;
    assert!(terminal.output().iter().any(|item| {
        item.blocks().iter().any(|block| {
            matches!(
                block,
                ContentBlock::ToolCall(call)
                    if call.correlation().caller_lineage()
                        .provider_programmatic_execution_id() == Some("pc_prior")
            )
        })
    }));
    let (_, accepts) = peer.await??;
    assert_eq!(accepts, 1);
    Ok(())
}

#[tokio::test]
async fn shared_model_host_loss_is_observational_terminal_is_self_validating_and_drop_cancels_join()
-> TestResult {
    let response = http_response("200 OK", Some("text/event-stream"), &mixed_model_sse());
    let (client_config, tls_identity, origin, peer) = responding_wire_peer(response).await?;
    let client =
        Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
    let generation = openai_generation_fingerprint(b"shared-model-host-gap-generation");
    let dialect = wire_dialect(origin, generation);
    let (execution, service, context, cancellation, _) = prepared_model_host(
        client,
        dialect,
        1,
        true,
        None,
        ResponsesHostedProgramContext::default(),
        false,
    )?;
    let stream = start_prepared_responses_model_call(
        execution,
        service,
        context,
        Instant::now() + Duration::from_secs(5),
    )
    .await?;
    let events = collect_model_events(stream).await;
    assert!(events.iter().all(Result::is_ok));
    let terminal = events
        .iter()
        .find_map(|event| match event {
            Ok(ModelEvent::ResponseCompleted(response)) => Some(response),
            _ => None,
        })
        .ok_or_else(|| io::Error::other("lossy stream omitted terminal aggregate"))?;
    terminal.validate_for_commit()?;
    assert!(terminal.family_artifact_candidate().is_some());
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Ok(ModelEvent::ResponseCompleted(_))))
            .count(),
        1
    );
    assert!(!cancellation.is_cancelled());
    assert_eq!(peer.await??.1, 1);

    let endpoint = wire_endpoint().await?;
    let client = Client::with_tls_config_and_identity(
        Limits::default(),
        endpoint.client_config,
        endpoint.tls_identity,
    )?;
    let dialect = wire_dialect(endpoint.origin, generation);
    let (execution, service, context, cancellation, _) = prepared_model_host(
        client,
        dialect,
        1,
        false,
        None,
        ResponsesHostedProgramContext::default(),
        false,
    )?;
    let stream = start_prepared_responses_model_call(
        execution,
        service,
        context,
        Instant::now() + Duration::from_secs(5),
    )
    .await?;
    drop(stream);
    assert!(cancellation.is_cancelled());
    drop(endpoint.listener);
    Ok(())
}

fn rate_limit_error_hook(
    error: &ResponsesAttemptError,
    _descriptor: &ResolvedModelDescriptor,
) -> Option<ModelError> {
    matches!(
        error,
        ResponsesAttemptError::HttpStatus {
            class: ResponsesHttpStatusClass::RateLimited,
            ..
        }
    )
    .then(|| ModelError::new(ModelErrorKind::QuotaExceeded, "dialect error hook"))
}

#[tokio::test]
async fn shared_model_host_maps_missing_duplicate_cancel_and_provider_hook() -> TestResult {
    let generation = openai_generation_fingerprint(b"shared-model-host-error-generation");
    let missing = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"in_progress\",\"model\":\"gpt-test\"}}\n\n"
    )
    .as_bytes();
    let mut duplicate = complete_sse_body();
    duplicate.extend_from_slice(
        concat!(
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"sequence_number\":3,\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"completed\",\"model\":\"gpt-test\",\"output\":[]}}\n\n"
        )
        .as_bytes(),
    );
    for body in [missing.to_vec()] {
        let (client_config, tls_identity, origin, peer) =
            responding_wire_peer(http_response("200 OK", Some("text/event-stream"), &body)).await?;
        let client =
            Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
        let dialect = wire_dialect(origin, generation);
        let (execution, service, context, _, _) = prepared_unretried_model_host(client, dialect)?;
        let stream = start_prepared_responses_model_call(
            execution,
            service,
            context,
            Instant::now() + Duration::from_secs(5),
        )
        .await?;
        let events = collect_model_events(stream).await;
        assert!(events.iter().any(|event| {
            matches!(event, Err(error) if error.kind() == ModelErrorKind::Protocol)
        }));
        assert_eq!(peer.await??.1, 1);
    }

    let (client_config, tls_identity, origin, peer) = responding_wire_peer(http_response(
        "200 OK",
        Some("text/event-stream"),
        &duplicate,
    ))
    .await?;
    let client =
        Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
    let dialect = wire_dialect(origin, generation);
    let (execution, service, context, _, _) = prepared_model_host(
        client,
        dialect,
        4,
        false,
        None,
        ResponsesHostedProgramContext::default(),
        false,
    )?;
    let stream = start_prepared_responses_model_call(
        execution,
        service,
        context,
        Instant::now() + Duration::from_secs(5),
    )
    .await?;
    let events = collect_model_events(stream).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Ok(ModelEvent::ResponseCompleted(_))))
    );
    assert!(!events.iter().any(Result::is_err));
    assert_eq!(peer.await??.1, 1);

    // Advice to wait past the caller hard deadline ends the attempt at once.
    let body = br#"{"error":{"message":"slow down","code":"rate_limit","request_id":"req_hook"}}"#;
    let (client_config, tls_identity, origin, peer) = responding_wire_peer(http_response_with(
        "429 Too Many Requests",
        Some("application/json"),
        "Retry-After: 30\r\n",
        body,
    ))
    .await?;
    let client =
        Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
    let dialect = wire_dialect(origin, generation);
    let (execution, service, context, _, _) = prepared_model_host(
        client,
        dialect,
        4,
        false,
        Some(rate_limit_error_hook),
        ResponsesHostedProgramContext::default(),
        false,
    )?;
    let stream = start_prepared_responses_model_call(
        execution,
        service,
        ModelCallContext::new(
            context.attempt_id(),
            context.cancellation().clone(),
            Some(Timestamp::new(1_700_000_006, 0)?),
        ),
        Instant::now() + Duration::from_secs(5),
    )
    .await?;
    let events = collect_model_events(stream).await;
    assert!(events.iter().any(|event| {
        matches!(event, Err(error) if error.kind() == ModelErrorKind::QuotaExceeded)
    }));
    assert_eq!(peer.await??.1, 1);

    let client = Client::new(Limits::default())?;
    let dialect = wire_dialect(Origin::parse("https://example.test")?, generation);
    let (execution, service, context, cancellation, _) = prepared_model_host(
        client,
        dialect,
        4,
        false,
        None,
        ResponsesHostedProgramContext::default(),
        false,
    )?;
    cancellation.cancel();
    let stream = start_prepared_responses_model_call(
        execution,
        service,
        context,
        Instant::now() + Duration::from_secs(5),
    )
    .await?;
    let events = collect_model_events(stream).await;
    assert!(
        events.iter().any(|event| {
            matches!(event, Err(error) if error.kind() == ModelErrorKind::Cancelled)
        })
    );
    Ok(())
}

#[tokio::test]
async fn shared_model_host_keeps_post_send_transport_as_transport_with_unsafe_retry_facts()
-> TestResult {
    let (client_config, tls_identity, origin, peer) = dropping_wire_peer_after_request().await?;
    let client =
        Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
    let generation = openai_generation_fingerprint(b"shared-model-host-transport-generation");
    let dialect = wire_dialect(origin, generation);
    let (execution, service, context, _, _) = prepared_unretried_model_host(client, dialect)?;
    let stream = start_prepared_responses_model_call(
        execution,
        service,
        context,
        Instant::now() + Duration::from_secs(5),
    )
    .await?;
    let events = collect_model_events(stream).await;
    let error = events
        .iter()
        .find_map(|event| event.as_ref().err())
        .ok_or_else(|| io::Error::other("post-send transport failure was not projected"))?;
    assert_eq!(error.kind(), ModelErrorKind::Transport);
    assert_eq!(
        error.message(),
        "Responses transport failed: connection dropped after the request was sent, before a response [1 requests; last: after send; retry declined: workflow]"
    );
    assert!(matches!(
        error.context().map(ModelErrorContext::retryability),
        Some(ModelRetryability::Unsafe {
            risk: ModelRetryRisk::DuplicateInferenceOrBilling
        })
    ));
    assert!(
        !peer.await??.is_empty(),
        "peer must observe the physical request"
    );
    Ok(())
}

#[tokio::test]
async fn shared_model_host_retains_redacted_response_head_deadline_diagnostics() -> TestResult {
    let WireEndpoint {
        listener,
        acceptor,
        client_config,
        tls_identity,
        origin,
    } = wire_endpoint().await?;
    let (release, released) = oneshot::channel();
    let peer = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut stream = acceptor.accept(stream).await?;
        let request = read_wire_request(&mut stream).await?;
        // Hold response headers until the client has joined its deadline failure.
        let _ = released.await;
        Ok::<_, TestError>(!request.is_empty())
    });
    let client = Client::with_tls_config_and_identity(
        Limits {
            response_head_timeout: Duration::from_millis(100),
            ..Limits::default()
        },
        client_config,
        tls_identity,
    )?;
    let generation = openai_generation_fingerprint(b"response-head-deadline-generation");
    let dialect = wire_dialect(origin, generation);
    let (execution, service, context, _, _) =
        prepared_unretried_model_host(client, Arc::clone(&dialect))?;
    let stream = start_prepared_responses_model_call(
        execution,
        service,
        context,
        Instant::now() + Duration::from_secs(5),
    )
    .await?;
    let events = collect_model_events(stream).await;
    let _ = release.send(());
    assert!(
        peer.await??,
        "peer must receive the request before its response is delayed"
    );
    let errors: Vec<_> = events
        .iter()
        .filter_map(|event| event.as_ref().err())
        .collect();
    assert_eq!(errors.len(), 1);
    let error = errors[0];
    assert_eq!(error.kind(), ModelErrorKind::Transport);
    assert_eq!(
        error.context().map(ModelErrorContext::phase),
        Some(plakous_model::ModelErrorPhase::Stream)
    );
    assert_eq!(
        error.context().map(ModelErrorContext::retryability),
        Some(ModelRetryability::Unsafe {
            risk: ModelRetryRisk::DuplicateInferenceOrBilling,
        })
    );
    assert_eq!(
        error.message(),
        "Responses transport failed: connection dropped after the request was sent, before a response [1 requests; last: after send; retry declined: workflow]"
    );
    assert_eq!(
        error
            .source_diagnostic()
            .map(plakous_model::RedactedModelErrorSource::message),
        Some("HTTP transport failed in ResponseHead (MayHaveTransmitted, DeadlineExceeded)")
    );
    let descriptor = test_descriptor()?;
    assert_eq!(
        error.context().map(ModelErrorContext::protocol),
        Some(descriptor.protocol())
    );
    assert_eq!(
        error.context().map(ModelErrorContext::route),
        Some(descriptor.route())
    );
    assert_eq!(
        error.context().map(ModelErrorContext::model),
        Some(descriptor.requested_model())
    );
    assert_eq!(dialect.preflights.load(Ordering::SeqCst), 1);
    assert_eq!(dialect.final_authorizations.load(Ordering::SeqCst), 1);
    let encoded = serde_json::to_string(error)?;
    assert!(!encoded.contains("exact-test-secret"));
    assert!(!encoded.contains("/responses?attempt=exact"));
    assert_eq!(serde_json::from_str::<ModelError>(&encoded)?, *error);
    Ok(())
}

fn sse(events: &[serde_json::Value]) -> Vec<u8> {
    let mut stream = String::new();
    for event in events {
        stream.push_str("data: ");
        stream.push_str(&event.to_string());
        stream.push_str("\n\n");
    }
    stream.into_bytes()
}

/// A stream that shows assistant text and then ends without a terminal.
fn text_stream_cut_before_terminal() -> Vec<u8> {
    sse(&[
        serde_json::json!({"type":"response.created","sequence_number":0,
            "response":{"id":"resp_cut","object":"response","status":"in_progress","model":"gpt-test"}}),
        serde_json::json!({"type":"response.output_item.added","sequence_number":1,
            "output_index":0,"item":{"id":"msg_cut","type":"message","role":"assistant","content":[]}}),
        serde_json::json!({"type":"response.content_part.added","sequence_number":2,
            "item_id":"msg_cut","output_index":0,"content_index":0,
            "part":{"type":"output_text","text":"","annotations":[]}}),
        serde_json::json!({"type":"response.output_text.delta","sequence_number":3,
            "item_id":"msg_cut","output_index":0,"content_index":0,"delta":"partial"}),
    ])
}

/// A stream that begins a function call and then ends without a terminal.
fn tool_call_stream_cut_before_terminal() -> Vec<u8> {
    sse(&[
        serde_json::json!({"type":"response.created","sequence_number":0,
            "response":{"id":"resp_tool","object":"response","status":"in_progress","model":"gpt-test"}}),
        serde_json::json!({"type":"response.output_item.added","sequence_number":1,
            "output_index":0,"item":{"id":"fc_1","type":"function_call","call_id":"call_1",
                "name":"lookup","arguments":"","status":"in_progress"}}),
    ])
}

fn drain_observations(
    observations: &mut mpsc::Receiver<ResponsesObservation>,
) -> Vec<ResponsesObservation> {
    let mut seen = Vec::new();
    while let Ok(observation) = observations.try_recv() {
        seen.push(observation);
    }
    seen
}

/// A connection reset after the request was written is replayed for a
/// cost-only turn, and never for a server-effect request or a compaction.
#[tokio::test]
async fn a_reset_after_send_is_replayed_only_for_a_cost_only_turn() -> TestResult {
    let accepted = http_response("200 OK", Some("text/event-stream"), &complete_sse_body());
    for (safety, turn, sends) in [
        (ReplaySafety::CostOnly, true, 2_usize),
        (ReplaySafety::ServerEffect, true, 1),
        (ReplaySafety::CostOnly, false, 1),
    ] {
        let responses = if sends == 2 {
            vec![Vec::new(), accepted.clone()]
        } else {
            vec![Vec::new()]
        };
        let (client_config, tls_identity, origin, peer) =
            responding_wire_peer_sequence(responses).await?;
        let client =
            Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
        let dialect = wire_dialect(origin, openai_generation_fingerprint(b"reset-after-send"));
        let authorization =
            ResponsesAuthorizationAdapter::new(1, 1, wire_preflight, wire_revalidate)?;
        let data = attempt_data(&client, &authorization)?;
        let mut attempt = attempt_with_safety(&data, CancellationToken::new(), safety)?;
        attempt.retry_permitted = turn;
        let (sender, mut observations) = mpsc::channel(64);
        let service =
            ResponsesAttemptService::new(client, Arc::clone(&dialect), data, authorization)?;
        let result = service.execute(attempt.with_observations(sender)).await;
        let retries: Vec<_> = drain_observations(&mut observations)
            .into_iter()
            .filter_map(|observation| match observation {
                ResponsesObservation::Retry(retry) if retry.loss().declined_reason.is_none() => {
                    Some(retry)
                }
                ResponsesObservation::Retry(_)
                | ResponsesObservation::Event(_)
                | ResponsesObservation::Activity(_) => None,
            })
            .collect();
        if sends == 2 {
            let response = result?;
            assert!(response.terminal().is_completed());
            assert_eq!(response.facts().physical_attempts(), 2);
            assert_eq!(retries.len(), 1);
            assert!(matches!(
                retries[0].cause(),
                ModelRetryCause::TransportAfterSend { .. }
            ));
            assert!(!retries[0].output_discarded());
            assert_eq!(
                (retries[0].next_physical(), retries[0].max_physical()),
                (2, 4)
            );
            assert!(retries[0].delay() <= Duration::from_secs(1));
        } else {
            let Err(error) = result else {
                panic!("{safety:?} turn={turn} must not be replayed");
            };
            assert!(matches!(error, ResponsesAttemptError::Transport { .. }));
            assert_eq!(error.facts().physical_attempts(), 1);
            assert_eq!(retries, [] as [ModelRetry; 0]);
        }
        assert_eq!(peer.await??.len(), sends);
    }
    Ok(())
}

/// A stream cut before any tool call is replayed; the receiver learns to
/// discard the failed request's events before the replay's arrive.
#[tokio::test]
async fn a_stream_cut_before_a_tool_call_is_replayed_and_its_output_discarded() -> TestResult {
    let (client_config, tls_identity, origin, peer) = responding_wire_peer_sequence(vec![
        http_response(
            "200 OK",
            Some("text/event-stream"),
            &text_stream_cut_before_terminal(),
        ),
        http_response("200 OK", Some("text/event-stream"), &complete_sse_body()),
    ])
    .await?;
    let client =
        Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
    let dialect = wire_dialect(origin, openai_generation_fingerprint(b"stream-cut"));
    let authorization = ResponsesAuthorizationAdapter::new(1, 1, wire_preflight, wire_revalidate)?;
    let data = attempt_data(&client, &authorization)?;
    let (sender, mut observations) = mpsc::channel(64);
    let attempt = executable_attempt(&data, CancellationToken::new())?.with_observations(sender);
    let service = ResponsesAttemptService::new(client, Arc::clone(&dialect), data, authorization)?;
    let response = service.execute(attempt).await?;
    assert!(response.terminal().is_completed());
    assert_eq!(response.facts().physical_attempts(), 2);
    let seen = drain_observations(&mut observations);
    let marker = seen
        .iter()
        .position(|observation| matches!(observation, ResponsesObservation::Retry(_)))
        .ok_or_else(|| io::Error::other("the replay was not announced"))?;
    let ResponsesObservation::Retry(retry) = &seen[marker] else {
        unreachable!();
    };
    assert!(retry.output_discarded());
    assert_eq!(retry.loss().assistant_text_bytes, 7);
    assert_eq!(
        retry.loss().transport_phase,
        Some(plakous_model::ModelRetryPhase::ResponseBody)
    );
    assert_eq!(
        retry.cause(),
        ModelRetryCause::StreamInterrupted {
            fault: ModelTransportFault::EndOfStream
        }
    );
    assert!(seen[..marker].iter().any(|observation| matches!(
        observation,
        ResponsesObservation::Event(ResponsesStreamEventKind::OutputTextDelta { .. })
    )));
    assert!(seen[marker + 1..].iter().all(|observation| !matches!(
        observation,
        ResponsesObservation::Event(ResponsesStreamEventKind::OutputTextDelta { .. })
            | ResponsesObservation::Retry(_)
    )));
    assert_eq!(peer.await??.len(), 2);
    Ok(())
}

/// An accepted stream that ends in the provider's own rate-limit or overload
/// refusal (a `response.failed` or an `error` event) is replayed for a
/// cost-only turn like any other mid-stream break, and a server-effect
/// request keeps its failed terminal after one request.
#[tokio::test]
async fn an_in_band_rate_limit_or_overload_is_replayed_only_for_a_cost_only_turn() -> TestResult {
    let created = serde_json::json!({"type":"response.created","sequence_number":0,
        "response":{"id":"resp_refused","object":"response","status":"in_progress","model":"gpt-test"}});
    let refusals = [
        (
            serde_json::json!({"type":"response.failed","sequence_number":1,
                "response":{"id":"resp_refused","object":"response","status":"failed","model":"gpt-test",
                    "error":{"code":"rate_limit_exceeded","message":"slow down"}}}),
            ModelTransportFault::RateLimited,
        ),
        (
            serde_json::json!({"type":"error","sequence_number":1,
                "error":{"type":"server_error","code":"server_is_overloaded","message":"busy"}}),
            ModelTransportFault::Overloaded,
        ),
    ];
    for (refusal, fault) in refusals {
        for safety in [ReplaySafety::CostOnly, ReplaySafety::ServerEffect] {
            let refused = http_response(
                "200 OK",
                Some("text/event-stream"),
                &sse(&[created.clone(), refusal.clone()]),
            );
            let accepted = http_response("200 OK", Some("text/event-stream"), &complete_sse_body());
            let replays = safety == ReplaySafety::CostOnly;
            let responses = if replays {
                vec![refused, accepted]
            } else {
                vec![refused]
            };
            let (client_config, tls_identity, origin, peer) =
                responding_wire_peer_sequence(responses).await?;
            let client = Client::with_tls_config_and_identity(
                Limits::default(),
                client_config,
                tls_identity,
            )?;
            let dialect = wire_dialect(origin, openai_generation_fingerprint(b"in-band"));
            let authorization =
                ResponsesAuthorizationAdapter::new(1, 1, wire_preflight, wire_revalidate)?;
            let data = attempt_data(&client, &authorization)?;
            let (sender, mut observations) = mpsc::channel(64);
            let attempt = attempt_with_safety(&data, CancellationToken::new(), safety)?
                .with_observations(sender);
            let ledger = attempt.core.observations();
            let service =
                ResponsesAttemptService::new(client, Arc::clone(&dialect), data, authorization)?;
            let response = service.execute(attempt).await?;
            let retries = drain_observations(&mut observations)
                .into_iter()
                .filter_map(|observation| match observation {
                    ResponsesObservation::Retry(retry)
                        if retry.loss().declined_reason.is_none() =>
                    {
                        Some(retry.cause())
                    }
                    ResponsesObservation::Retry(_)
                    | ResponsesObservation::Event(_)
                    | ResponsesObservation::Activity(_) => None,
                })
                .collect::<Vec<_>>();
            if replays {
                assert!(response.terminal().is_completed(), "{fault:?}");
                assert_eq!(response.facts().physical_attempts(), 2, "{fault:?}");
                assert_eq!(
                    retries,
                    [ModelRetryCause::StreamInterrupted { fault }],
                    "{fault:?}"
                );
            } else {
                assert!(!response.terminal().is_completed(), "{fault:?}");
                assert_eq!(response.facts().physical_attempts(), 1, "{fault:?}");
                assert!(retries.is_empty(), "{fault:?}");
                assert!(ledger.is_logically_terminal(), "{fault:?}");
            }
            assert_eq!(peer.await??.len(), if replays { 2 } else { 1 });
        }
    }
    Ok(())
}

/// Runs one cost-only attempt against a peer that answers its requests with
/// `bodies` in order; returns the response, the observations, whether the
/// ledger is logically terminal, and how many requests the peer saw.
async fn serve_in_band_refusals(
    bodies: Vec<Vec<u8>>,
    fingerprint_seed: &[u8],
) -> TestResult<(
    ResponsesAttemptResponse,
    Vec<ResponsesObservation>,
    bool,
    usize,
)> {
    let responses = bodies
        .iter()
        .map(|body| http_response("200 OK", Some("text/event-stream"), body))
        .collect();
    let (client_config, tls_identity, origin, peer) =
        responding_wire_peer_sequence(responses).await?;
    let client =
        Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
    let dialect = wire_dialect(origin, openai_generation_fingerprint(fingerprint_seed));
    let authorization = ResponsesAuthorizationAdapter::new(1, 1, wire_preflight, wire_revalidate)?;
    let data = attempt_data(&client, &authorization)?;
    let (sender, mut observations) = mpsc::channel(64);
    let mut prepared = prepared_attempt(&data)?;
    prepared.operation.replay_safety = ReplaySafety::CostOnly;
    // Room for the whole unadvised backoff (at most 1s + 2s + 4s).
    let attempt = ResponsesAttempt::from_prepared(
        prepared,
        typed_id::<AttemptId>(14)?,
        CancellationToken::new(),
        Instant::now() + Duration::from_secs(30),
    )
    .with_observations(sender);
    let ledger = attempt.core.observations();
    let service = ResponsesAttemptService::new(client, Arc::clone(&dialect), data, authorization)?;
    let response = service.execute(attempt).await?;
    let seen = drain_observations(&mut observations);
    let sends = peer.await??.len();
    Ok((response, seen, ledger.is_logically_terminal(), sends))
}

fn rate_limited_failure(sequence: u64) -> serde_json::Value {
    serde_json::json!({"type":"response.failed","sequence_number":sequence,
        "response":{"id":"resp_refused","object":"response","status":"failed","model":"gpt-test",
            "error":{"code":"rate_limit_exceeded","message":"slow down"}}})
}

/// Four in-band refusals spend the whole physical budget: the attempt ends in
/// one failed terminal after four requests, never a fifth.
#[tokio::test]
async fn in_band_refusals_that_exhaust_the_budget_end_in_one_failed_terminal() -> TestResult {
    let created = serde_json::json!({"type":"response.created","sequence_number":0,
        "response":{"id":"resp_refused","object":"response","status":"in_progress","model":"gpt-test"}});
    let refused = sse(&[created, rate_limited_failure(1)]);
    let (response, seen, terminal, sends) =
        serve_in_band_refusals(vec![refused; 4], b"in-band-exhausted").await?;
    assert!(!response.terminal().is_completed());
    assert_eq!(response.facts().physical_attempts(), 4);
    assert!(terminal);
    assert_eq!(
        seen.iter()
            .filter(|observation| matches!(observation, ResponsesObservation::Retry(retry) if retry.loss().declined_reason.is_none()))
            .count(),
        3
    );
    assert_eq!(sends, 4);
    Ok(())
}

/// An in-band refusal after a tool-call item began discards its partial and replays.
#[tokio::test]
async fn an_in_band_refusal_after_a_local_tool_call_began_is_replayed() -> TestResult {
    let refused = sse(&[
        serde_json::json!({"type":"response.created","sequence_number":0,
            "response":{"id":"resp_refused","object":"response","status":"in_progress","model":"gpt-test"}}),
        serde_json::json!({"type":"response.output_item.added","sequence_number":1,
            "output_index":0,"item":{"id":"fc_1","type":"function_call","call_id":"call_1",
                "name":"lookup","arguments":"","status":"in_progress"}}),
        serde_json::json!({"type":"error","sequence_number":2,
            "error":{"type":"server_error","code":"server_is_overloaded","message":"busy"}}),
    ]);
    let (response, seen, terminal, sends) =
        serve_in_band_refusals(vec![refused, complete_sse_body()], b"in-band-tool").await?;
    assert!(response.terminal().is_completed());
    assert_eq!(response.facts().physical_attempts(), 2);
    assert!(terminal);
    assert!(
        seen.iter()
            .any(|observation| matches!(observation, ResponsesObservation::Retry(retry) if retry.output_discarded()))
    );
    assert_eq!(sends, 2);
    Ok(())
}

/// A begun local tool call in an incomplete response is discarded before replay.
#[tokio::test]
async fn a_stream_cut_after_a_local_tool_call_began_is_replayed() -> TestResult {
    let (client_config, tls_identity, origin, peer) = responding_wire_peer_sequence(vec![
        http_response(
            "200 OK",
            Some("text/event-stream"),
            &tool_call_stream_cut_before_terminal(),
        ),
        http_response("200 OK", Some("text/event-stream"), &complete_sse_body()),
    ])
    .await?;
    let client =
        Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
    let dialect = wire_dialect(origin, openai_generation_fingerprint(b"tool-cut"));
    let authorization = ResponsesAuthorizationAdapter::new(1, 1, wire_preflight, wire_revalidate)?;
    let data = attempt_data(&client, &authorization)?;
    let (sender, mut observations) = mpsc::channel(64);
    let attempt = executable_attempt(&data, CancellationToken::new())?.with_observations(sender);
    let ledger = attempt.core.observations();
    let service = ResponsesAttemptService::new(client, Arc::clone(&dialect), data, authorization)?;
    let response = service.execute(attempt).await?;
    assert!(response.terminal().is_completed());
    assert_eq!(response.facts().physical_attempts(), 2);
    assert!(ledger.is_logically_terminal());
    assert!(
        drain_observations(&mut observations)
            .iter()
            .any(|observation| matches!(observation, ResponsesObservation::Retry(retry) if retry.output_discarded()))
    );
    let requests = peer.await??;
    assert_eq!(requests.len(), 2);
    let offsets = requests
        .iter()
        .map(|request| {
            request
                .windows(4)
                .position(|bytes| bytes == b"\r\n\r\n")
                .ok_or("request head boundary")
        })
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(
        &requests[0][offsets[0] + 4..],
        &requests[1][offsets[1] + 4..]
    );
    Ok(())
}

/// Each generation's head budget starts at its own dispatch. The first
/// generation's body stays open longer than that budget, and the replay then
/// waits out its backoff, so a budget counted from the first dispatch would
/// already be spent when the replay is sent. The budget itself is wide enough
/// that a loaded host's TLS handshake and response head fit inside it: 200ms
/// is only a liveness bound and has elapsed before the head on a loaded lane.
#[tokio::test]
async fn replay_wait_is_not_bounded_by_first_generation_head_deadline() -> TestResult {
    const HEAD_BUDGET: Duration = Duration::from_secs(3);
    let (client_config, tls_identity, origin, peer) = responding_wire_peer_sequence_holding_first(
        vec![
            http_response(
                "200 OK",
                Some("text/event-stream"),
                &tool_call_stream_cut_before_terminal(),
            ),
            http_response("200 OK", Some("text/event-stream"), &complete_sse_body()),
        ],
        HEAD_BUDGET + Duration::from_millis(500),
    )
    .await?;
    let client =
        Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
    let dialect = wire_dialect(origin, openai_generation_fingerprint(b"tool-cut"));
    let authorization = ResponsesAuthorizationAdapter::new(1, 1, wire_preflight, wire_revalidate)?;
    let data = attempt_data(&client, &authorization)?;
    let (sender, mut observations) = mpsc::channel(64);
    let mut attempt =
        executable_attempt(&data, CancellationToken::new())?.with_observations(sender);
    attempt.dispatch_timeout = HEAD_BUDGET;
    attempt.deadline = Instant::now() + attempt.dispatch_timeout;
    let ledger = attempt.core.observations();
    let service = ResponsesAttemptService::new(client, Arc::clone(&dialect), data, authorization)?;
    let response = timeout(Duration::from_secs(30), service.execute(attempt))
        .await
        .map_err(|_| "replay did not finish")??;
    assert!(response.terminal().is_completed());
    assert_eq!(response.facts().physical_attempts(), 2);
    assert!(ledger.is_logically_terminal());
    assert!(
        drain_observations(&mut observations)
            .iter()
            .any(|observation| matches!(observation, ResponsesObservation::Retry(retry) if retry.output_discarded()))
    );
    let requests = peer.await??;
    assert_eq!(requests.len(), 2);
    let offsets = requests
        .iter()
        .map(|request| {
            request
                .windows(4)
                .position(|bytes| bytes == b"\r\n\r\n")
                .ok_or("request head boundary")
        })
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(
        &requests[0][offsets[0] + 4..],
        &requests[1][offsets[1] + 4..]
    );
    Ok(())
}

/// Through the model host, a replayed stream surfaces one discarding retry
/// event and completes with only the replay's response.
#[tokio::test]
async fn shared_model_host_replays_a_cut_stream_as_a_fresh_response() -> TestResult {
    let (client_config, tls_identity, origin, peer) = responding_wire_peer_sequence(vec![
        http_response(
            "200 OK",
            Some("text/event-stream"),
            &text_stream_cut_before_terminal(),
        ),
        http_response("200 OK", Some("text/event-stream"), &complete_sse_body()),
    ])
    .await?;
    let client =
        Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
    let dialect = wire_dialect(origin, openai_generation_fingerprint(b"model-host-cut"));
    let (execution, service, context, _, _) = prepared_model_host(
        client,
        dialect,
        64,
        false,
        None,
        ResponsesHostedProgramContext::default(),
        false,
    )?;
    let stream = start_prepared_responses_model_call(
        execution,
        service,
        context,
        Instant::now() + Duration::from_secs(5),
    )
    .await?;
    let events = collect_model_events(stream).await;
    let events = events.into_iter().collect::<Result<Vec<_>, _>>()?;
    let retry = events
        .iter()
        .position(|event| matches!(event, ModelEvent::Retry(retry) if retry.output_discarded()))
        .ok_or_else(|| io::Error::other("the replay was not surfaced"))?;
    let completed = events
        .iter()
        .position(|event| matches!(event, ModelEvent::ResponseCompleted(_)))
        .ok_or_else(|| io::Error::other("the replay did not complete"))?;
    assert!(retry < completed);
    assert_eq!(peer.await??.len(), 2);
    Ok(())
}

#[tokio::test]
async fn in_band_content_policy_refusal_never_replays() -> TestResult {
    for label in [
        "content_policy_violation",
        "content_filter",
        "cyber_safety_violation",
    ] {
        let body = sse(&[
            serde_json::json!({"type":"response.created","sequence_number":0,
                "response":{"id":"resp_policy","object":"response","status":"in_progress","model":"gpt-test"}}),
            serde_json::json!({"type":"response.failed","sequence_number":1,
                "response":{"id":"resp_policy","object":"response","status":"failed","model":"gpt-test",
                    "error":{"code":label,"message":"This content was flagged for possible cybersecurity risk"}}}),
        ]);
        let response = http_response("200 OK", Some("text/event-stream"), &body);
        let (config, identity, origin, peer) =
            responding_wire_peer_sequence(vec![response]).await?;
        let client = Client::with_tls_config_and_identity(Limits::default(), config, identity)?;
        let dialect = wire_dialect(origin, openai_generation_fingerprint(b"terminal-policy"));
        let authorization =
            ResponsesAuthorizationAdapter::new(1, 1, wire_preflight, wire_revalidate)?;
        let data = attempt_data(&client, &authorization)?;
        let (sender, mut observations) = mpsc::channel(64);
        let attempt = attempt_with_safety(&data, CancellationToken::new(), ReplaySafety::CostOnly)?
            .with_observations(sender);
        let ledger = attempt.core.observations();
        let service = ResponsesAttemptService::new(client, dialect, data, authorization)?;
        let response = service.execute(attempt).await?;
        assert!(!response.terminal().is_completed());
        assert_eq!(response.facts().physical_attempts(), 1);
        assert!(ledger.is_logically_terminal());
        assert!(
            !drain_observations(&mut observations)
                .iter()
                .any(|event| matches!(event, ResponsesObservation::Retry(_)))
        );
        assert_eq!(peer.await??.len(), 1);
    }
    Ok(())
}

#[tokio::test]
async fn terminal_retry_advice_and_quota_bodies_never_replay() -> TestResult {
    for (code, header) in [
        ("rate_limit_exceeded", "x-should-retry: false\r\n"),
        ("insufficient_quota", ""),
        ("credit_balance_exhausted", ""),
        ("usage_not_included", ""),
    ] {
        let body = serde_json::to_vec(
            &serde_json::json!({"error":{"code":code,"message":"fixture refusal"}}),
        )?;
        let response = http_response_with(
            "429 Fixture",
            Some("application/json"),
            &format!("Retry-After: 0\r\n{header}"),
            &body,
        );
        let (client_config, tls_identity, origin, peer) =
            responding_wire_peer_sequence(vec![response]).await?;
        let client =
            Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
        let dialect = wire_dialect(origin, openai_generation_fingerprint(b"terminal-quota"));
        let authorization =
            ResponsesAuthorizationAdapter::new(1, 1, wire_preflight, wire_revalidate)?;
        let data = attempt_data(&client, &authorization)?;
        let attempt = executable_attempt(&data, CancellationToken::new())?;
        let service = ResponsesAttemptService::new(client, dialect, data, authorization)?;
        let Err(error) = service.execute(attempt).await else {
            panic!("terminal refusal");
        };
        assert_eq!(error.facts().physical_attempts(), 1, "{code}");
        assert_eq!(peer.await??.len(), 1);
    }
    Ok(())
}

#[tokio::test]
async fn unreplayed_in_band_refusals_finalize_workflows_and_retry_disabled_turns() -> TestResult {
    let created = serde_json::json!({"type":"response.created","sequence_number":0,
        "response":{"id":"resp_refused","object":"response","status":"in_progress","model":"gpt-test"}});
    let refusals = [
        rate_limited_failure(1),
        serde_json::json!({"type":"error","sequence_number":1,
            "error":{"type":"server_error","code":"server_is_overloaded","message":"busy"}}),
    ];
    for workflow in [true, false] {
        for refusal in &refusals {
            let response = http_response_with(
                "200 OK",
                Some("text/event-stream"),
                if workflow {
                    ""
                } else {
                    "x-should-retry: false\r\n"
                },
                &sse(&[created.clone(), refusal.clone()]),
            );
            let (client_config, tls_identity, origin, peer) =
                responding_wire_peer_sequence(vec![response]).await?;
            let client = Client::with_tls_config_and_identity(
                Limits::default(),
                client_config,
                tls_identity,
            )?;
            let dialect = wire_dialect(origin, openai_generation_fingerprint(b"terminal-in-band"));
            let authorization =
                ResponsesAuthorizationAdapter::new(1, 1, wire_preflight, wire_revalidate)?;
            let data = attempt_data(&client, &authorization)?;
            let prepared = if workflow {
                // Compaction, titles and context measurement use this same
                // session-workflow authority and unsent-only retry gate.
                let metadata = RequestMetadata::for_session_workflow(
                    typed_id::<RunId>(10)?,
                    SessionWorkflowRootId::new([7; 32])?,
                    typed_id::<AttemptId>(12)?,
                    Timestamp::new(1_700_000_000, 0)?,
                );
                let (request, authority) = exact_request_authority_with_metadata(false, metadata)?;
                prepare_responses_attempt(authority, &request, None, &data)?
            } else {
                prepared_attempt(&data)?
            };
            assert_eq!(prepared.retry_permitted, !workflow);
            let (sender, mut observations) = mpsc::channel(64);
            let attempt = ResponsesAttempt::from_prepared(
                prepared,
                typed_id::<AttemptId>(14)?,
                CancellationToken::new(),
                Instant::now() + Duration::from_secs(5),
            )
            .with_observations(sender);
            let lifecycle = attempt.core.lifecycle();
            let ledger = attempt.core.observations();
            let service = ResponsesAttemptService::new(client, dialect, data, authorization)?;
            let response = service.execute(attempt).await?;
            assert!(!response.terminal().is_completed());
            assert_eq!(lifecycle.load(Ordering::Acquire), plakous_attempt::FAILED);
            assert!(ledger.is_logically_terminal());
            assert_eq!(
                response.facts(),
                TerminalFacts::from_snapshot(ledger.snapshot())
                    .with_summary(ledger.attempt_summary())
            );
            assert_eq!(response.facts().physical_attempts(), 1);
            assert_eq!(
                response.facts().provider_acceptance(),
                ProviderAcceptance::Accepted
            );
            assert!(!ledger.snapshot().logical_success);
            assert!(
                drain_observations(&mut observations)
                    .iter()
                    .all(|o| !matches!(o, ResponsesObservation::Retry(retry) if retry.loss().declined_reason.is_none()))
            );
            assert!(matches!(
                observations.try_recv(),
                Err(mpsc::error::TryRecvError::Disconnected)
            ));
            assert_eq!(peer.await??.len(), 1);
        }
    }
    Ok(())
}

#[tokio::test]
async fn terminal_retry_advice_on_a_success_head_suppresses_stream_replay() -> TestResult {
    let response = http_response_with(
        "200 OK",
        Some("text/event-stream"),
        "x-should-retry: false\r\n",
        &text_stream_cut_before_terminal(),
    );
    let (client_config, tls_identity, origin, peer) =
        responding_wire_peer_sequence(vec![response]).await?;
    let client =
        Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
    let dialect = wire_dialect(origin, openai_generation_fingerprint(b"terminal-stream"));
    let authorization = ResponsesAuthorizationAdapter::new(1, 1, wire_preflight, wire_revalidate)?;
    let data = attempt_data(&client, &authorization)?;
    let attempt = executable_attempt(&data, CancellationToken::new())?;
    let service = ResponsesAttemptService::new(client, dialect, data, authorization)?;
    let Err(error) = service.execute(attempt).await else {
        panic!("cut stream");
    };
    assert_eq!(error.facts().physical_attempts(), 1);
    assert_eq!(peer.await??.len(), 1);
    Ok(())
}

#[tokio::test]
async fn a_session_workflow_retries_unsent_tls_but_never_an_http_refusal() -> TestResult {
    for accepted in [false, true] {
        let WireEndpoint {
            listener,
            acceptor,
            client_config,
            tls_identity,
            origin,
        } = wire_endpoint().await?;
        let response = if accepted {
            http_response("200 OK", Some("text/event-stream"), &complete_sse_body())
        } else {
            refusal("503 Service Unavailable", "0")
        };
        let peer = tokio::spawn(async move {
            let (unsent, _) = listener.accept().await?;
            drop(unsent);
            let (stream, _) = listener.accept().await?;
            let mut stream = acceptor.accept(stream).await?;
            let request = read_wire_request(&mut stream).await?;
            stream.write_all(&response).await?;
            stream.shutdown().await?;
            Ok::<_, TestError>(request)
        });
        let client =
            Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
        let dialect = wire_dialect(origin, openai_generation_fingerprint(b"workflow-unsent"));
        let authorization =
            ResponsesAuthorizationAdapter::new(1, 1, wire_preflight, wire_revalidate)?;
        let data = attempt_data(&client, &authorization)?;
        let metadata = RequestMetadata::for_session_workflow(
            typed_id::<RunId>(10)?,
            SessionWorkflowRootId::new([7; 32])?,
            typed_id::<AttemptId>(12)?,
            Timestamp::new(1_700_000_000, 0)?,
        );
        let (request, authority) = exact_request_authority_with_metadata(false, metadata)?;
        let prepared = prepare_responses_attempt(authority, &request, None, &data)?;
        let (sender, mut observations) = mpsc::channel(64);
        let attempt = ResponsesAttempt::from_prepared(
            prepared,
            typed_id::<AttemptId>(14)?,
            CancellationToken::new(),
            Instant::now() + Duration::from_secs(5),
        )
        .with_observations(sender);
        let service =
            ResponsesAttemptService::new(client, Arc::clone(&dialect), data, authorization)?;
        let result = service.execute(attempt).await;
        let attempts = if accepted {
            result?.facts().physical_attempts()
        } else {
            let Err(error) = result else {
                panic!("workflow HTTP refusal must not replay");
            };
            assert!(matches!(error, ResponsesAttemptError::HttpStatus { .. }));
            error.facts().physical_attempts()
        };
        assert_eq!(attempts, 2);
        assert_eq!(dialect.preflights.load(Ordering::SeqCst), 2);
        assert_eq!(dialect.final_authorizations.load(Ordering::SeqCst), 2);
        let mut retries = Vec::new();
        while let Ok(observation) = observations.try_recv() {
            if let ResponsesObservation::Retry(retry) = observation
                && retry.loss().declined_reason.is_none()
            {
                retries.push(retry);
            }
        }
        assert_eq!(retries.len(), 1);
        assert_eq!(retries[0].cause(), ModelRetryCause::Connect);
        assert_eq!(
            retries[0].loss().transport_phase,
            Some(plakous_model::ModelRetryPhase::Tls)
        );
        assert_ne!(peer.await??, [] as [u8; 0]);
    }
    Ok(())
}

fn refuse_wire_redispatch(
    dialect: &WireDialect,
    request: ResponsesRevalidationRequest<'_, WireGuard>,
) -> Result<(), ResponsesDispatchRejection> {
    if dialect.final_authorizations.load(Ordering::SeqCst) != 0 {
        dialect.final_authorizations.fetch_add(1, Ordering::SeqCst);
        return Err(ResponsesDispatchRejection::Binding);
    }
    wire_revalidate(dialect, request)
}

#[tokio::test]
async fn record_rejected_workflow_resends_immediately_on_a_fresh_connection() -> TestResult {
    let (client_config, tls_identity, origin, peer) =
        responding_wire_peer_sequence(vec![http_response(
            "200 OK",
            Some("text/event-stream"),
            &complete_sse_body(),
        )])
        .await?;
    let client =
        Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?
            .pooled()
            .with_faults(Some(plakous_http::FaultPlan::parse(
                "fault=record-rejected,nth=1",
            )?));
    let dialect = wire_dialect(origin, openai_generation_fingerprint(b"record-rejected"));
    let authorization = ResponsesAuthorizationAdapter::new(1, 1, wire_preflight, wire_revalidate)?;
    let data = attempt_data(&client, &authorization)?;
    let metadata = RequestMetadata::for_session_workflow(
        typed_id::<RunId>(10)?,
        SessionWorkflowRootId::new([7; 32])?,
        typed_id::<AttemptId>(12)?,
        Timestamp::new(1_700_000_000, 0)?,
    );
    let (request, authority) = exact_request_authority_with_metadata(false, metadata)?;
    let prepared = prepare_responses_attempt(authority, &request, None, &data)?;
    let (sender, mut observations) = mpsc::channel(64);
    let attempt = ResponsesAttempt::from_prepared(
        prepared,
        typed_id::<AttemptId>(14)?,
        CancellationToken::new(),
        Instant::now() + Duration::from_secs(5),
    )
    .with_observations(sender);
    let service = ResponsesAttemptService::new(client, Arc::clone(&dialect), data, authorization)?;
    assert_eq!(
        service.execute(attempt).await?.facts().physical_attempts(),
        2
    );
    assert_eq!(dialect.final_authorizations.load(Ordering::SeqCst), 2);
    let retries: Vec<_> = drain_observations(&mut observations)
        .into_iter()
        .filter_map(|o| match o {
            ResponsesObservation::Retry(r) => Some(r),
            _ => None,
        })
        .collect();
    assert_eq!(retries.len(), 1);
    assert_eq!(retries[0].cause().kind(), "record_rejected_in_transit");
    assert_eq!(
        retries[0].cause().to_string(),
        "request corrupted in transit (TLS record rejected)"
    );
    assert_eq!(retries[0].delay(), Duration::ZERO);
    let (host, route, bytes) = retries[0].request().ok_or("request logging facts")?;
    assert_eq!(host, "127.0.0.1");
    assert!(route.ends_with("/responses"));
    assert!(bytes > 0);
    assert_eq!(peer.await??.len(), 1);
    Ok(())
}

#[tokio::test]
async fn workflow_connect_retry_is_refused_when_fresh_revalidation_fails() -> TestResult {
    let endpoint = wire_endpoint().await?;
    let client = Client::with_tls_config_and_identity(
        Limits::default(),
        endpoint.client_config,
        endpoint.tls_identity,
    )?
    .with_faults(Some(plakous_http::FaultPlan::parse("fault=connect,nth=1")?));
    let dialect = wire_dialect(
        endpoint.origin,
        openai_generation_fingerprint(b"workflow-revalidation"),
    );
    let authorization =
        ResponsesAuthorizationAdapter::new(1, 1, wire_preflight, refuse_wire_redispatch)?;
    let data = attempt_data(&client, &authorization)?;
    let metadata = RequestMetadata::for_session_workflow(
        typed_id::<RunId>(10)?,
        SessionWorkflowRootId::new([7; 32])?,
        typed_id::<AttemptId>(12)?,
        Timestamp::new(1_700_000_000, 0)?,
    );
    let (request, authority) = exact_request_authority_with_metadata(false, metadata)?;
    let prepared = prepare_responses_attempt(authority, &request, None, &data)?;
    let attempt = ResponsesAttempt::from_prepared(
        prepared,
        typed_id::<AttemptId>(14)?,
        CancellationToken::new(),
        Instant::now() + Duration::from_secs(5),
    );
    let service = ResponsesAttemptService::new(client, Arc::clone(&dialect), data, authorization)?;
    let Err(error) = service.execute(attempt).await else {
        panic!("a resend needs fresh authorization");
    };
    assert!(matches!(error, ResponsesAttemptError::Revalidation { .. }));
    assert_eq!(error.facts().physical_attempts(), 2);
    assert_eq!(dialect.preflights.load(Ordering::SeqCst), 2);
    assert_eq!(dialect.final_authorizations.load(Ordering::SeqCst), 2);
    assert_eq!(dialect.guards_dropped.load(Ordering::SeqCst), 2);
    assert!(
        timeout(Duration::from_millis(100), endpoint.listener.accept())
            .await
            .is_err()
    );
    Ok(())
}

#[test]
fn keepalives_do_not_start_or_extend_the_semantic_stall_deadline() {
    let start = Instant::now();
    let mut progress = StreamProgress::default();
    let keepalive = ResponsesStreamEventKind::Unknown {
        kind: "ping".to_owned(),
        native: serde_json::json!({"type":"ping"}),
    };
    progress.observe_semantic_event(&keepalive, start);
    assert_eq!(progress.semantic_deadline(), None);
    let output = ResponsesStreamEventKind::CustomToolCallInputDelta {
        output_index: 0,
        item_id: Some("call-1".to_owned()),
        delta: "input".to_owned(),
    };
    progress.observe_semantic_event(&output, start);
    assert_eq!(
        progress.semantic_deadline(),
        Some(start + Duration::from_secs(300))
    );
    progress.observe_semantic_event(&keepalive, start + Duration::from_secs(250));
    assert_eq!(
        progress.semantic_deadline(),
        Some(start + Duration::from_secs(300))
    );
    assert_eq!(progress.activity.keepalive_count, 2);
    assert_eq!(progress.activity.max_semantic_gap_ms, 250_000);
    progress.observe_semantic_event(&output, start + Duration::from_secs(299));
    assert_eq!(
        progress.semantic_deadline(),
        Some(start + Duration::from_secs(599))
    );
    progress.observe_semantic_event(
        &ResponsesStreamEventKind::Opaque {
            kind: "response.progress".to_owned(),
        },
        start + Duration::from_secs(400),
    );
    assert_eq!(
        progress.semantic_deadline(),
        Some(start + Duration::from_secs(700))
    );
    assert_eq!(progress.activity.keepalive_count, 2);
}

async fn controlled_stream_peer(
    prefixes: Vec<Vec<u8>>,
    complete_replay: bool,
) -> TestResult<(
    ClientConfig,
    TlsConfigIdentity,
    Origin,
    mpsc::Sender<Option<Vec<u8>>>,
    JoinHandle<TestResult<usize>>,
)> {
    let WireEndpoint {
        listener,
        acceptor,
        client_config,
        tls_identity,
        origin,
    } = wire_endpoint().await?;
    let (send, mut commands) = mpsc::channel::<Option<Vec<u8>>>(8);
    let peer = tokio::spawn(async move {
        let mut requests = 0;
        for prefix in prefixes {
            let (stream, _) = timeout(Duration::from_secs(5), listener.accept()).await??;
            let mut stream = acceptor.accept(stream).await?;
            let _ = read_wire_request(&mut stream).await?;
            requests += 1;
            stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n").await?;
            stream
                .write_all(format!("{:x}\r\n", prefix.len()).as_bytes())
                .await?;
            stream.write_all(&prefix).await?;
            stream.write_all(b"\r\n").await?;
            while let Some(Some(bytes)) = commands.recv().await {
                stream
                    .write_all(format!("{:x}\r\n", bytes.len()).as_bytes())
                    .await?;
                stream.write_all(&bytes).await?;
                stream.write_all(b"\r\n").await?;
                if bytes.is_empty() {
                    break;
                }
            }
            let _ = stream.shutdown().await;
        }
        if complete_replay {
            let (stream, _) = timeout(Duration::from_secs(5), listener.accept()).await??;
            let mut stream = acceptor.accept(stream).await?;
            let _ = read_wire_request(&mut stream).await?;
            stream
                .write_all(&http_response(
                    "200 OK",
                    Some("text/event-stream"),
                    &complete_sse_body(),
                ))
                .await?;
            stream.shutdown().await?;
            requests += 1;
        }
        Ok(requests)
    });
    Ok((client_config, tls_identity, origin, send, peer))
}

async fn observe_activity(
    observations: &mut mpsc::Receiver<ResponsesObservation>,
    output: bool,
    keepalives: u64,
) -> TestResult {
    loop {
        let event = timeout(Duration::from_secs(5), observations.recv())
            .await?
            .ok_or("observation channel closed")?;
        if !output
            && keepalives == 0
            && matches!(
                event,
                ResponsesObservation::Event(ResponsesStreamEventKind::ResponseCreated { .. })
            )
        {
            return Ok(());
        }
        if let ResponsesObservation::Activity(activity) = event
            && activity.output_started == output
            && activity.keepalive_count >= keepalives
        {
            return Ok(());
        }
    }
}

async fn observe_output(observations: &mut mpsc::Receiver<ResponsesObservation>) -> TestResult {
    loop {
        let event = timeout(Duration::from_secs(5), observations.recv())
            .await?
            .ok_or("observation channel closed")?;
        if matches!(
            event,
            ResponsesObservation::Event(ResponsesStreamEventKind::OutputTextDelta { .. })
        ) {
            return Ok(());
        }
    }
}

async fn advance_with_keepalive(
    commands: &mpsc::Sender<Option<Vec<u8>>>,
    observations: &mut mpsc::Receiver<ResponsesObservation>,
    output: bool,
    keepalives: u64,
) -> TestResult {
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(100)).await;
    tokio::time::resume();
    commands.send(Some(b": keepalive\n\n".to_vec())).await?;
    observe_activity(observations, output, keepalives).await
}

#[tokio::test]
async fn a_keepalive_masked_stall_after_the_first_head_deadline_is_replayed() -> TestResult {
    let (config, identity, origin, commands, peer) =
        controlled_stream_peer(vec![text_stream_cut_before_terminal()], true).await?;
    let client = Client::with_tls_config_and_identity(
        Limits {
            response_body_timeout: Duration::from_secs(180),
            ..Limits::default()
        },
        config,
        identity,
    )?;
    let dialect = wire_dialect(origin, openai_generation_fingerprint(b"semantic-stall"));
    let authorization = ResponsesAuthorizationAdapter::new(1, 1, wire_preflight, wire_revalidate)?;
    let data = attempt_data(&client, &authorization)?;
    let (send, mut observations) = mpsc::channel(64);
    let attempt = executable_attempt(&data, CancellationToken::new())?.with_observations(send);
    let service = ResponsesAttemptService::new(client, dialect, data, authorization)?;
    let operation = tokio::spawn(async move { service.execute(attempt).await });
    observe_output(&mut observations).await?;
    advance_with_keepalive(&commands, &mut observations, true, 1).await?;
    advance_with_keepalive(&commands, &mut observations, true, 2).await?;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(101)).await;
    tokio::time::resume();
    let retry = loop {
        if let ResponsesObservation::Retry(retry) =
            timeout(Duration::from_secs(5), observations.recv())
                .await?
                .ok_or("retry channel closed")?
        {
            break retry;
        }
    };
    assert_eq!(
        retry.cause(),
        ModelRetryCause::StreamInterrupted {
            fault: ModelTransportFault::Timeout
        }
    );
    assert!(retry.output_discarded());
    assert!(retry.loss().elapsed_ms > 300_000);
    assert_eq!(
        retry.loss().transport_phase,
        Some(plakous_model::ModelRetryPhase::ResponseBody)
    );
    commands.send(None).await?;
    let response = timeout(Duration::from_secs(5), operation).await???;
    assert!(response.terminal().is_completed());
    assert_eq!(response.facts().physical_attempts(), 2);
    assert_eq!(peer.await??, 2);
    Ok(())
}

#[tokio::test]
async fn reasoning_items_before_output_have_no_semantic_stall_bound() -> TestResult {
    reasoning_keepalives_have_no_semantic_stall_bound(Some(false)).await
}

#[tokio::test]
async fn a_hosted_item_after_visible_text_suspends_the_semantic_stall_bound() -> TestResult {
    let (config, identity, origin, commands, peer) =
        controlled_stream_peer(vec![text_stream_cut_before_terminal()], false).await?;
    let client = Client::with_tls_config_and_identity(
        Limits {
            response_body_timeout: Duration::from_secs(180),
            ..Limits::default()
        },
        config,
        identity,
    )?;
    let dialect = wire_dialect(origin, openai_generation_fingerprint(b"hosted-stall"));
    let authorization = ResponsesAuthorizationAdapter::new(1, 1, wire_preflight, wire_revalidate)?;
    let data = attempt_data(&client, &authorization)?;
    let (send, mut observations) = mpsc::channel(64);
    let attempt = executable_attempt(&data, CancellationToken::new())?.with_observations(send);
    let service = ResponsesAttemptService::new(client, dialect, data, authorization)?;
    let operation = tokio::spawn(async move { service.execute(attempt).await });
    observe_output(&mut observations).await?;
    commands
        .send(Some(sse(&[serde_json::json!({
            "type":"response.output_item.added","sequence_number":4,"output_index":1,
            "item":{"type":"web_search_call","id":"search-1","status":"in_progress"}
        })])))
        .await?;
    loop {
        let event = timeout(Duration::from_secs(5), observations.recv())
            .await?
            .ok_or("hosted item channel closed")?;
        if matches!(
            event,
            ResponsesObservation::Event(ResponsesStreamEventKind::OutputItemAdded {
                item: ResponsesOutputItem::Opaque { .. },
                ..
            })
        ) {
            break;
        }
    }
    for count in 1..=4 {
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(100)).await;
        tokio::time::resume();
        let bytes = if count % 2 == 0 {
            sse(&[
                serde_json::json!({"type":"response.web_search_call.searching",
                "sequence_number":4 + count / 2,"output_index":1}),
            ])
        } else {
            b": keepalive\n\n".to_vec()
        };
        commands.send(Some(bytes)).await?;
        observe_activity(&mut observations, true, count).await?;
    }
    assert!(!operation.is_finished());
    let message = serde_json::json!({"id":"msg_cut","type":"message","role":"assistant",
        "content":[{"type":"output_text","text":"partial","annotations":[]}]});
    let hosted = serde_json::json!({"type":"web_search_call","id":"search-1","status":"completed"});
    commands
        .send(Some(sse(&[
            serde_json::json!({"type":"response.output_text.done","sequence_number":7,
                "item_id":"msg_cut","output_index":0,"content_index":0,"text":"partial"}),
            serde_json::json!({"type":"response.content_part.done","sequence_number":8,
                "item_id":"msg_cut","output_index":0,"content_index":0,
                "part":{"type":"output_text","text":"partial","annotations":[]}}),
            serde_json::json!({"type":"response.output_item.done","sequence_number":9,
                "output_index":0,"item":message}),
            serde_json::json!({"type":"response.output_item.done","sequence_number":10,
                "output_index":1,"item":hosted}),
            serde_json::json!({
                "type":"response.completed","sequence_number":11,
                "response":{"id":"resp_cut","object":"response","status":"completed",
                    "model":"gpt-test","output":[
                        message, hosted
                    ]}
            }),
        ])))
        .await?;
    commands.send(Some(Vec::new())).await?;
    let response = timeout(Duration::from_secs(5), operation).await???;
    assert!(response.terminal().is_completed());
    assert_eq!(response.facts().physical_attempts(), 1);
    assert_eq!(peer.await??, 1);
    Ok(())
}

#[tokio::test]
async fn reasoning_summary_parts_before_output_have_no_semantic_stall_bound() -> TestResult {
    reasoning_keepalives_have_no_semantic_stall_bound(Some(true)).await
}

#[tokio::test]
async fn keepalives_before_output_have_no_semantic_stall_bound() -> TestResult {
    reasoning_keepalives_have_no_semantic_stall_bound(None).await
}

async fn reasoning_keepalives_have_no_semantic_stall_bound(summary: Option<bool>) -> TestResult {
    let complete = complete_sse_body();
    let boundary = complete
        .windows(2)
        .position(|bytes| bytes == b"\n\n")
        .ok_or("SSE boundary")?
        + 2;
    let mut prefix = complete[..boundary].to_vec();
    if summary.is_some() {
        prefix.extend(sse(&[serde_json::json!({"type":"response.output_item.added","sequence_number":1,
            "output_index":0,"item":{"type":"reasoning","id":"reasoning-1","status":"in_progress","summary":[]}})]));
    }
    if summary == Some(true) {
        prefix.extend(sse(&[
            serde_json::json!({"type":"response.reasoning_summary_part.added","sequence_number":2,
            "output_index":0,"summary_index":0,"part":{"type":"summary_text","text":""}}),
        ]));
    }
    let (config, identity, origin, commands, peer) =
        controlled_stream_peer(vec![prefix], false).await?;
    let client = Client::with_tls_config_and_identity(
        Limits {
            response_body_timeout: Duration::from_secs(180),
            ..Limits::default()
        },
        config,
        identity,
    )?;
    let dialect = wire_dialect(origin, openai_generation_fingerprint(b"hidden-reasoning"));
    let authorization = ResponsesAuthorizationAdapter::new(1, 1, wire_preflight, wire_revalidate)?;
    let data = attempt_data(&client, &authorization)?;
    let (send, mut observations) = mpsc::channel(64);
    let attempt = executable_attempt(&data, CancellationToken::new())?.with_observations(send);
    let service = ResponsesAttemptService::new(client, dialect, data, authorization)?;
    let operation = tokio::spawn(async move { service.execute(attempt).await });
    observe_activity(&mut observations, false, 0).await?;
    for count in 1..=4 {
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(100)).await;
        tokio::time::resume();
        commands.send(Some(b": keepalive\n\n".to_vec())).await?;
        loop {
            let event = timeout(Duration::from_secs(5), observations.recv())
                .await?
                .ok_or("keepalive observation channel closed")?;
            if matches!(event, ResponsesObservation::Activity(activity) if activity.keepalive_count >= count)
            {
                break;
            }
        }
    }
    assert!(!operation.is_finished());
    let suffix = if let Some(summary) = summary {
        let mut sequence = if summary { 3 } else { 2 };
        let mut events = Vec::new();
        if summary {
            events.push(
                serde_json::json!({"type":"response.reasoning_summary_text.done",
                "sequence_number":sequence,"item_id":"reasoning-1","output_index":0,
                "summary_index":0,"text":""}),
            );
            sequence += 1;
            events.push(
                serde_json::json!({"type":"response.reasoning_summary_part.done",
                "sequence_number":sequence,"item_id":"reasoning-1","output_index":0,
                "summary_index":0,"part":{"type":"summary_text","text":""}}),
            );
            sequence += 1;
        }
        let item = serde_json::json!({"type":"reasoning","id":"reasoning-1","status":"completed",
            "summary": if summary { serde_json::json!([{"type":"summary_text","text":""}]) } else { serde_json::json!([]) }});
        events.push(serde_json::json!({"type":"response.output_item.done",
            "sequence_number":sequence,"output_index":0,"item":item}));
        events.push(serde_json::json!({"type":"response.completed","sequence_number":sequence + 1,
            "response":{"id":"resp_1","object":"response","status":"completed","model":"gpt-test","output":[item]}}));
        sse(&events)
    } else {
        complete[boundary..].to_vec()
    };
    commands.send(Some(suffix)).await?;
    commands.send(Some(Vec::new())).await?;
    let response = timeout(Duration::from_secs(5), operation).await???;
    assert!(response.terminal().is_completed());
    assert_eq!(response.facts().physical_attempts(), 1);
    assert_eq!(peer.await??, 1);
    Ok(())
}

#[tokio::test]
async fn a_second_long_failure_ends_the_attempt_without_a_third_dispatch() -> TestResult {
    let (config, identity, origin, commands, peer) =
        controlled_stream_peer(vec![text_stream_cut_before_terminal(); 2], false).await?;
    let client = Client::with_tls_config_and_identity(
        Limits {
            response_body_timeout: Duration::from_secs(180),
            ..Limits::default()
        },
        config,
        identity,
    )?;
    let dialect = wire_dialect(origin, openai_generation_fingerprint(b"long-cap"));
    let authorization = ResponsesAuthorizationAdapter::new(1, 1, wire_preflight, wire_revalidate)?;
    let data = attempt_data(&client, &authorization)?;
    let (send, mut observations) = mpsc::channel(64);
    let attempt = executable_attempt(&data, CancellationToken::new())?.with_observations(send);
    let service = ResponsesAttemptService::new(client, dialect, data, authorization)?;
    let operation = tokio::spawn(async move { service.execute(attempt).await });
    for physical in 1..=2 {
        let previous_keepalives = (physical - 1) * 2;
        observe_output(&mut observations).await?;
        advance_with_keepalive(&commands, &mut observations, true, previous_keepalives + 1).await?;
        advance_with_keepalive(&commands, &mut observations, true, previous_keepalives + 2).await?;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(101)).await;
        tokio::time::resume();
        let retry = loop {
            if let ResponsesObservation::Retry(retry) =
                timeout(Duration::from_secs(5), observations.recv())
                    .await?
                    .ok_or("retry channel closed")?
            {
                break retry;
            }
        };
        assert!(retry.loss().elapsed_ms > 300_000);
        assert_eq!(retry.loss().assistant_text_bytes, 7);
        if physical == 1 {
            assert!(retry.output_discarded());
            assert_eq!(retry.loss().declined_reason, None);
        } else {
            assert!(!retry.output_discarded());
            assert_eq!(
                retry.loss().declined_reason,
                Some(plakous_model::ModelRetryDeclineReason::Budget)
            );
        }
        commands.send(None).await?;
    }
    let error = timeout(Duration::from_secs(5), operation)
        .await??
        .err()
        .ok_or("second stall must end the attempt")?;
    assert_eq!(error.facts().physical_attempts(), 2);
    assert_eq!(peer.await??, 2);
    Ok(())
}

/// The caller's hard deadline declines the replay and keeps output that
/// already streamed. The deadline is wide enough for a loaded host to finish
/// TLS and deliver the text delta; the first response then withholds its last
/// bytes past that deadline, so the decline still happens with output in
/// hand. Two hundred milliseconds is only a liveness bound and has elapsed
/// during the handshake on a loaded lane, before any output existed to keep.
#[tokio::test]
async fn a_caller_hard_deadline_declines_replay_without_discarding_output() -> TestResult {
    const HARD_DEADLINE: Duration = Duration::from_secs(3);
    let (config, identity, origin, peer) = responding_wire_peer_sequence_holding_first(
        vec![http_response(
            "200 OK",
            Some("text/event-stream"),
            &text_stream_cut_before_terminal(),
        )],
        HARD_DEADLINE + Duration::from_millis(500),
    )
    .await?;
    let client = Client::with_tls_config_and_identity(
        Limits {
            response_body_timeout: Duration::from_secs(180),
            ..Limits::default()
        },
        config,
        identity,
    )?;
    let dialect = wire_dialect(origin, openai_generation_fingerprint(b"hard-deadline"));
    let authorization = ResponsesAuthorizationAdapter::new(1, 1, wire_preflight, wire_revalidate)?;
    let data = attempt_data(&client, &authorization)?;
    let (send, mut observations) = mpsc::channel(64);
    let attempt = executable_attempt(&data, CancellationToken::new())?
        .with_dispatch_deadlines(Duration::from_secs(5), Some(Instant::now() + HARD_DEADLINE))
        .with_observations(send);
    let service = ResponsesAttemptService::new(client, dialect, data, authorization)?;
    let error = timeout(Duration::from_secs(30), service.execute(attempt))
        .await
        .map_err(|_| "hard deadline must prevent a replay")?
        .err()
        .ok_or("hard deadline must prevent a replay")?;
    assert_eq!(error.facts().physical_attempts(), 1);
    let seen = drain_observations(&mut observations);
    let retry = seen
        .iter()
        .find_map(|event| match event {
            ResponsesObservation::Retry(retry) => Some(retry),
            _ => None,
        })
        .ok_or("declined retry was not observed")?;
    assert_eq!(
        retry.loss().declined_reason,
        Some(plakous_model::ModelRetryDeclineReason::Deadline)
    );
    assert!(!retry.output_discarded());
    assert_eq!(peer.await??.len(), 1);
    Ok(())
}

#[test]
fn a_reasoning_item_suspends_the_stall_bound_until_visible_output_resumes() {
    let start = Instant::now();
    let mut progress = StreamProgress::default();
    let visible = ResponsesStreamEventKind::CustomToolCallInputDelta {
        output_index: 0,
        item_id: None,
        delta: "input".to_owned(),
    };
    progress.observe_semantic_event(&visible, start);
    assert_eq!(
        progress.semantic_deadline(),
        Some(start + Duration::from_secs(300))
    );
    let reasoning = ResponsesStreamEventKind::OutputItemAdded {
        output_index: 1,
        item: ResponsesOutputItem::Reasoning {
            id: Some("reasoning-1".to_owned()),
            status: None,
            summary: Vec::new(),
            content: Vec::new(),
            encrypted_content: None,
            native: serde_json::json!({"type":"reasoning"}),
        },
    };
    progress.observe_semantic_event(&reasoning, start + Duration::from_secs(250));
    assert!(progress.activity.output_started);
    assert_eq!(progress.semantic_deadline(), None);
    for event in [
        ResponsesStreamEventKind::ReasoningSummaryPartAdded {
            output_index: 1,
            summary_index: 0,
            part: serde_json::json!({"type":"summary_text","text":""}),
        },
        ResponsesStreamEventKind::ReasoningSummaryTextDelta {
            output_index: 1,
            summary_index: 0,
            delta: "summary".to_owned(),
        },
        ResponsesStreamEventKind::ReasoningTextDelta {
            output_index: 1,
            content_index: 0,
            item_id: None,
            delta: "reasoning".to_owned(),
        },
    ] {
        progress.observe_semantic_event(&event, start + Duration::from_secs(700));
        assert_eq!(progress.semantic_deadline(), None);
    }
    progress.observe_semantic_event(&visible, start + Duration::from_secs(900));
    assert_eq!(
        progress.semantic_deadline(),
        Some(start + Duration::from_secs(1200))
    );
}

#[test]
fn stream_activity_is_coalesced_and_flushed_at_close() {
    let start = Instant::now();
    let mut empty = StreamProgress::default();
    assert_eq!(empty.activity_to_emit(start, true), Some(empty.activity));
    let mut progress = StreamProgress::default();
    assert_eq!(progress.activity_to_emit(start, false), None);
    progress.activity.output_started = true;
    assert!(progress.activity_to_emit(start, false).is_some());
    progress.activity.keepalive_count = 1;
    assert_eq!(
        progress.activity_to_emit(start + Duration::from_millis(999), false),
        None
    );
    assert!(
        progress
            .activity_to_emit(start + Duration::from_secs(1), false)
            .is_some()
    );
    progress.activity.semantic_gap_ms = 1200;
    assert_eq!(
        progress.activity_to_emit(start + Duration::from_secs(2), false),
        None
    );
    progress.activity.keepalive_count = 2;
    assert_eq!(
        progress.activity_to_emit(start + Duration::from_millis(1500), false),
        None
    );
    assert!(matches!(
        progress.activity_to_emit(start + Duration::from_millis(1500), true),
        Some(plakous_model::ModelStreamActivity {
            keepalive_count: 2,
            semantic_gap_ms: 1200,
            ..
        })
    ));
}

#[test]
fn output_started_bypasses_the_keepalive_activity_throttle() {
    let start = Instant::now();
    let mut progress = StreamProgress::default();
    progress.activity.keepalive_count = 1;
    assert!(progress.activity_to_emit(start, false).is_some());
    progress.observe_semantic_event(
        &ResponsesStreamEventKind::CustomToolCallInputDelta {
            output_index: 0,
            item_id: None,
            delta: "input".to_owned(),
        },
        start + Duration::from_millis(10),
    );
    assert!(matches!(
        progress.activity_to_emit(start + Duration::from_millis(10), false),
        Some(plakous_model::ModelStreamActivity {
            output_started: true,
            ..
        })
    ));
    progress.activity.keepalive_count = 2;
    assert_eq!(
        progress.activity_to_emit(start + Duration::from_millis(20), false),
        None
    );
}

#[tokio::test]
async fn an_expired_workflow_hard_deadline_declines_as_deadline() -> TestResult {
    let (config, identity, origin, commands, peer) =
        controlled_stream_peer(vec![text_stream_cut_before_terminal()], false).await?;
    let client = Client::with_tls_config_and_identity(Limits::default(), config, identity)?;
    let dialect = wire_dialect(
        origin,
        openai_generation_fingerprint(b"workflow-hard-deadline"),
    );
    let authorization = ResponsesAuthorizationAdapter::new(1, 1, wire_preflight, wire_revalidate)?;
    let data = attempt_data(&client, &authorization)?;
    let (send, mut observations) = mpsc::channel(64);
    let mut attempt = executable_attempt(&data, CancellationToken::new())?
        .with_dispatch_deadlines(
            Duration::from_secs(5),
            Some(monotonic_now() + Duration::from_secs(2)),
        )
        .with_observations(send);
    attempt.retry_permitted = false;
    let service = ResponsesAttemptService::new(client, dialect, data, authorization)?;
    let operation = tokio::spawn(async move { service.execute(attempt).await });
    observe_output(&mut observations).await?;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(3)).await;
    tokio::time::resume();
    let retry = loop {
        let event = timeout(Duration::from_secs(5), observations.recv())
            .await?
            .ok_or("decline channel closed")?;
        if let ResponsesObservation::Retry(retry) = event {
            break retry;
        }
    };
    assert_eq!(retry.loss().declined_reason, Some(Decline::Deadline));
    assert_eq!(retry.next_physical(), 2);
    assert_eq!(retry.delay(), Duration::ZERO);
    assert!(!retry.output_discarded());
    commands.send(None).await?;
    let error = timeout(Duration::from_secs(5), operation)
        .await??
        .err()
        .ok_or("hard deadline must end workflow")?;
    assert_eq!(error.facts().physical_attempts(), 1);
    assert_eq!(peer.await??, 1);
    Ok(())
}
