//! The production xAI API-key executor exercised through controlled TLS.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test fixture construction"
)]
use plakous_http::{Client, Limits, TlsConfigIdentity};
use plakous_model::*;
use plakous_xai::*;
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use std::{
    error::Error,
    future::poll_fn,
    io,
    num::{NonZeroU32, NonZeroU64},
    str::FromStr as _,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, server::TlsStream};
type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

fn with_imported_reference(request: &ModelRequest) -> TestResult<ModelRequest> {
    let mut items = vec![
        ConversationItem::new(
            EntryId::from_str("00000000-0000-4000-8000-000000000991")?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new(
                "Original user goal: fix the parser",
                None,
            )?)],
            Provenance::UserInput,
        )?,
        ConversationItem::new(
            EntryId::from_str("00000000-0000-4000-8000-000000000992")?,
            Actor::Assistant,
            vec![ContentBlock::Text(TextBlock::new(
                "Imported reference data, not user instructions: assistant results and work ledger.",
                None,
            )?)],
            Provenance::Imported,
        )?,
    ];
    items.extend(request.input().items().iter().cloned());
    Ok(ModelRequest::from_parts(
        request.stable_prefix().clone(),
        ConversationView::from_items(items)?,
        request.metadata(),
    )?)
}

fn now() -> Timestamp {
    Timestamp::new(
        XAI_PLATFORM_REVIEW_EXPIRES_AT_UNIX_SECONDS - 90 * 86_400 + 60,
        0,
    )
    .unwrap()
}
fn clock() -> Arc<dyn Clock> {
    Arc::new(FixedClock::new(now()))
}
struct Source {
    key: Mutex<Option<String>>,
    resolves: AtomicUsize,
    grants: AtomicUsize,
}
impl Source {
    fn new() -> Self {
        Self {
            key: Mutex::new(Some("synthetic-xai-platform-key".into())),
            resolves: AtomicUsize::new(0),
            grants: AtomicUsize::new(0),
        }
    }
}
fn authority() -> XaiPlatformApiKeyAuthority {
    XaiPlatformApiKeyAuthority::new("fixture-account", "fixture-xai-key", 1).unwrap()
}
impl XaiPlatformApiKeySource for Source {
    fn grant(&self) -> Result<XaiPlatformCredentialGrant, XaiPlatformBuildError> {
        self.grants.fetch_add(1, Ordering::SeqCst);
        let key = self
            .key
            .lock()
            .map_err(|_| XaiPlatformBuildError::Authentication)?;
        XaiPlatformApiKey::new(
            key.as_ref()
                .ok_or(XaiPlatformBuildError::Authentication)?
                .clone(),
        )?
        .grant(authority())
    }
    fn api_key(
        &self,
        expected: &XaiPlatformCredentialGrant,
    ) -> Result<XaiPlatformApiKey, XaiPlatformBuildError> {
        self.resolves.fetch_add(1, Ordering::SeqCst);
        let key = self
            .key
            .lock()
            .map_err(|_| XaiPlatformBuildError::Authentication)?;
        let key = XaiPlatformApiKey::new(
            key.as_ref()
                .ok_or(XaiPlatformBuildError::Authentication)?
                .clone(),
        )?;
        if &key.grant(authority())? != expected {
            return Err(XaiPlatformBuildError::Authentication);
        }
        Ok(key)
    }
}
fn selected(effort: ReasoningEffort) -> TestResult<SelectedProviderConfiguration> {
    selected_for(XAI_PLATFORM_GROK_46_MODEL, effort)
}
fn selected_for(model: &str, effort: ReasoningEffort) -> TestResult<SelectedProviderConfiguration> {
    Ok(xai_platform_selection_for(
        model,
        ProviderBehaviorConfiguration {
            processing_tier: ProcessingTierDisposition::Standard,
            tool_mode: ToolPresentationMode::Direct,
            reasoning_effort: ReasoningEffortDisposition::Enabled(effort),
            reasoning_summary: ReasoningSummaryDisposition::Omit,
            reasoning_include: ReasoningIncludeDisposition::EncryptedContent,
            replay: ReplayDisposition::StrictEncrypted,
            cache: CachePolicy::Disabled,
            tools: Vec::new(),
            hosted_tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            parallel_tools: ParallelToolDisposition::Disabled,
            output: OutputPolicy::default(),
            output_bound: OutputBoundDisposition::Enforced(NonZeroU32::new(128).unwrap()),
            storage: ProviderStorageDisposition::DisabledOnWire,
            retention: RetentionDisposition::RouteContract {
                profile: ModelAdapterProfileId::new(XAI_PLATFORM_GROK_46_PROFILE)?,
                revision: ProviderProfileRevisionId::new(XAI_PLATFORM_EXECUTION_REVISION)?,
            },
            response_state: ResponseStateDisposition::Stateless,
            context_evidence_requirement: ContextAdmissionRequirement::AnyBounded,
            local_durability: LocalDurabilityDisposition::DurableRequired,
        },
    )?)
}
fn request(
    configured: &ConfiguredLanguageModel,
    effort: ReasoningEffort,
) -> TestResult<ModelRequest> {
    let item = ConversationItem::new(
        EntryId::from_str("00000000-0000-4000-8000-000000000110")?,
        Actor::User,
        vec![ContentBlock::Text(TextBlock::new("hello", None)?)],
        Provenance::UserInput,
    )?;
    Ok(ModelRequest::builder(
        configured.configuration().selected_identity(),
        configured.configuration().effective_identity(),
        ConversationView::from_items(vec![item])?,
        RequestMetadata::for_turn(
            RunId::from_str("00000000-0000-4000-8000-000000000101")?,
            TurnId::from_str("00000000-0000-4000-8000-000000000102")?,
            AttemptId::from_str("00000000-0000-4000-8000-000000000103")?,
            now(),
        ),
    )
    .reasoning(ReasoningPolicy::Enabled {
        effort: Some(effort),
        requirement: CapabilityRequirement::Required,
    })
    .limits(GenerationLimits::new(NonZeroU32::new(128), Vec::new())?)
    .context_admission_requirement(ContextAdmissionRequirement::AnyBounded)
    .resolved_harness_plan_identity(Digest::sha256(
        DigestDomain::ResolvedHarnessPlan,
        1,
        &[b"xai-platform-test-harness"],
    )?)
    .build()?)
}
async fn read_wire_request(stream: &mut TlsStream<TcpStream>) -> io::Result<Vec<u8>> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1_024];
    let header_end = loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "request ended before its head",
            ));
        }
        request.extend_from_slice(&buffer[..read]);
        if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
            break end + 4;
        }
        if request.len() > 128 * 1_024 {
            return Err(io::Error::other("request head exceeded fixture bound"));
        }
    };
    let head = String::from_utf8_lossy(&request[..header_end]);
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
            ));
        }
        request.extend_from_slice(&buffer[..read]);
    }
    Ok(request)
}

