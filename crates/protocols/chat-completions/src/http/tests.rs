use super::*;

use std::error::Error;
use std::io;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use plakous_attempt::LedgerPolicy;
use plakous_http::{Bytes, HeaderMap, Limits, Method, Request, StatusCode, Version};
use plakous_model::{
    AttemptId, CancellationToken, Digest, DigestDomain, EffectiveProviderConfigurationId,
    FixedClock, ModelCallContext, ModelId, ProtocolId, ReasoningPolicy,
    SelectedProviderConfigurationId, SequenceIdGenerator, Timestamp, Usage,
};
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use uuid::Uuid;

use crate::{
    CHAT_FAMILY_LEDGER_POLICY, ChatAttemptEventReceiver, ChatReasoningReplay, ChatReplayTargetV1,
    ChatStreamLimits, ReasoningAliasPolicy, call_chat_operation,
};

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Clone, Default)]
struct FakeHooks {
    physical: Arc<Mutex<Option<ChatPhysicalRequestBinding>>>,
}

impl ChatHttpHooks for FakeHooks {
    type ErrorMetadata = ();

    fn build_request(
        &self,
        client: &Client,
        dial_origin: Origin,
        physical: &ChatPhysicalRequestBinding,
        operation: &PreparedChatHttpOperation,
    ) -> Result<HttpRequest, ChatAttemptProtocolError> {
        *self.physical.lock().expect("physical capture") = Some(physical.clone());
        Request::new(
            dial_origin,
            Method::POST,
            "/chat",
            HeaderMap::new(),
            Bytes::copy_from_slice(operation.body()),
            client.limits(),
        )
        .map_err(|_| ChatAttemptProtocolError::new("fake request construction failed"))
    }

    fn classify_response_head(
        &self,
        response: &Response,
    ) -> Result<ChatHttpResponseHead<Self::ErrorMetadata>, ChatAttemptProtocolError> {
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            return Ok(ChatHttpResponseHead::Error(ChatHttpErrorHead {
                metadata: (),
                outcome: PhysicalRequestOutcome::DefiniteRejection,
                provider_accepted: false,
            }));
        }
        if response.status() != StatusCode::OK || response.version() != Version::HTTP_11 {
            return Err(ChatAttemptProtocolError::new("fake response head mismatch"));
        }
        Ok(ChatHttpResponseHead::Stream(Some(
            ModelProviderRequestId::new("head-request-exact")
                .map_err(|_| ChatAttemptProtocolError::new("fake request id failed"))?,
        )))
    }

    fn classify_error_response(
        &self,
        (): (),
        _body: &[u8],
        _safety: ChatHttpSafety,
    ) -> ChatAttemptProtocolError {
        ChatAttemptProtocolError::new("fake status refusal")
    }

    fn classify_transport_failure(
        &self,
        failure: HttpFailure,
        _request_id: Option<&str>,
    ) -> ChatAttemptProtocolError {
        ChatAttemptProtocolError::new(format!("fake transport failure: {:?}", failure.kind()))
    }

    fn cancellation_error(&self) -> ChatAttemptProtocolError {
        ChatAttemptProtocolError::new("fake cancellation")
    }

    fn classify_stream_error(&self, error: ChatStreamError) -> ChatAttemptProtocolError {
        let protocol = ChatAttemptProtocolError::new(error.message());
        let Some(status) = error.http_status() else {
            return protocol;
        };
        let protocol = protocol.with_http_status(status);
        match plakous_attempt::ProviderRefusal::from_http_status(status) {
            Some(refusal) => protocol.with_refusal(refusal),
            None => protocol,
        }
    }
}

fn digest(domain: DigestDomain, label: &[u8]) -> TestResult<Digest> {
    Ok(Digest::sha256(domain, 1, &[label])?)
}

fn configuration_id<T>(domain: DigestDomain, label: &[u8]) -> TestResult<T>
where
    T: serde::de::DeserializeOwned,
{
    let digest = Digest::sha256(domain, 5, &[label])?;
    Ok(serde_json::from_value(serde_json::to_value(digest)?)?)
}

fn fixture_operation(label: &[u8]) -> TestResult<PreparedChatHttpOperation> {
    fixture_operation_with_body(label, b"{}")
}