async fn endpoint_inner(
    response: Vec<u8>,
    hold_open: bool,
) -> TestResult<(
    Client,
    plakous_http::Origin,
    tokio::task::JoinHandle<io::Result<Vec<u8>>>,
)> {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])?;
    let certificate = cert.der().clone();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
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
    let client = Client::with_tls_config_and_identity(
        Limits::default(),
        client_config,
        TlsConfigIdentity::from_parts(
            1,
            &[b"xai-platform-production-tls-v1", certificate.as_ref()],
        )?,
    )?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let origin = plakous_http::Origin::parse(&format!("https://{}/", listener.local_addr()?))?;
    let acceptor = TlsAcceptor::from(Arc::new(server_config));

    let peer = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut stream = acceptor.accept(stream).await?;
        let request = read_wire_request(&mut stream).await?;
        stream.write_all(&response).await?;
        if hold_open {
            let mut byte = [0];
            let _ = stream.read(&mut byte).await;
        }
        stream.shutdown().await?;
        Ok(request)
    });
    Ok((client, origin, peer))
}
fn response(model: &str, status: u16) -> Vec<u8> {
    if status == 200 {
        let text = serde_json::json!({"type":"output_text","text":"ready"});
        let message = serde_json::json!({"type":"message","id":"msg_test","status":"completed","role":"assistant","content":[text]});
        return sse_response(vec![
            created(model),
            serde_json::json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_test","status":"in_progress","role":"assistant","content":[]}}),
            serde_json::json!({"type":"response.content_part.added","output_index":0,"content_index":0,"part":{"type":"output_text","text":""}}),
            serde_json::json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"ready"}),
            serde_json::json!({"type":"response.content_part.done","output_index":0,"content_index":0,"part":text}),
            serde_json::json!({"type":"response.output_item.done","output_index":0,"item":message}),
            terminal(model, serde_json::json!([message])),
        ]);
    }
    let body =
        "{\"error\":{\"type\":\"invalid_request_error\",\"message\":\"synthetic rejection\"}}";
    format!("HTTP/1.1 {status} Fixture\r\ncontent-type: application/json\r\nx-request-id: req_test\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).into_bytes()
}
fn sse_response(events: Vec<serde_json::Value>) -> Vec<u8> {
    use std::fmt::Write as _;
    let mut body = String::new();
    for (index, mut event) in events.into_iter().enumerate() {
        event["sequence_number"] = serde_json::json!(index);
        write!(&mut body, "data: {event}\n\n").expect("string write");
    }
    format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).into_bytes()
}
fn created(model: &str) -> serde_json::Value {
    serde_json::json!({"type":"response.created","response":{"id":"resp_test","object":"response","status":"in_progress","model":model}})
}
fn terminal(model: &str, output: serde_json::Value) -> serde_json::Value {
    let mut event = serde_json::json!({"type":"response.completed","response":{"id":"resp_test","object":"response","status":"completed","model":model,"usage":{"input_tokens":8,"output_tokens":4,"total_tokens":12}}});
    event["response"]["output"] = output;
    event
}

async fn execute_request(
    configured: &ConfiguredLanguageModel,
    request: ModelRequest,
    active: Option<ActiveFamilyArtifact>,
    cancellation: CancellationToken,
) -> TestResult<Vec<Result<ModelEvent, ModelError>>> {
    let attempt = request.metadata().attempt_id();
    let plan = configured
        .registered()
        .descriptor()
        .validate_request(request, ValidationMode::Strict)?;
    let model = configured.model();
    let runtime = model.clone().revalidate_runtime_authority(now()).await?;
    let mut context = ModelPreparationContext::new(runtime, now());
    if let Some(active) = active {
        context = context.with_active_family_artifact(active);
    }
    let prepared = model.prepare_call(plan, context)?;
    let mut stream = prepared
        .start(ModelCallContext::new(attempt, cancellation, None))
        .await?;
    let mut events = Vec::new();
    while let Some(event) = poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
        events.push(event);
    }
    Ok(events)
}

async fn configured(
    source: Arc<Source>,
    selection: SelectedProviderConfiguration,
    client: Client,
    origin: plakous_http::Origin,
) -> TestResult<ConfiguredLanguageModel> {
    Ok(Arc::new(XaiPlatformModel::with_local_transport(
        authority(),
        source,
        NonZeroU64::new(65_536).unwrap(),
        selection,
        clock(),
        client,
        origin,
    )?)
    .configured()
    .await?)
}

/// Every effort the reviewed ladder publishes reaches the wire as
/// `reasoning.effort`, including `xhigh` and grok-4.3's explicit `none`.
#[tokio::test]
async fn reviewed_efforts_use_production_bearer_path_and_complete() -> TestResult {
    for (model, effort, wire_effort) in [
        (XAI_PLATFORM_GROK_46_MODEL, ReasoningEffort::Low, "low"),
        (
            XAI_PLATFORM_GROK_46_MODEL,
            ReasoningEffort::Medium,
            "medium",
        ),
        (XAI_PLATFORM_GROK_46_MODEL, ReasoningEffort::XHigh, "xhigh"),
        ("grok-4.7", ReasoningEffort::XHigh, "xhigh"),
        ("grok-4.3", ReasoningEffort::None, "none"),
    ] {
        let source = Arc::new(Source::new());
        let (client, origin, peer) = endpoint(response(model, 200)).await?;
        let configured =
            configured(source.clone(), selected_for(model, effort)?, client, origin).await?;
        assert_eq!(source.resolves.load(Ordering::SeqCst), 0);
        let events = execute_request(
            &configured,
            with_imported_reference(&request(&configured, effort)?)?,
            None,
            CancellationToken::new(),
        )
        .await?;
        assert!(events.iter().all(Result::is_ok), "{events:?}");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Ok(ModelEvent::ResponseCompleted(_)))),
            "{events:?}"
        );
        let wire = peer.await??;
        let end = wire
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .ok_or("headers")?;
        let headers = String::from_utf8_lossy(&wire[..end]).to_ascii_lowercase();
        assert!(headers.starts_with("post /v1/responses "));
        assert!(headers.contains("authorization: bearer synthetic-xai-platform-key"));
        assert!(
            !headers.contains("grok-session")
                && !headers.contains("cookie:")
                && !headers.contains("openai-project:")
        );
        let body: serde_json::Value = serde_json::from_slice(&wire[end + 4..])?;
        assert_eq!(body["model"], model);
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][1]["role"], "assistant");
        assert!(
            body["input"][1]["content"]
                .to_string()
                .contains("work ledger")
        );
        assert_eq!(
            body["reasoning"]["effort"], wire_effort,
            "{model} {effort:?}"
        );
        assert_eq!(
            body["include"],
            serde_json::json!(["reasoning.encrypted_content"])
        );
        assert_eq!(body["store"], false);
    }
    Ok(())
}