fn fixture_operation_with_body(label: &[u8], body: &[u8]) -> TestResult<PreparedChatHttpOperation> {
    let plan = digest(DigestDomain::RequestPlan, label)?;
    let protocol = ProtocolId::new("chat-completions.test")?;
    let model = ModelId::new("model-literal")?;
    let selected: SelectedProviderConfigurationId =
        configuration_id(DigestDomain::SelectedProviderConfiguration, b"selected")?;
    let effective: EffectiveProviderConfigurationId =
        configuration_id(DigestDomain::EffectiveProviderConfiguration, b"effective")?;
    let replay = ChatReplayTargetV1::new(
        protocol.clone(),
        model.clone(),
        digest(DigestDomain::ModelBinding, b"account")?,
        selected,
        effective,
    );
    let stream = PreparedChatStreamBinding::new(
        plan,
        protocol,
        model,
        replay,
        ReasoningPolicy::Disabled,
        ChatReasoningReplay::VisibleOnly,
        ReasoningAliasPolicy::Reasoning,
        ChatStreamLimits::default(),
    );
    let lowered_digest = digest(DigestDomain::ProviderOperation, label)?;
    let lowered = LoweredChatRequest::test_fixture(body.to_vec(), lowered_digest);
    let binding = ChatOperationBinding::new(
        digest(DigestDomain::ModelBinding, label)?,
        plan,
        lowered_digest,
        lowered.replay_safety(),
    );
    Ok(PreparedChatHttpOperation::new(binding, lowered, stream))
}

fn fixture_attempt(
    operation: ChatOperationBinding,
) -> TestResult<(ChatAttempt, ChatAttemptEventReceiver)> {
    let context = ModelCallContext::new(
        AttemptId::try_from(Uuid::from_u128(100))?,
        CancellationToken::new(),
        None,
    );
    Ok(ChatAttempt::with_event_channel(
        operation,
        context,
        NonZeroUsize::new(32).expect("nonzero"),
    ))
}

fn clock() -> Arc<dyn Clock> {
    Arc::new(FixedClock::new(
        Timestamp::new(1_700_000_000, 0).expect("timestamp"),
    ))
}

fn ids() -> Arc<dyn IdGenerator> {
    Arc::new(SequenceIdGenerator::new((1_u128..=8).map(Uuid::from_u128)).expect("sequence ids"))
}

async fn loopback(response_body: Vec<u8>) -> TestResult<(Client, Origin)> {
    let (client, origin, _) = loopback_responses(vec![(200, response_body, false)]).await?;
    Ok((client, origin))
}

async fn loopback_responses(
    responses: Vec<(u16, Vec<u8>, bool)>,
) -> TestResult<(Client, Origin, Arc<AtomicUsize>)> {
    loopback_responses_with_hold(responses, None).await
}

async fn loopback_responses_with_hold(
    responses: Vec<(u16, Vec<u8>, bool)>,
    mut release: Option<tokio::sync::mpsc::Receiver<()>>,
) -> TestResult<(Client, Origin, Arc<AtomicUsize>)> {
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
    let origin = Origin::parse(&format!("https://{}", listener.local_addr()?))?;
    let acceptor = TlsAcceptor::from(Arc::new(server_config));
    let body_timeout = if release.is_some() {
        Duration::from_secs(600)
    } else {
        Limits::default().response_body_timeout
    };
    let dispatches = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&dispatches);
    tokio::spawn(async move {
        for (status, response_body, incomplete) in responses {
            let (stream, _) = listener.accept().await?;
            let mut stream = acceptor.accept(stream).await?;
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            loop {
                let read = stream.read(&mut buffer).await?;
                if read == 0 {
                    return Err::<(), io::Error>(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "request ended before its body",
                    ));
                }
                request.extend_from_slice(&buffer[..read]);
                if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&request[..end]);
                    let length = head
                        .lines()
                        .filter_map(|line| line.split_once(':'))
                        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                        .ok_or_else(|| io::Error::other("fixture request content-length"))?;
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            seen.fetch_add(1, Ordering::SeqCst);
            let head = format!(
                "HTTP/1.1 {status} Fixture\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n",
                response_body.len() + usize::from(incomplete)
            );
            stream.write_all(head.as_bytes()).await?;
            stream.write_all(&response_body).await?;
            if let Some(release) = &mut release {
                let _ = release.recv().await;
            }
            stream.shutdown().await?;
        }
        Ok(())
    });
    Ok((
        Client::with_tls_config(
            Limits {
                response_body_timeout: body_timeout,
                ..Limits::default()
            },
            client_config,
        )?,
        origin,
        dispatches,
    ))
}