#[tokio::test]
async fn http_rejection_and_incomplete_or_wrong_model_stream_never_complete() -> TestResult {
    for (index, wire_response) in [
        response(XAI_PLATFORM_GROK_46_MODEL, 401),
        // A 429 is retried under the kernel policy; see the Responses host tests.
        response(XAI_PLATFORM_GROK_46_MODEL, 403),
        // A stream cut before its terminal is replayed for a turn; see the
        // Responses host tests.
        sse_response(vec![
            created(XAI_PLATFORM_GROK_46_MODEL),
            terminal("wrong-model", serde_json::json!([])),
        ]),
        sse_response(vec![
            created(XAI_PLATFORM_GROK_46_MODEL),
            created(XAI_PLATFORM_GROK_46_MODEL),
        ]),
    ]
    .into_iter()
    .enumerate()
    {
        let (client, origin, peer) = endpoint(wire_response).await?;
        let source = Arc::new(Source::new());
        let configured = configured(
            source.clone(),
            selected(ReasoningEffort::Low)?,
            client,
            origin,
        )
        .await?;
        let events = execute_request(
            &configured,
            request(&configured, ReasoningEffort::Low)?,
            None,
            CancellationToken::new(),
        )
        .await?;
        let errors: Vec<_> = events
            .iter()
            .filter_map(|event| event.as_ref().err())
            .collect();
        assert_eq!(errors.len(), 1, "{events:?}");
        assert!(matches!(events.last(), Some(Err(_))));
        if index < 2 {
            let error = errors[0];
            let (status, kind) = if index == 0 {
                (401, ModelErrorKind::Authentication)
            } else {
                (403, ModelErrorKind::Authorization)
            };
            assert_eq!(error.kind(), kind);
            let context = error.context().ok_or("HTTP error context")?;
            assert_eq!(context.phase(), ModelErrorPhase::FirstEvent);
            assert_eq!(
                context.http_status().map(ModelHttpStatus::get),
                Some(status)
            );
            assert_eq!(
                context.retryability(),
                if status == 401 {
                    ModelRetryability::Permanent
                } else {
                    ModelRetryability::Unsafe {
                        risk: ModelRetryRisk::DuplicateInferenceOrBilling,
                    }
                }
            );
            assert!(error.dispatch_rejection().is_none());
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, Ok(ModelEvent::ResponseStarted(_)))),
                "HTTP rejection must not publish acceptance: {events:?}"
            );
        }
        // An invalid SSE batch may be rejected before its created event is
        // published; packet boundaries must not determine terminal success.
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Ok(ModelEvent::ResponseCompleted(_))))
        );
        peer.await??;
        assert_eq!(source.resolves.load(Ordering::SeqCst), 1);
    }
    Ok(())
}

#[tokio::test]
async fn changed_or_revoked_key_is_refused_before_secret_resolution_and_socket() -> TestResult {
    for changed in [Some("changed-synthetic-key".to_owned()), None] {
        let source = Arc::new(Source::new());
        let (client, origin, peer) = endpoint(response(XAI_PLATFORM_GROK_46_MODEL, 200)).await?;
        let configured = configured(
            source.clone(),
            selected(ReasoningEffort::Low)?,
            client,
            origin,
        )
        .await?;
        *source.key.lock().unwrap() = changed;
        let result = execute_request(
            &configured,
            request(&configured, ReasoningEffort::Low)?,
            None,
            CancellationToken::new(),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(source.resolves.load(Ordering::SeqCst), 0);
        assert!(!peer.is_finished());
        peer.abort();
        let _ = peer.await;
    }
    Ok(())
}

async fn endpoint(
    response: Vec<u8>,
) -> TestResult<(
    Client,
    plakous_http::Origin,
    tokio::task::JoinHandle<io::Result<Vec<u8>>>,
)> {
    endpoint_inner(response, false).await
}
fn tool() -> TestResult<ConfiguredTool> {
    use plakous_model::{
        ConfiguredTool, ToolCallerDisposition, ToolConcurrency, ToolDefinition, ToolEffect,
        ToolExecutionDisposition, ToolInputContract, ToolName, ToolOutputContract,
    };
    Ok(ConfiguredTool::new(
        ToolDefinition::new(
            ToolName::new("lookup")?,
            "1.0.0",
            "Look up a value",
            ToolInputContract::structured(
                serde_json::json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{"q":{"type":"string"}},"required":["q"],"additionalProperties":false}),
            )?,
            ToolOutputContract::portable_model_content(),
            ToolConcurrency::Serial,
            ToolEffect::ReadOnly,
        )?,
        ToolExecutionDisposition::Local,
        ToolCallerDisposition::DirectOnly,
    ))
}
fn tool_request(
    configured: &ConfiguredLanguageModel,
    input: Vec<ConversationItem>,
    ordinal: u64,
) -> TestResult<ModelRequest> {
    Ok(ModelRequest::builder(
        configured.configuration().selected_identity(),
        configured.configuration().effective_identity(),
        ConversationView::from_items(input)?,
        RequestMetadata::for_turn(
            RunId::from_str("00000000-0000-4000-8000-000000000101")?,
            TurnId::from_str("00000000-0000-4000-8000-000000000102")?,
            AttemptId::from_str(&format!("00000000-0000-4000-8000-{ordinal:012}"))?,
            now(),
        ),
    )
    .tools(vec![tool()?])
    .context_admission_requirement(ContextAdmissionRequirement::AnyBounded)
    .reasoning(ReasoningPolicy::Enabled {
        effort: Some(ReasoningEffort::Low),
        requirement: CapabilityRequirement::Required,
    })
    .limits(GenerationLimits::new(NonZeroU32::new(128), Vec::new())?)
    .resolved_harness_plan_identity(Digest::sha256(
        DigestDomain::ResolvedHarnessPlan,
        1,
        &[b"platform-tools-test"],
    )?)
    .build()?)
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one source response proves same-key continuation and both changed-key artifact states"
)]
async fn local_tool_and_encrypted_reasoning_round_trip_reconstructs_exact_binding() -> TestResult {
    let reasoning = serde_json::json!({"type":"reasoning","id":"rs_test","status":"completed","summary":[],"encrypted_content":"synthetic-xai-ciphertext"});
    let function = serde_json::json!({"type":"function_call","id":"fc_test","status":"completed","call_id":"call_test","name":"lookup","arguments":"{\"q\":\"fixture\"}"});
    let events = vec![
        created(XAI_PLATFORM_GROK_46_MODEL),
        serde_json::json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_test","status":"in_progress","summary":[]}}),
        serde_json::json!({"type":"response.output_item.done","output_index":0,"item":reasoning}),
        serde_json::json!({"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc_test","status":"in_progress","call_id":"call_test","name":"lookup","arguments":""}}),
        serde_json::json!({"type":"response.function_call_arguments.delta","output_index":1,"item_id":"fc_test","delta":"{\"q\":\"fixture\"}"}),
        serde_json::json!({"type":"response.function_call_arguments.done","output_index":1,"item_id":"fc_test","arguments":"{\"q\":\"fixture\"}"}),
        serde_json::json!({"type":"response.output_item.done","output_index":1,"item":function}),
        terminal(
            XAI_PLATFORM_GROK_46_MODEL,
            serde_json::json!([reasoning, function]),
        ),
    ];
    let source = Arc::new(Source::new());
    let (client, origin, peer) = endpoint(sse_response(events)).await?;
    let mut behavior = selected(ReasoningEffort::Low)?.behavior().clone();
    behavior.tools = vec![tool()?];
    let selection = xai_platform_selection(behavior)?;
    let configured_model = configured(source.clone(), selection.clone(), client, origin).await?;
    let mut input = vec![ConversationItem::new(
        EntryId::from_str("00000000-0000-4000-8000-000000000201")?,
        Actor::User,
        vec![ContentBlock::Text(TextBlock::new("lookup fixture", None)?)],
        Provenance::UserInput,
    )?];
    let events = execute_request(
        &configured_model,
        tool_request(&configured_model, input.clone(), 301)?,
        None,
        CancellationToken::new(),
    )
    .await?;
    assert!(events.iter().all(Result::is_ok), "{events:?}");
    let output = events
        .iter()
        .find_map(|e| {
            if let Ok(ModelEvent::ResponseCompleted(response)) = e {
                Some(response)
            } else {
                None
            }
        })
        .ok_or("terminal")?;
    let call = output
        .output()
        .iter()
        .flat_map(ConversationItem::blocks)
        .find_map(|b| {
            if let ContentBlock::ToolCall(c) = b {
                Some(c)
            } else {
                None
            }
        })
        .ok_or("tool call")?;
    assert_eq!(call.arguments(), Some(&serde_json::json!({"q":"fixture"})));
    let candidate = output
        .family_artifact_candidate()
        .ok_or("maintained adapter omitted replay artifact")?;
    let active = ActiveFamilyArtifact::new(
        candidate.family(),
        candidate.schema_revision(),
        candidate.codec_revision(),
        candidate.binding(),
        candidate.bytes().to_vec(),
    )?;
    input.extend_from_slice(output.output());
    input.push(ConversationItem::new(
        EntryId::from_str("00000000-0000-4000-8000-000000000202")?,
        Actor::Tool,
        vec![ContentBlock::ToolResult(ToolResultBlock::new(
            call.call_id(),
            ToolExecutionOwner::Local,
            ToolResultKind::Success,
            vec![ToolResultPart::Text(TextBlock::new(
                "fixture result",
                None,
            )?)],
            call.correlation().clone(),
        )?)],
        Provenance::ToolExecution,
    )?);
    peer.await??;
    let (client, origin, peer) = endpoint(response(XAI_PLATFORM_GROK_46_MODEL, 200)).await?;
    let reopened = configured(source.clone(), selection.clone(), client, origin).await?;
    assert_eq!(configured_model.configuration(), reopened.configuration());
    let events = execute_request(
        &reopened,
        tool_request(&reopened, input.clone(), 302)?,
        Some(active.clone()),
        CancellationToken::new(),
    )
    .await?;
    assert!(events.iter().all(Result::is_ok), "{events:?}");
    let wire = peer.await??;
    let end = wire
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("headers")?;
    let body: serde_json::Value = serde_json::from_slice(&wire[end + 4..])?;
    assert!(
        body["input"]
            .as_array()
            .ok_or("input")?
            .iter()
            .any(|i| i["type"] == "function_call_output" && i["output"] == "fixture result")
    );
    assert!(
        body["input"]
            .as_array()
            .ok_or("input")?
            .iter()
            .any(|item| item["type"] == "reasoning"
                && item["encrypted_content"] == "synthetic-xai-ciphertext"),
        "encrypted replay was omitted: {body}"
    );
    // Canonical projection explicitly declares omission for the reasoning
    // envelope when the managed artifact cannot authorize native replay.
    assert!(
        input
            .iter()
            .flat_map(ConversationItem::blocks)
            .any(|block| {
                matches!(block, ContentBlock::Opaque(opaque)
            if opaque.fallback() == &OpaqueFallback::Omit
                && opaque.replay().payload().get().contains("synthetic-xai-ciphertext"))
            })
    );
    *source.key.lock().unwrap() = Some("different-synthetic-account-key".into());
    for retained_artifact in [None, Some(active)] {
        let (client, origin, peer) = endpoint(response(XAI_PLATFORM_GROK_46_MODEL, 200)).await?;
        let changed = configured(source.clone(), selection.clone(), client, origin).await?;
        let before = source.resolves.load(Ordering::SeqCst);
        let has_artifact = retained_artifact.is_some();
        let result = execute_request(
            &changed,
            tool_request(&changed, input.clone(), 303)?,
            retained_artifact,
            CancellationToken::new(),
        )
        .await;
        if has_artifact {
            assert!(
                result.is_err(),
                "foreign managed artifact must refuse preparation"
            );
            assert_eq!(source.resolves.load(Ordering::SeqCst), before);
            assert!(
                !peer.is_finished(),
                "foreign managed artifact reached the network"
            );
            peer.abort();
            let _ = peer.await;
        } else {
            assert!(result?.iter().all(Result::is_ok));
            let wire = peer.await??;
            let end = wire
                .windows(4)
                .position(|part| part == b"\r\n\r\n")
                .ok_or("headers")?;
            let body: serde_json::Value = serde_json::from_slice(&wire[end + 4..])?;
            assert!(!body.to_string().contains("synthetic-xai-ciphertext"));
            let projected = body["input"].as_array().ok_or("input")?;
            assert!(!projected.iter().any(|item| item["type"] == "reasoning"));
            assert!(
                projected
                    .iter()
                    .any(|item| item["type"] == "function_call_output"
                        && item["output"] == "fixture result")
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn cancellation_after_acceptance_closes_and_joins_transport() -> TestResult {
    let body = format!("data: {}\n\n", created(XAI_PLATFORM_GROK_46_MODEL));
    let response=format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n{:x}\r\n{}\r\n",body.len(),body).into_bytes();
    let (client, origin, peer) = endpoint_inner(response, true).await?;
    let source = Arc::new(Source::new());
    let configured = configured(
        source.clone(),
        selected(ReasoningEffort::Low)?,
        client,
        origin,
    )
    .await?;
    let request = request(&configured, ReasoningEffort::Low)?;
    let attempt = request.metadata().attempt_id();
    let plan = configured
        .registered()
        .descriptor()
        .validate_request(request, ValidationMode::Strict)?;
    let model = configured.model();
    let authority = model.clone().revalidate_runtime_authority(now()).await?;
    let prepared = model.prepare_call(plan, ModelPreparationContext::new(authority, now()))?;
    let cancellation = CancellationToken::new();
    let mut stream = prepared
        .start(ModelCallContext::new(attempt, cancellation.clone(), None))
        .await?;
    let first = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        poll_fn(|cx| stream.as_mut().poll_next(cx)),
    )
    .await?
    .ok_or("first event")?;
    assert!(
        matches!(first, Ok(ModelEvent::ResponseStarted(_))),
        "{first:?}"
    );
    cancellation.cancel();
    let events = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let mut events = Vec::new();
        while let Some(event) = poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
            events.push(event);
        }
        events
    })
    .await?;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Ok(ModelEvent::ResponseCompleted(_))))
    );
    let errors: Vec<_> = events
        .iter()
        .filter_map(|event| event.as_ref().err())
        .collect();
    assert_eq!(errors.len(), 1, "{events:?}");
    assert!(matches!(events.last(), Some(Err(_))));
    let error = errors[0];
    assert_eq!(error.kind(), ModelErrorKind::Cancelled);
    let context = error.context().ok_or("cancellation context")?;
    assert_eq!(context.phase(), ModelErrorPhase::Operation);
    // Accepted inference may have been billed; nothing may replay it.
    assert_eq!(
        context.retryability(),
        ModelRetryability::Unsafe {
            risk: ModelRetryRisk::DuplicateInferenceOrBilling
        }
    );
    assert!(context.http_status().is_none());
    assert!(error.dispatch_rejection().is_none());
    drop(stream);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), peer).await??;
    assert_eq!(source.resolves.load(Ordering::SeqCst), 1);
    Ok(())
}