fn successful_sse() -> Vec<u8> {
    let prefix = |choices: &str| {
        format!(
            "data: {{\"id\":\"response-exact\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"model-literal\",\"choices\":{choices}}}\n\n"
        )
    };
    [
        prefix(r#"[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]"#),
        prefix(r#"[{"index":0,"delta":{"content":"exact answer"},"finish_reason":null}]"#),
        prefix(r#"[{"index":0,"delta":{},"finish_reason":"stop"}]"#),
        String::from(
            "data: {\"id\":\"response-exact\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"model-literal\",\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":4,\"total_tokens\":11,\"prompt_tokens_details\":{\"cached_tokens\":3},\"completion_tokens_details\":{\"reasoning_tokens\":2}}}\n\n",
        ),
        String::from("data: [DONE]\n\n"),
    ]
    .concat()
    .into_bytes()
}

fn deadline_sse() -> Vec<u8> {
    b"data: {\"error\":{\"code\":504,\"message\":\"upstream timeout\"}}\n\n".to_vec()
}

#[tokio::test]
async fn a_second_in_band_deadline_stops_before_a_third_scripted_reply() -> TestResult {
    let operation = fixture_operation(b"deadline-cap")?;
    let (attempt, _receiver) = fixture_attempt(operation.binding().clone())?;
    let (client, origin, dispatches) = loopback_responses(vec![
        (200, deadline_sse(), false),
        (200, deadline_sse(), false),
        (200, successful_sse(), false),
    ])
    .await?;
    let mut service = ChatHttpAttemptService::new(
        client,
        origin,
        operation,
        FakeHooks::default(),
        clock(),
        ids(),
        Duration::from_secs(30),
        1024,
    )
    .with_turn_retry();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        call_chat_operation(&mut service, attempt),
    )
    .await?;
    assert_eq!(
        dispatches.load(Ordering::SeqCst),
        2,
        "a missing cap dispatched the third scripted reply"
    );
    let error = result.expect_err("the second provider deadline is terminal");
    assert_eq!(error.facts().ok_or("facts")?.physical_attempts(), 2);
    assert_eq!(
        error.facts().ok_or("facts")?.summary().decline,
        Some(plakous_attempt::AttemptRetryDecline::Budget)
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_transport_idle_stall_does_not_spend_the_provider_deadline_replay() -> TestResult {
    let (_keep_clock, clock_held) = std::sync::mpsc::channel::<()>();
    tokio::task::spawn_blocking(move || {
        let _ = clock_held.recv();
    });
    let operation = fixture_operation(b"idle-then-deadline")?;
    let (attempt, mut receiver) = fixture_attempt(operation.binding().clone())?;
    let complete = String::from_utf8(successful_sse())?;
    let boundary = complete
        .match_indices("\n\n")
        .nth(1)
        .ok_or("prefix boundary")?
        .0
        + 2;
    let (release, holds) = tokio::sync::mpsc::channel(1);
    let mut release = Some(release);
    let (client, origin, dispatches) = loopback_responses_with_hold(
        vec![
            (200, complete.as_bytes()[..boundary].to_vec(), true),
            (200, deadline_sse(), false),
            (200, successful_sse(), false),
        ],
        Some(holds),
    )
    .await?;
    let client = client.with_limits(Limits {
        response_body_timeout: Duration::from_secs(1),
        ..client.limits().clone()
    })?;
    let mut service = ChatHttpAttemptService::new(
        client,
        origin,
        operation,
        FakeHooks::default(),
        clock(),
        ids(),
        Duration::from_secs(30),
        1024,
    )
    .with_turn_retry();
    let execution = tokio::spawn(async move { call_chat_operation(&mut service, attempt).await });
    let mut stalled = false;
    let mut retries = 0;
    while let Some(event) = receiver.next().await {
        match event? {
            ModelEvent::TextDelta(_) if !stalled => {
                stalled = true;
                tokio::time::advance(Duration::from_secs(2)).await;
            }
            ModelEvent::Retry(retry) => {
                assert_eq!(retry.loss().declined_reason, None, "{retry:?}");
                assert_eq!(
                    retry.cause(),
                    ModelRetryCause::StreamInterrupted {
                        fault: ModelTransportFault::Timeout
                    }
                );
                retries += 1;
                drop(release.take());
                tokio::time::advance(retry.delay() + Duration::from_millis(2)).await;
            }
            _ => {}
        }
    }
    let response = execution.await??;
    assert_eq!(response.facts().physical_attempts(), 3);
    assert_eq!(dispatches.load(Ordering::SeqCst), 3);
    assert_eq!(retries, 2);
    Ok(())
}

#[tokio::test]
async fn a_server_effect_chat_body_never_replays_an_in_band_deadline() -> TestResult {
    let operation = fixture_operation_with_body(
        b"effect-deadline",
        br#"{"plugins":[{"id":"web","enabled":true}]}"#,
    )?;
    assert_eq!(
        operation.binding().replay_safety(),
        plakous_attempt::ReplaySafety::ServerEffect
    );
    let (attempt, mut receiver) = fixture_attempt(operation.binding().clone())?;
    let (client, origin, dispatches) = loopback_responses(vec![
        (200, deadline_sse(), false),
        (200, successful_sse(), false),
    ])
    .await?;
    let mut service = ChatHttpAttemptService::new(
        client,
        origin,
        operation,
        FakeHooks::default(),
        clock(),
        ids(),
        Duration::from_secs(30),
        1024,
    )
    .with_turn_retry();
    let error = call_chat_operation(&mut service, attempt)
        .await
        .expect_err("server effect forbids replay");
    assert_eq!(error.facts().ok_or("facts")?.physical_attempts(), 1);
    assert_eq!(dispatches.load(Ordering::SeqCst), 1);
    let mut declined = 0;
    while let Some(event) = receiver.next().await {
        if let Ok(ModelEvent::Retry(retry)) = event {
            assert_eq!(retry.loss().declined_reason, Some(Decline::ServerEffect));
            assert!(!retry.output_discarded());
            declined += 1;
        }
    }
    assert_eq!(declined, 1);
    Ok(())
}

#[derive(Clone, Copy)]
enum SinkFailure {
    Never,
    Chunk,
    Terminal,
}

struct CaptureObserver {
    required: bool,
    failure: SinkFailure,
}

impl plakous_http::ExchangeObserver for CaptureObserver {
    fn required(&self) -> bool {
        self.required
    }

    fn start(&self, _: &[u8]) -> Option<Box<dyn plakous_http::ExchangeObservation>> {
        Some(Box::new(CaptureSink {
            failure: self.failure,
            failed: false,
        }))
    }
}

struct CaptureSink {
    failure: SinkFailure,
    failed: bool,
}

impl plakous_http::ExchangeObservation for CaptureSink {
    fn failed(&self) -> bool {
        self.failed
    }

    fn response_head(&mut self, _: u16) {}

    fn response_chunk(&mut self, _: &[u8]) {
        self.failed |= matches!(self.failure, SinkFailure::Chunk);
    }

    fn terminal(&mut self, _: &plakous_http::ExchangeTerminal) {
        self.failed |= matches!(self.failure, SinkFailure::Terminal);
    }
}

#[tokio::test]
async fn a_long_chat_stream_failure_allows_only_one_replay() -> TestResult {
    let operation = fixture_operation(b"long-chat-cap")?;
    let (attempt, mut receiver) = fixture_attempt(operation.binding().clone())?;
    let complete = String::from_utf8(successful_sse())?;
    let boundary = complete
        .match_indices("\n\n")
        .nth(1)
        .ok_or("SSE boundary")?
        .0
        + 2;
    let prefix = complete.as_bytes()[..boundary].to_vec();
    let (release, holds) = tokio::sync::mpsc::channel(1);
    let (client, origin, dispatches) = loopback_responses_with_hold(
        vec![(200, prefix.clone(), true), (200, prefix, true)],
        Some(holds),
    )
    .await?;
    let mut service = ChatHttpAttemptService::new(
        client,
        origin,
        operation,
        FakeHooks::default(),
        clock(),
        ids(),
        Duration::from_secs(5),
        1024,
    )
    .with_turn_retry();
    let execution = tokio::spawn(async move { call_chat_operation(&mut service, attempt).await });
    for physical in 1..=2 {
        loop {
            let event = tokio::time::timeout(Duration::from_secs(5), receiver.next())
                .await?
                .ok_or("output channel closed")??;
            if matches!(event, ModelEvent::TextDelta(_)) {
                break;
            }
        }
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(if physical == 1 { 301 } else { 1 })).await;
        tokio::time::resume();
        release.send(()).await?;
        let retry = loop {
            let event = tokio::time::timeout(Duration::from_secs(5), receiver.next())
                .await?
                .ok_or("retry channel closed")??;
            if let ModelEvent::Retry(retry) = event {
                break retry;
            }
        };
        assert_eq!(retry.next_physical(), physical + 1);
        if physical == 1 {
            assert!(retry.loss().elapsed_ms > 300_000);
            assert!(retry.output_discarded());
            assert_eq!(retry.loss().declined_reason, None);
        } else {
            assert!(retry.loss().elapsed_ms < 300_000);
            assert!(!retry.output_discarded());
            assert_eq!(retry.delay(), Duration::ZERO);
            assert_eq!(retry.loss().declined_reason, Some(Decline::Budget));
        }
    }
    let error = tokio::time::timeout(Duration::from_secs(5), execution)
        .await??
        .expect_err("second long failure must be terminal");
    assert_eq!(
        error
            .facts()
            .ok_or("admitted failure facts")?
            .physical_attempts(),
        2
    );
    assert_eq!(dispatches.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test]
async fn required_capture_failure_on_429_ends_without_retry() -> TestResult {
    // Exercise chunk failure, clean and truncated EOF terminal failure, and
    // early-close failure. The healthy response exposes any retry.
    for (failure, limit, incomplete) in [
        (SinkFailure::Chunk, 1024, false),
        (SinkFailure::Terminal, 1024, false),
        (SinkFailure::Terminal, 1024, true),
        (SinkFailure::Terminal, 1, false),
    ] {
        let operation = fixture_operation(b"capture-refusal")?;
        let (attempt, mut receiver) = fixture_attempt(operation.binding().clone())?;
        let (client, origin, dispatches) = loopback_responses(vec![
            (429, b"optional error body".to_vec(), incomplete),
            (200, successful_sse(), false),
        ])
        .await?;
        let client = client.with_observer(Some(Arc::new(CaptureObserver {
            required: true,
            failure,
        })));
        let mut service = ChatHttpAttemptService::new(
            client,
            origin,
            operation,
            FakeHooks::default(),
            clock(),
            ids(),
            Duration::from_secs(5),
            limit,
        )
        .with_turn_retry();
        let error = call_chat_operation(&mut service, attempt)
            .await
            .expect_err("required capture must abort a refused attempt");
        let ChatAttemptError::Protocol { source, facts } = error else {
            panic!("capture failure must use the terminal protocol path");
        };
        assert_eq!(source.message(), "fake transport failure: Internal");
        assert_eq!(source.exchange_break(), ExchangeBreak::Transport);
        assert_eq!(facts.physical_attempts(), 1);
        assert_eq!(dispatches.load(Ordering::SeqCst), 1);
        let observed = receiver
            .next()
            .await
            .expect("one terminal error, not a retry notice")
            .expect_err("capture error observation");
        assert_eq!(observed.message(), source.message());
        assert!(receiver.next().await.is_none());
    }
    Ok(())
}

#[tokio::test]
async fn optional_429_body_faults_and_passive_capture_still_allow_retry() -> TestResult {
    for (required, failure, limit, incomplete) in [
        (true, SinkFailure::Never, 1024, false),
        (false, SinkFailure::Chunk, 1024, false),
        (false, SinkFailure::Terminal, 1, false),
        (true, SinkFailure::Never, 1, false),
        (true, SinkFailure::Never, 1024, true),
    ] {
        let operation = fixture_operation(b"capture-control")?;
        let (attempt, mut receiver) = fixture_attempt(operation.binding().clone())?;
        let (client, origin, dispatches) = loopback_responses(vec![
            (429, b"{malformed optional json".to_vec(), incomplete),
            (200, successful_sse(), false),
        ])
        .await?;
        let client = client.with_observer(Some(Arc::new(CaptureObserver { required, failure })));
        let mut service = ChatHttpAttemptService::new(
            client,
            origin,
            operation,
            FakeHooks::default(),
            clock(),
            ids(),
            Duration::from_secs(5),
            limit,
        )
        .with_turn_retry();
        let response = call_chat_operation(&mut service, attempt).await?;
        assert_eq!(response.facts().physical_attempts(), 2);
        assert_eq!(dispatches.load(Ordering::SeqCst), 2);
        let mut notices = 0;
        while let Some(event) = receiver.next().await {
            if matches!(event?, ModelEvent::Retry(_)) {
                notices += 1;
            }
        }
        assert_eq!(notices, 1);
    }
    Ok(())
}

#[tokio::test]
async fn record_rejected_resends_immediately_on_a_fresh_connection() -> TestResult {
    let operation = fixture_operation(b"record-rejected")?;
    let (attempt, mut receiver) = fixture_attempt(operation.binding().clone())?;
    let (client, origin, dispatches) =
        loopback_responses(vec![(200, successful_sse(), false)]).await?;
    let client = client
        .pooled()
        .with_faults(Some(plakous_http::FaultPlan::parse(
            "fault=record-rejected,nth=1",
        )?));
    let mut service = ChatHttpAttemptService::new(
        client,
        origin,
        operation,
        FakeHooks::default(),
        clock(),
        ids(),
        Duration::from_secs(5),
        1024,
    )
    .with_turn_retry();
    let response = call_chat_operation(&mut service, attempt).await?;
    assert_eq!(response.facts().physical_attempts(), 2);
    assert_eq!(dispatches.load(Ordering::SeqCst), 1);
    let mut retries = Vec::new();
    while let Some(event) = receiver.next().await {
        if let ModelEvent::Retry(retry) = event? {
            retries.push(retry);
        }
    }
    assert_eq!(retries.len(), 1);
    assert_eq!(retries[0].cause().kind(), "record_rejected_in_transit");
    assert_eq!(
        retries[0].cause().to_string(),
        "request corrupted in transit (TLS record rejected)"
    );
    assert_eq!(retries[0].delay(), Duration::ZERO);
    assert_eq!(retries[0].request(), Some(("127.0.0.1", "/chat", 2)));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn chat_unreachable_ride_out_excludes_incomplete_requests() -> TestResult {
    for (fault, expected) in [("connect", 12), ("write-reset", 4), ("record-rejected", 4)] {
        let operation = fixture_operation(b"ride-out")?;
        let (attempt, _receiver) = fixture_attempt(operation.binding().clone())?;
        let client = Client::new(Limits::default())?.with_faults(Some(
            plakous_http::FaultPlan::parse(&format!("fault={fault}"))?,
        ));
        let mut service = ChatHttpAttemptService::new(
            client,
            Origin::parse("https://127.0.0.1:9")?,
            operation,
            FakeHooks::default(),
            clock(),
            ids(),
            Duration::from_secs(5),
            1024,
        )
        .with_turn_retry();
        let started = Instant::now();
        let error = call_chat_operation(&mut service, attempt)
            .await
            .expect_err("injected outage");
        assert_eq!(
            error.facts().ok_or("facts")?.physical_attempts(),
            expected,
            "{fault}"
        );
        assert_eq!(
            error.facts().ok_or("facts")?.summary().ride_out_physical,
            if fault == "connect" { expected } else { 0 }
        );
        assert!(started.elapsed() <= Duration::from_secs(240));
    }
    Ok(())
}

#[tokio::test]
async fn loopback_success_seals_exact_head_metadata_before_one_terminal() -> TestResult {
    assert_eq!(
        CHAT_FAMILY_LEDGER_POLICY,
        LedgerPolicy {
            require_service_admitted_for_dispatch: true,
            require_active_request_always: false,
            dual_auth_slots: false,
            clear_service_on_terminal_success: true,
        }
    );
    let operation = fixture_operation(b"success")?;
    let (attempt, mut receiver) = fixture_attempt(operation.binding().clone())?;
    let expected_physical = ChatPhysicalRequestBinding::new(attempt.binding().clone(), 1);
    let hooks = FakeHooks::default();
    let captured = Arc::clone(&hooks.physical);
    let (client, origin) = loopback(successful_sse()).await?;
    let mut service = ChatHttpAttemptService::new(
        client,
        origin,
        operation,
        hooks,
        clock(),
        ids(),
        Duration::from_secs(5),
        1024,
    );
    let drain = tokio::spawn(async move {
        let mut events = Vec::new();
        while let Some(event) = receiver.next().await {
            events.push(event);
        }
        events
    });
    let response = call_chat_operation(&mut service, attempt).await?;
    let events = drain.await?;

    assert_eq!(
        captured.lock().expect("captured").as_ref(),
        Some(&expected_physical)
    );
    assert_eq!(response.request_id().as_str(), "head-request-exact");
    assert_eq!(
        response.response().provider_request_id(),
        Some("head-request-exact")
    );
    assert_eq!(
        response.response().actual_model().map(ModelId::as_str),
        Some("model-literal")
    );
    assert_eq!(response.response().usage(), Some(Usage::new(7, 4, 2, 3, 0)));
    assert_eq!(response.finish().wire(), "stop");
    assert!(response.facts().provider_accepted());
    assert_eq!(
        response.facts().retry_class(),
        plakous_attempt::RetryClass::Never
    );
    assert!(events.iter().all(Result::is_ok));
    let started = events
        .iter()
        .find_map(|event| match event {
            Ok(ModelEvent::ResponseStarted(started)) => Some(started),
            _ => None,
        })
        .expect("response started");
    assert_eq!(started.provider_request_id(), Some("head-request-exact"));
    assert_eq!(
        started.actual_model().map(ModelId::as_str),
        Some("model-literal")
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Ok(ModelEvent::ResponseCompleted(_))))
            .count(),
        1
    );
    let terminal = events.last().expect("terminal").as_ref().expect("success");
    let ModelEvent::ResponseCompleted(terminal) = terminal else {
        panic!("last event must be terminal");
    };
    assert_eq!(terminal, response.response());
    assert_eq!(terminal.provider_request_id(), Some("head-request-exact"));
    Ok(())
}

#[tokio::test]
async fn rejects_foreign_operation_before_building_a_physical_request() -> TestResult {
    let operation = fixture_operation(b"service")?;
    let foreign = fixture_operation(b"foreign")?;
    let (attempt, _receiver) = fixture_attempt(foreign.binding().clone())?;
    let hooks = FakeHooks::default();
    let captured = Arc::clone(&hooks.physical);
    let (client, origin) = loopback(successful_sse()).await?;
    let mut service = ChatHttpAttemptService::new(
        client,
        origin,
        operation,
        hooks,
        clock(),
        ids(),
        Duration::from_secs(5),
        1024,
    );
    let error = call_chat_operation(&mut service, attempt)
        .await
        .expect_err("foreign binding");
    assert!(matches!(error, ChatAttemptError::Binding { .. }));
    assert!(captured.lock().expect("captured").is_none());
    assert_eq!(
        error.facts().expect("admitted facts").physical_attempts(),
        0
    );
    Ok(())
}

#[tokio::test]
async fn accepted_malformed_stream_error_retains_outcome_unknown_facts() -> TestResult {
    let operation = fixture_operation(b"malformed")?;
    let (attempt, mut receiver) = fixture_attempt(operation.binding().clone())?;
    let (client, origin) = loopback(b"data: {bad}\n\n".to_vec()).await?;
    let mut service = ChatHttpAttemptService::new(
        client,
        origin,
        operation,
        FakeHooks::default(),
        clock(),
        ids(),
        Duration::from_secs(5),
        1024,
    );

    let operation_error = call_chat_operation(&mut service, attempt)
        .await
        .expect_err("malformed accepted stream");
    assert_eq!(
        operation_error.provider_acceptance(),
        Some(plakous_attempt::ProviderAcceptance::Accepted)
    );
    let observation_error = receiver
        .next()
        .await
        .expect("terminal observation")
        .expect_err("protocol error observation");
    assert_eq!(
        observation_error.provider_acceptance(),
        Some(plakous_attempt::ProviderAcceptance::Accepted)
    );
    assert!(receiver.next().await.is_none());
    Ok(())
}