#[test]
fn unsupported_behavior_and_stale_context_refuse_before_credentials() -> TestResult {
    let base = selected(ReasoningEffort::Low)?.behavior().clone();
    let mut unsupported = Vec::new();
    let mut behavior = base.clone();
    behavior.reasoning_effort = ReasoningEffortDisposition::Enabled(ReasoningEffort::Ultra);
    unsupported.push(behavior);
    let mut behavior = base.clone();
    behavior.hosted_tools = vec![HostedTool::CodeInterpreter];
    unsupported.push(behavior);
    let mut behavior = base.clone();
    behavior.context_evidence_requirement = ContextAdmissionRequirement::ReleaseVerifiedRequired;
    unsupported.push(behavior);
    let mut behavior = base.clone();
    behavior.local_durability = LocalDurabilityDisposition::Ephemeral;
    unsupported.push(behavior);
    for behavior in unsupported {
        let source = Arc::new(Source::new());
        let selection = xai_platform_selection(behavior)?;
        let result = XaiPlatformModel::new(
            authority(),
            source.clone(),
            NonZeroU64::new(65_536).unwrap(),
            selection,
            clock(),
        );
        assert!(matches!(result, Err(XaiPlatformBuildError::Configuration)));
        assert_eq!(source.grants.load(Ordering::SeqCst), 0);
        assert_eq!(source.resolves.load(Ordering::SeqCst), 0);
    }
    let source = Arc::new(Source::new());
    let stale = Arc::new(FixedClock::new(Timestamp::new(
        XAI_PLATFORM_REVIEW_EXPIRES_AT_UNIX_SECONDS + 86_400,
        0,
    )?));
    let preflight_then_grant = || {
        XaiPlatformModel::preflight_configuration(
            &selected(ReasoningEffort::Low).unwrap(),
            stale.as_ref(),
        )?;
        source.grant()
    };
    assert!(matches!(
        preflight_then_grant(),
        Err(XaiPlatformBuildError::Stale)
    ));
    assert!(matches!(
        XaiPlatformModel::new(
            authority(),
            source.clone(),
            NonZeroU64::new(65_536).unwrap(),
            selected(ReasoningEffort::Low)?,
            stale
        ),
        Err(XaiPlatformBuildError::Stale)
    ));
    assert_eq!(source.grants.load(Ordering::SeqCst), 0);
    assert!(
        !format!("{:?}", XaiPlatformApiKey::new("synthetic-visible-marker")?)
            .contains("synthetic-visible-marker")
    );
    Ok(())
}

#[tokio::test]
async fn key_revoked_after_preparation_is_fenced_at_dispatch() -> TestResult {
    let source = Arc::new(Source::new());
    let (client, origin, peer) = endpoint(response(XAI_PLATFORM_GROK_46_MODEL, 200)).await?;
    let configured = configured(
        source.clone(),
        selected(ReasoningEffort::Low)?,
        client,
        origin,
    )
    .await?;
    let request = request(&configured, ReasoningEffort::Low)?;
    let attempt = request.metadata().attempt_id();
    let plan = configured
        .registered()
        .descriptor()
        .validate_request(request, ValidationMode::Strict)?;
    let model = configured.model();
    let runtime = model.clone().revalidate_runtime_authority(now()).await?;
    let prepared = model.prepare_call(plan, ModelPreparationContext::new(runtime, now()))?;
    *source.key.lock().unwrap() = None;
    match prepared
        .start(ModelCallContext::new(
            attempt,
            CancellationToken::new(),
            None,
        ))
        .await
    {
        Err(_) => {}
        Ok(mut stream) => {
            let mut saw_error = false;
            while let Some(event) = poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
                assert!(!matches!(event, Ok(ModelEvent::ResponseCompleted(_))));
                saw_error |= event.is_err();
            }
            assert!(saw_error);
        }
    }
    assert_eq!(source.resolves.load(Ordering::SeqCst), 0);
    assert!(!peer.is_finished());
    peer.abort();
    let _ = peer.await;
    Ok(())
}

#[test]
fn hosted_code_witness_remains_non_executable() -> TestResult {
    let profile = ModelAdapterProfileId::new(XAI_PLATFORM_GROK_46_HOSTED_CODE_PROFILE)?;
    let revision = ProviderProfileRevisionId::new(XAI_PLATFORM_GROK_46_HOSTED_CODE_REVISION)?;
    let mut behavior = selected(ReasoningEffort::Low)?.behavior().clone();
    behavior.hosted_tools = vec![HostedTool::CodeInterpreter];
    behavior.retention = RetentionDisposition::RouteContract {
        profile: profile.clone(),
        revision: revision.clone(),
    };
    let selection = SelectedProviderConfiguration::new(
        ProviderRouteSelection::new(
            ProtocolId::new(XaiDialect::PlatformResponses2026_08.protocol_id())?,
            RouteId::new(XaiRouteKind::PlatformApiKey.route_id())?,
            profile,
            revision,
            ModelAdapterProfileId::new(XAI_PLATFORM_API_KEY_AUTH_PROFILE)?,
            ProviderProfileRevisionId::new(XAI_PLATFORM_API_KEY_AUTH_REVISION)?,
            ProviderAuthorityClassId::new(XAI_PLATFORM_API_KEY_AUTHORITY_CLASS)?,
            CredentialSelectionId::new(XAI_PLATFORM_API_KEY_CREDENTIAL_SELECTION)?,
            ModelId::new(XAI_PLATFORM_GROK_46_MODEL)?,
        ),
        behavior,
    )?;
    assert!(matches!(
        compile_xai_platform_grok_46_hosted_code_configuration(
            &selection,
            XaiPlatformHostedCodeWitness::reviewed_grok_46()?,
            &authority()
        ),
        Err(XaiPlatformConfigurationError::HostedCodeProductExtensionDisabled)
    ));
    Ok(())
}

#[tokio::test]
async fn public_constructor_keeps_caller_capacity_distinct_from_maintained_adapter() -> TestResult {
    let source = Arc::new(Source::new());
    let configured = XaiPlatformModel::from_behavior(
        authority(),
        source.clone(),
        NonZeroU64::new(65_536).unwrap(),
        selected(ReasoningEffort::Low)?.behavior().clone(),
        clock(),
    )
    .await?;
    let descriptor = configured.registered().descriptor();
    assert_eq!(descriptor.integration_class(), IntegrationClass::Standard);
    assert!(matches!(
        descriptor
            .context_contract()
            .ok_or("context")?
            .capacity()
            .evidence_class(),
        ContextEvidenceClass::CustomEnvelope { .. }
    ));
    let same = XaiPlatformModel::from_behavior(
        authority(),
        source.clone(),
        NonZeroU64::new(65_536).unwrap(),
        selected(ReasoningEffort::Low)?.behavior().clone(),
        clock(),
    )
    .await?;
    assert_eq!(configured.configuration(), same.configuration());
    *source.key.lock().unwrap() = Some("rotated-synthetic-key".to_owned());
    let rotated = XaiPlatformModel::from_behavior(
        authority(),
        source.clone(),
        NonZeroU64::new(65_536).unwrap(),
        selected(ReasoningEffort::Low)?.behavior().clone(),
        clock(),
    )
    .await?;
    assert_ne!(configured.configuration(), rotated.configuration());
    assert_eq!(source.resolves.load(Ordering::SeqCst), 0);
    Ok(())
}

fn x_search_query() -> ResponsesRetrievalQuery {
    ResponsesRetrievalQuery::PostSearch {
        query: "Rust programming language release".to_owned(),
        handles: vec!["rustlang".to_owned()],
        from_date: None,
        to_date: None,
    }
}

#[tokio::test]
async fn x_search_side_request_uses_the_session_key_and_model() -> TestResult {
    let call = serde_json::json!({"type": "x_search_call", "id": "xs_1", "status": "completed",
        "action": {"type": "search", "sources": [{"type": "url", "url": "https://x.com/rustlang/status/1"}]}});
    let message = serde_json::json!({"type": "message", "role": "assistant", "content": [
        {"type": "output_text", "text": "Rust 1.99 is out.", "annotations": []}]});
    let (client, origin, peer) = endpoint(sse_response(vec![
        created(XAI_PLATFORM_GROK_46_MODEL),
        terminal(
            XAI_PLATFORM_GROK_46_MODEL,
            serde_json::json!([call, message]),
        ),
    ]))
    .await?;
    let source = Arc::new(Source::new());
    let model = Arc::new(XaiPlatformModel::with_local_transport(
        authority(),
        source.clone(),
        NonZeroU64::new(65_536).unwrap(),
        selected(ReasoningEffort::Low)?,
        clock(),
        client,
        origin,
    )?);
    let outcome = model
        .retrieval(None)
        .retrieve(x_search_query(), &CancellationToken::new())
        .await?;
    assert_eq!(outcome.text, "Rust 1.99 is out.");
    assert_eq!(outcome.sources.len(), 1);
    assert_eq!(outcome.usage.hosted_requests, 1);
    assert_eq!(source.resolves.load(Ordering::SeqCst), 1);
    let wire = peer.await??;
    let head_end = wire
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .ok_or("request head")?;
    let head = String::from_utf8_lossy(&wire[..head_end]).to_ascii_lowercase();
    assert!(head.starts_with("post /v1/responses "), "{head}");
    assert!(head.contains("authorization: bearer synthetic-xai-platform-key"));
    let body: serde_json::Value = serde_json::from_slice(&wire[head_end + 4..])?;
    assert_eq!(body["model"], XAI_PLATFORM_GROK_46_MODEL);
    assert_eq!(
        body["tools"],
        serde_json::json!([{"type": "x_search", "allowed_x_handles": ["rustlang"]}])
    );
    assert_eq!(body.get("reasoning"), None);
    Ok(())
}

#[tokio::test]
async fn a_revoked_key_refuses_the_side_request_before_the_socket() -> TestResult {
    let (client, origin, peer) = endpoint(sse_response(Vec::new())).await?;
    let source = Arc::new(Source::new());
    let model = Arc::new(XaiPlatformModel::with_local_transport(
        authority(),
        source.clone(),
        NonZeroU64::new(65_536).unwrap(),
        selected(ReasoningEffort::Low)?,
        clock(),
        client,
        origin,
    )?);
    *source.key.lock().map_err(|_| "poisoned")? = None;
    let error = model
        .retrieval(None)
        .retrieve(x_search_query(), &CancellationToken::new())
        .await
        .err()
        .ok_or("a revoked key searched")?;
    assert_eq!(error.kind(), ResponsesRetrievalErrorKind::Unauthorized);
    assert_eq!(error.physical_requests(), 1);
    assert_eq!(source.resolves.load(Ordering::SeqCst), 0);
    peer.abort();
    Ok(())
}
