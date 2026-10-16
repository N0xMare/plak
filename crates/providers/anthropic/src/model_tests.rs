//! Production executor qualification against a controlled TLS peer.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "offline assertions"
)]
use super::*;
use futures_util::StreamExt as _;
use plakous_agent::{AgentBuilder, AgentRequestPolicy};
use plakous_http::TlsConfigIdentity;
use plakous_model::*;
use plakous_tools::{ToolRegistryBuilder, WorkspacePolicy};
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use std::{error::Error, io, num::NonZeroU32, time::Duration};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
    time::timeout,
};
use tokio_rustls::{TlsAcceptor, server::TlsStream};
type TestResult<T = ()> = Result<T, Box<dyn Error>>;
const SUCCESS_SSE: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_local_01\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-5\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"local\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":1}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);
async fn read_wire_request(stream: &mut TlsStream<TcpStream>) -> io::Result<Vec<u8>> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];
    let header_end = loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "request head EOF",
            ));
        }
        request.extend_from_slice(&buffer[..read]);
        if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
            break end + 4;
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
    while request.len() < header_end + content_length {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "request body EOF",
            ));
        }
        request.extend_from_slice(&buffer[..read]);
    }
    Ok(request)
}

async fn tls_exchange(
    responses: Vec<Option<Vec<u8>>>,
    stall_after_response: bool,
) -> TestResult<(Client, Origin, JoinHandle<io::Result<Vec<Vec<u8>>>>)> {
    tls_exchange_holding_first(responses, stall_after_response, Duration::ZERO).await
}

/// [`tls_exchange`] whose first response withholds its last bytes for `hold`,
/// so that body is still streaming when `hold` has passed.
async fn tls_exchange_holding_first(
    responses: Vec<Option<Vec<u8>>>,
    stall_after_response: bool,
    hold: Duration,
) -> TestResult<(Client, Origin, JoinHandle<io::Result<Vec<Vec<u8>>>>)> {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])?;
    let certificate = cert.der().clone();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
    let mut server = ServerConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(vec![certificate.clone()], private_key)?;
    server.alpn_protocols = vec![b"http/1.1".to_vec()];
    let mut roots = RootCertStore::empty();
    roots.add(CertificateDer::from(certificate.to_vec()))?;
    let client_config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let client = Client::with_tls_config_and_identity(
        Limits::default(),
        client_config,
        TlsConfigIdentity::from_parts(1, &[b"anthropic-local-tls-v1", certificate.as_ref()])?,
    )?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let origin = Origin::parse(&format!("https://{}", listener.local_addr()?))?;
    let acceptor = TlsAcceptor::from(Arc::new(server));
    let peer = tokio::spawn(async move {
        let mut requests = Vec::new();
        for (index, response) in responses.into_iter().enumerate() {
            let (stream, _) = listener.accept().await?;
            let mut stream = acceptor.accept(stream).await?;
            let request = read_wire_request(&mut stream).await?;
            if let Some(response) = response {
                let delivery = async {
                    let chunks = response.chunks(7).count();
                    for (sent, chunk) in response.chunks(7).enumerate() {
                        if index == 0 && sent + 1 == chunks {
                            tokio::time::sleep(hold).await;
                        }
                        stream.write_all(chunk).await?;
                        tokio::task::yield_now().await;
                    }
                    if stall_after_response {
                        let mut byte = [0u8; 1];
                        let _ = stream.read(&mut byte).await;
                        return Ok(());
                    }
                    stream.shutdown().await
                };
                // A client that reaches a terminal SSE error may close before the
                // fragmented body is fully written; that is a valid early close, not a
                // peer failure.
                match delivery.await {
                    Ok(()) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::BrokenPipe
                                | io::ErrorKind::ConnectionReset
                                | io::ErrorKind::NotConnected
                        ) => {}
                    Err(error) => return Err(error),
                }
            }
            requests.push(request);
        }
        Ok(requests)
    });
    Ok((client, origin, peer))
}

fn behavior() -> TestResult<ProviderBehaviorConfiguration> {
    let behavior = ProviderBehaviorConfiguration {
        processing_tier: ProcessingTierDisposition::Standard,
        tool_mode: ToolPresentationMode::Direct,
        reasoning_effort: ReasoningEffortDisposition::Enabled(ReasoningEffort::Low),
        reasoning_summary: ReasoningSummaryDisposition::Omit,
        reasoning_include: ReasoningIncludeDisposition::Omit,
        replay: ReplayDisposition::StrictEncrypted,
        cache: CachePolicy::Disabled,
        tools: Vec::new(),
        hosted_tools: Vec::new(),
        tool_choice: ToolChoice::Auto,
        parallel_tools: ParallelToolDisposition::Disabled,
        output: OutputPolicy::default(),
        output_bound: OutputBoundDisposition::Enforced(NonZeroU32::new(64).ok_or("output")?),
        storage: ProviderStorageDisposition::OmittedRouteContract,
        retention: RetentionDisposition::RouteContract {
            profile: ModelAdapterProfileId::new(crate::ANTHROPIC_API_ADAPTIVE_PROFILE_ID)?,
            revision: ProviderProfileRevisionId::new("messages-local-retention-v1")?,
        },
        response_state: ResponseStateDisposition::Stateless,
        context_evidence_requirement: ContextAdmissionRequirement::AnyBounded,
        local_durability: LocalDurabilityDisposition::Ephemeral,
    };
    Ok(behavior)
}

fn response(body: &str) -> Vec<u8> {
    format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nrequest-id: req_api_fixture\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",body.len()).into_bytes()
}

fn configured(
    client: Client,
    origin: Origin,
    source: Arc<dyn AnthropicApiKeySource>,
    model: &str,
    effort: ReasoningEffort,
) -> TestResult<ConfiguredLanguageModel> {
    let mut policy = behavior()?;
    policy.reasoning_effort = ReasoningEffortDisposition::Enabled(effort);
    Ok(AnthropicLanguageModel::build(
        source,
        "fixture-account",
        &crate::AnthropicApiModel::bundled(model)?,
        b"",
        policy,
        NonZeroU64::new(64_000).unwrap(),
        client,
        Some(origin),
    )?
    .0)
}

fn agent(
    configured: ConfiguredLanguageModel,
    workspace: &std::path::Path,
    effort: ReasoningEffort,
) -> TestResult<plakous_agent::Agent> {
    Ok(AgentBuilder::new(
        configured,
        ToolRegistryBuilder::default().freeze(),
        Arc::new(WorkspacePolicy::host(workspace, ".")?),
    )
    .request_policy(AgentRequestPolicy::new(
        GenerationLimits::new(NonZeroU32::new(64), Vec::new())?,
        ReasoningPolicy::Enabled {
            effort: Some(effort),
            requirement: CapabilityRequirement::Required,
        },
    ))
    .build()?)
}

#[tokio::test]
async fn production_api_executes_sonnet_and_opus_5_family_low_medium_through_agent() -> TestResult {
    for model in [
        crate::CLAUDE_SONNET_5_MODEL_ID,
        crate::config::CLAUDE_OPUS_5_MODEL_ID,
        crate::config::CLAUDE_OPUS_5_5_MODEL_ID,
    ] {
        for effort in [ReasoningEffort::Low, ReasoningEffort::Medium] {
            let body = SUCCESS_SSE.replace("claude-sonnet-5", model);
            let (client, origin, peer) = tls_exchange(vec![Some(response(&body))], false).await?;
            let binding = configured(
                client,
                origin,
                Arc::new(crate::AnthropicApiKey::new("offline-only")?),
                model,
                effort,
            )?;
            assert_eq!(
                binding.registered().descriptor().integration_class(),
                IntegrationClass::Standard
            );
            let root = tempfile::tempdir()?;
            let agent = agent(binding, root.path(), effort)?;
            let result = timeout(Duration::from_secs(5), async {
                agent.prompt("hello").await?.result().await
            })
            .await??;
            assert_eq!(result.response().ok_or("response")?.output_text(), "local");
            assert_eq!(result.model_rounds(), 1);
            agent.shutdown().await?;
            let wire = String::from_utf8(timeout(Duration::from_secs(5), peer).await???.remove(0))?;
            let (head, body) = wire.split_once("\r\n\r\n").ok_or("request framing")?;
            assert!(head.starts_with("POST /v1/messages HTTP/1.1"));
            assert!(head.contains("x-api-key: offline-only"));
            assert!(head.contains("anthropic-version: 2023-06-01"));
            for forbidden in ["authorization:", "anthropic-beta:", "x-app:"] {
                assert!(!head.to_ascii_lowercase().contains(forbidden));
            }
            let json: serde_json::Value = serde_json::from_str(body)?;
            assert_eq!(json["model"], model);
            assert_eq!(json["thinking"]["type"], "adaptive");
            assert_eq!(
                json["output_config"]["effort"],
                if effort == ReasoningEffort::Low {
                    "low"
                } else {
                    "medium"
                }
            );
            assert!(json.get("metadata").is_none());
        }
    }
    Ok(())
}

struct Rotating(std::sync::Mutex<&'static str>);
impl AnthropicApiKeySource for Rotating {
    fn current_key(&self) -> Result<crate::AnthropicApiKey, AnthropicError> {
        crate::AnthropicApiKey::new(*self.0.lock().unwrap())
    }
}

#[test]
fn api_complete_wire_boundary_preserves_typed_context_denial() -> TestResult {
    let limit = 64_000;
    let binding = AnthropicLanguageModel::configured(
        Arc::new(Rotating(std::sync::Mutex::new("offline-unused"))),
        "fixture-account",
        &crate::AnthropicApiModel::bundled(crate::CLAUDE_SONNET_5_MODEL_ID)?,
        b"",
        behavior()?,
        NonZeroU64::new(limit).unwrap(),
    )?;
    let now = binding.initial_authority().observed_at();
    let descriptor = binding.registered().descriptor();
    let plan_for = |text: &str| -> TestResult<ValidatedRequestPlan> {
        let request = ModelRequest::builder(
            binding.configuration().selected().identity(),
            binding.configuration().effective().identity(),
            ConversationView::from_items(vec![ConversationItem::new(
                EntryId::try_from(uuid::Uuid::from_u128(1))?,
                Actor::User,
                vec![ContentBlock::Text(TextBlock::new(text, None)?)],
                Provenance::UserInput,
            )?])?,
            RequestMetadata::for_turn(
                RunId::try_from(uuid::Uuid::from_u128(2))?,
                TurnId::try_from(uuid::Uuid::from_u128(3))?,
                AttemptId::try_from(uuid::Uuid::from_u128(4))?,
                now,
            ),
        )
        .reasoning(ReasoningPolicy::Enabled {
            effort: Some(ReasoningEffort::Low),
            requirement: CapabilityRequirement::Required,
        })
        .limits(GenerationLimits::new(NonZeroU32::new(64), vec![])?)
        .build()?;
        Ok(descriptor.validate_request(request, ValidationMode::Strict)?)
    };
    let context = ModelPreparationContext::new(binding.initial_authority().clone(), now);
    let baseline = binding
        .model()
        .prepare_call(plan_for("x")?, context.clone())?;
    let overhead = baseline.record().projection().wire_size() - 1;
    for wire_size in [limit - 1, limit, limit + 1] {
        let plan = plan_for(&"x".repeat(usize::try_from(wire_size - overhead)?))?;
        let input_identity = plan.request().model_input_identity();
        let result = binding.model().prepare_call(plan.clone(), context.clone());
        assert_eq!(result?.record().projection().wire_size(), wire_size);
        if wire_size <= limit {
            continue;
        }
        let error = binding
            .model()
            .prepare_call(plan, context.clone().with_required_input_bound())
            .expect_err("the paid fold requires a conservative input bound");
        assert!(matches!(
            error,
            ModelPreparationError::ContextAdmissionDenied { .. }
        ));
        let status = error.context_status().unwrap();
        assert_eq!(
            status.measurement().unwrap().bound().upper_bound(),
            wire_size
        );
        assert_eq!(status.model_input_identity(), input_identity);
        let status_identity = status.identity();
        let projected = ModelError::from(error);
        assert_eq!(projected.kind(), ModelErrorKind::ContextLimit);
        let failure = projected.context_capacity().unwrap();
        assert_eq!(failure.status_identity(), status_identity);
        assert_eq!(failure.model_input_identity(), input_identity);
        assert!(!failure.is_compactable());
    }
    Ok(())
}

#[tokio::test]
async fn production_api_rotation_revokes_existing_runtime_authority_before_dispatch() -> TestResult
{
    let (client, origin, mut peer) = tls_exchange(vec![None], false).await?;
    let source = Arc::new(Rotating(std::sync::Mutex::new("offline-one")));
    let binding = configured(
        client,
        origin,
        source.clone(),
        crate::CLAUDE_SONNET_5_MODEL_ID,
        ReasoningEffort::Low,
    )?;
    *source.0.lock().unwrap() = "offline-two";
    assert!(
        binding
            .model()
            .revalidate_runtime_authority(SystemClock.now())
            .await
            .is_err()
    );
    assert!(timeout(Duration::from_millis(50), &mut peer).await.is_err());
    peer.abort();
    assert!(peer.await.unwrap_err().is_cancelled());
    Ok(())
}

#[tokio::test]
async fn production_api_wrong_model_stream_never_completes() -> TestResult {
    for body in [SUCCESS_SSE.replace("claude-sonnet-5", "claude-opus-5")] {
        let (client, origin, peer) = tls_exchange(vec![Some(response(&body))], false).await?;
        let binding = configured(
            client,
            origin,
            Arc::new(crate::AnthropicApiKey::new("offline-only")?),
            crate::CLAUDE_SONNET_5_MODEL_ID,
            ReasoningEffort::Low,
        )?;
        let root = tempfile::tempdir()?;
        let agent = agent(binding, root.path(), ReasoningEffort::Low)?;
        let result = timeout(Duration::from_secs(5), async {
            agent.prompt("hello").await?.result().await
        })
        .await??;
        assert!(result.error().is_some(), "{result:?}");
        assert!(result.response().is_none());
        agent.shutdown().await?;
        timeout(Duration::from_secs(5), peer).await???;
    }
    Ok(())
}

/// A turn whose stream is cut before `message_stop` is replayed and completes
/// with the replay's response alone.
#[tokio::test]
async fn production_api_truncated_stream_is_replayed_for_a_turn() -> TestResult {
    let truncated = SUCCESS_SSE
        .split("event: message_stop")
        .next()
        .unwrap()
        .to_owned();
    let (client, origin, peer) = tls_exchange(
        vec![Some(response(&truncated)), Some(response(SUCCESS_SSE))],
        false,
    )
    .await?;
    let binding = configured(
        client,
        origin,
        Arc::new(crate::AnthropicApiKey::new("offline-only")?),
        crate::CLAUDE_SONNET_5_MODEL_ID,
        ReasoningEffort::Low,
    )?;
    let root = tempfile::tempdir()?;
    let agent = agent(binding, root.path(), ReasoningEffort::Low)?;
    let result = timeout(Duration::from_secs(5), async {
        agent.prompt("hello").await?.result().await
    })
    .await??;
    assert!(result.error().is_none(), "{result:?}");
    assert_eq!(result.response().ok_or("response")?.output_text(), "local");
    agent.shutdown().await?;
    timeout(Duration::from_secs(5), peer).await???;
    Ok(())
}

/// Each generation's head budget starts at its own dispatch. The first
/// generation streams for longer than the whole budget before its body ends
/// without `message_stop`, and the replay then waits out its backoff, so a
/// budget counted from the first dispatch would already be spent when the
/// replay is sent. The budget itself is wide enough that a loaded host's TLS
/// exchange fits inside it.
#[tokio::test]
async fn messages_replay_wait_gets_a_fresh_generation_head_budget() -> TestResult {
    const HEAD_BUDGET: Duration = Duration::from_secs(3);
    let truncated = SUCCESS_SSE
        .split("event: message_stop")
        .next()
        .unwrap()
        .to_owned();
    let (client, origin, peer) = tls_exchange_holding_first(
        vec![Some(response(&truncated)), Some(response(SUCCESS_SSE))],
        false,
        HEAD_BUDGET + Duration::from_millis(500),
    )
    .await?;
    let binding = configured(
        client,
        origin,
        Arc::new(crate::AnthropicApiKey::new("offline-only")?),
        crate::CLAUDE_SONNET_5_MODEL_ID,
        ReasoningEffort::Low,
    )?;
    let root = tempfile::tempdir()?;
    let agent = AgentBuilder::new(
        binding,
        ToolRegistryBuilder::default().freeze(),
        Arc::new(WorkspacePolicy::host(root.path(), ".")?),
    )
    .config(plakous_agent::AgentConfig::default().with_model_operation_timeout(HEAD_BUDGET))
    .request_policy(AgentRequestPolicy::new(
        GenerationLimits::new(NonZeroU32::new(64), Vec::new())?,
        ReasoningPolicy::Enabled {
            effort: Some(ReasoningEffort::Low),
            requirement: CapabilityRequirement::Required,
        },
    ))
    .build()?;
    let result = timeout(Duration::from_secs(30), async {
        agent.prompt("hello").await?.result().await
    })
    .await??;
    assert!(result.error().is_none(), "{result:?}");
    assert_eq!(result.response().ok_or("response")?.output_text(), "local");
    agent.shutdown().await?;
    timeout(Duration::from_secs(5), peer).await???;
    Ok(())
}

/// Runs one prompt to its terminal result and collects its retry markers.
async fn prompt_with_retries(
    agent: &plakous_agent::Agent,
    prompt: &str,
) -> TestResult<(plakous_agent::TurnResult, Vec<ModelRetry>)> {
    let mut turn = agent.prompt(prompt).await?;
    let mut events = turn.take_events()?;
    let mut retries = Vec::new();
    loop {
        let event = timeout(Duration::from_secs(10), events.recv()).await??;
        if let Some(ModelEvent::Retry(retry)) = event.kind().model_event()
            && retry.loss().declined_reason.is_none()
        {
            retries.push(retry.clone());
        }
        if matches!(
            event.kind(),
            plakous_agent::AgentEventKind::TurnCompleted { .. }
                | plakous_agent::AgentEventKind::TurnFailed { .. }
        ) {
            break;
        }
    }
    Ok((turn.result().await?, retries))
}

/// The motivating failure: the peer reads the whole request, then drops TLS
/// without a byte or a `close_notify`. The request may have reached the
/// provider, but a cost-only turn can only pay a duplicate inference, so the
/// turn replays it and completes.
#[tokio::test]
async fn production_api_dropped_exchange_after_send_is_replayed_for_a_turn() -> TestResult {
    let (client, origin, peer) =
        tls_exchange(vec![None, Some(response(SUCCESS_SSE))], false).await?;
    let binding = configured(
        client,
        origin,
        Arc::new(crate::AnthropicApiKey::new("offline-only")?),
        crate::CLAUDE_SONNET_5_MODEL_ID,
        ReasoningEffort::Low,
    )?;
    let root = tempfile::tempdir()?;
    let agent = agent(binding, root.path(), ReasoningEffort::Low)?;
    let (result, retries) = prompt_with_retries(&agent, "hello").await?;
    assert!(result.error().is_none(), "{result:?}");
    assert_eq!(result.response().ok_or("response")?.output_text(), "local");
    assert_eq!(retries.len(), 1, "{retries:?}");
    assert_eq!(retries[0].cause().kind(), "transport_after_send");
    assert!(!retries[0].output_discarded());
    assert_eq!(
        (retries[0].next_physical(), retries[0].max_physical()),
        (2, 4)
    );
    agent.shutdown().await?;
    let requests = timeout(Duration::from_secs(5), peer).await???;
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], requests[1], "the replay is the exact request");
    Ok(())
}

struct Echo(std::sync::atomic::AtomicUsize);
impl plakous_tools::ToolExecutor for Echo {
    fn execute(
        self: Arc<Self>,
        _: plakous_tools::ToolInvocation,
        _: plakous_tools::ToolContext,
    ) -> plakous_tools::ToolFuture {
        Box::pin(async move {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            plakous_tools::ToolOutcome::success_text("echo-result")
        })
    }
}

#[tokio::test]
async fn production_api_tool_result_continues_without_redispatching_effect() -> TestResult {
    let tool_sse = SUCCESS_SSE
        .replace(
            "\"type\":\"text\",\"text\":\"\"",
            "\"type\":\"tool_use\",\"id\":\"call_echo\",\"name\":\"echo\",\"input\":{}",
        )
        .replace(
            "\"type\":\"text_delta\",\"text\":\"local\"",
            "\"type\":\"input_json_delta\",\"partial_json\":\"{}\"",
        )
        .replace(
            "\"stop_reason\":\"end_turn\"",
            "\"stop_reason\":\"tool_use\"",
        );
    let thinking = concat!(
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"fixture reasoning\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"fixture-signature\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    );
    let tool_sse = tool_sse.replace("\"index\":0", "\"index\":1").replacen(
        "event: content_block_start",
        &format!("{thinking}event: content_block_start"),
        1,
    );
    let (client, origin, peer) = tls_exchange(
        vec![Some(response(&tool_sse)), Some(response(SUCCESS_SSE))],
        false,
    )
    .await?;
    let definition = ToolDefinition::new(
        ToolName::new("echo")?,
        "v1",
        "Echo fixture",
        ToolInputContract::structured(
            serde_json::json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{},"required":[],"additionalProperties":false}),
        )?,
        ToolOutputContract::portable_model_content(),
        ToolConcurrency::Serial,
        ToolEffect::ReadOnly,
    )?;
    let executor = Arc::new(Echo(std::sync::atomic::AtomicUsize::new(0)));
    let mut registry = ToolRegistryBuilder::default();
    registry.register(plakous_tools::ToolRegistration::new(
        plakous_tools::RegisteredToolId::new("echo")?,
        plakous_tools::ToolExecutorId::new("echo-v1")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"echo-v1"])?,
        definition.clone(),
        executor.clone(),
    ))?;
    let mut policy = behavior()?;
    policy.tools.push(ConfiguredTool::new(
        definition,
        ToolExecutionDisposition::Local,
        ToolCallerDisposition::DirectOnly,
    ));
    let binding = AnthropicLanguageModel::build(
        Arc::new(crate::AnthropicApiKey::new("offline-only")?),
        "fixture-account",
        &crate::AnthropicApiModel::bundled(crate::CLAUDE_SONNET_5_MODEL_ID)?,
        b"",
        policy,
        NonZeroU64::new(64_000).unwrap(),
        client,
        Some(origin),
    )?
    .0;
    let root = tempfile::tempdir()?;
    let agent = AgentBuilder::new(
        binding,
        registry.freeze(),
        Arc::new(WorkspacePolicy::host(root.path(), ".")?),
    )
    .request_policy(AgentRequestPolicy::new(
        GenerationLimits::new(NonZeroU32::new(64), vec![])?,
        ReasoningPolicy::Enabled {
            effort: Some(ReasoningEffort::Low),
            requirement: CapabilityRequirement::Required,
        },
    ))
    .build()?;
    let result = timeout(Duration::from_secs(5), async {
        agent.prompt("echo").await?.result().await
    })
    .await??;
    assert_eq!(
        result.response().ok_or("final response")?.output_text(),
        "local"
    );
    assert_eq!(result.model_rounds(), 2);
    assert_eq!(executor.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    agent.shutdown().await?;
    let requests = timeout(Duration::from_secs(5), peer).await???;
    assert_eq!(requests.len(), 2);
    let second = String::from_utf8(requests[1].clone())?;
    assert!(
        second.contains("tool_result")
            && second.contains("echo-result")
            && second.contains("call_echo")
    );
    assert!(second.contains("fixture-signature") && second.contains("fixture reasoning"));
    Ok(())
}

#[test]
fn production_api_reconstruction_preserves_semantics_but_key_rotation_does_not() -> TestResult {
    let source = Arc::new(Rotating(std::sync::Mutex::new("offline-one")));
    let construct = || {
        AnthropicLanguageModel::configured(
            source.clone(),
            "stable-account",
            &crate::AnthropicApiModel::bundled(crate::CLAUDE_SONNET_5_MODEL_ID)?,
            b"",
            behavior().unwrap(),
            NonZeroU64::new(64_000).unwrap(),
        )
    };
    let first = construct()?;
    let second = construct()?;
    assert_eq!(
        first.configuration().effective().identity(),
        second.configuration().effective().identity()
    );
    *source.0.lock().unwrap() = "offline-two";
    let rotated = construct()?;
    assert_ne!(
        first.configuration().effective().identity(),
        rotated.configuration().effective().identity()
    );
    assert_ne!(
        first.initial_authority().credential_authority_binding(),
        rotated.initial_authority().credential_authority_binding()
    );
    assert_eq!(
        first.initial_authority().account_binding(),
        rotated.initial_authority().account_binding()
    );
    Ok(())
}

struct Accepted(Arc<tokio::sync::Notify>);
impl plakous_http::ExchangeObserver for Accepted {
    fn start(&self, _: &[u8]) -> Option<Box<dyn plakous_http::ExchangeObservation>> {
        Some(Box::new(Self(self.0.clone())))
    }
}
impl plakous_http::ExchangeObservation for Accepted {
    fn response_head(&mut self, status: u16) {
        if status == 200 {
            self.0.notify_one();
        }
    }
    fn response_chunk(&mut self, _: &[u8]) {}
    fn terminal(&mut self, _: &plakous_http::ExchangeTerminal) {}
}

#[tokio::test]
async fn production_api_accepted_stall_cancellation_joins_connection() -> TestResult {
    let head=b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nrequest-id: req_stall\r\nconnection: close\r\n\r\n".to_vec();
    let (client, origin, peer) = tls_exchange(vec![Some(head)], true).await?;
    let accepted = Arc::new(tokio::sync::Notify::new());
    let client = client.with_observer(Some(Arc::new(Accepted(accepted.clone()))));
    let binding = configured(
        client,
        origin,
        Arc::new(crate::AnthropicApiKey::new("offline-only")?),
        crate::CLAUDE_SONNET_5_MODEL_ID,
        ReasoningEffort::Low,
    )?;
    let root = tempfile::tempdir()?;
    let agent = agent(binding, root.path(), ReasoningEffort::Low)?;
    let turn = agent.prompt("hello").await?;
    timeout(Duration::from_secs(5), accepted.notified()).await?;
    let cancelled = timeout(Duration::from_secs(5), turn.control().cancel()).await?;
    assert!(cancelled.is_ok(), "{cancelled:?}");
    let result = timeout(Duration::from_secs(5), turn.result()).await??;
    assert!(
        matches!(
            result.outcome(),
            plakous_agent::TurnResultOutcome::Cancelled(_)
        ),
        "{result:?}"
    );
    assert!(result.response().is_none());
    assert_eq!(
        result.error().ok_or("cancelled turn error")?.kind(),
        plakous_agent::AgentErrorKind::Cancelled
    );
    assert_eq!(result.model_rounds(), 1);
    timeout(Duration::from_secs(5), agent.shutdown()).await??;
    assert_eq!(timeout(Duration::from_secs(5), peer).await???.len(), 1);
    Ok(())
}

#[tokio::test]
async fn production_api_rotation_after_preparation_refuses_deferred_dispatch() -> TestResult {
    let (client, origin, mut peer) = tls_exchange(vec![None], false).await?;
    let source = Arc::new(Rotating(std::sync::Mutex::new("offline-one")));
    let binding = configured(
        client,
        origin,
        source.clone(),
        crate::CLAUDE_SONNET_5_MODEL_ID,
        ReasoningEffort::Low,
    )?;
    let now = SystemClock.now();
    let attempt = AttemptId::try_from(uuid::Uuid::from_u128(4))?;
    let request = ModelRequest::builder(
        binding.configuration().selected().identity(),
        binding.configuration().effective().identity(),
        ConversationView::from_items(vec![ConversationItem::new(
            EntryId::try_from(uuid::Uuid::from_u128(1))?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("hello", None)?)],
            Provenance::UserInput,
        )?])?,
        RequestMetadata::for_turn(
            RunId::try_from(uuid::Uuid::from_u128(2))?,
            TurnId::try_from(uuid::Uuid::from_u128(3))?,
            attempt,
            now,
        ),
    )
    .reasoning(ReasoningPolicy::Enabled {
        effort: Some(ReasoningEffort::Low),
        requirement: CapabilityRequirement::Required,
    })
    .limits(GenerationLimits::new(NonZeroU32::new(64), vec![])?)
    .build()?;
    let plan = binding
        .registered()
        .descriptor()
        .validate_request(request, ValidationMode::Strict)?;
    let prepared = binding.model().prepare_call(
        plan,
        ModelPreparationContext::new(binding.initial_authority().clone(), now),
    )?;
    *source.0.lock().unwrap() = "offline-two";
    let result = prepared
        .start(ModelCallContext::new(
            attempt,
            CancellationToken::new(),
            None,
        ))
        .await;
    assert!(matches!(result,Err(ref error) if error.kind()==ModelErrorKind::Authentication));
    assert!(timeout(Duration::from_millis(50), &mut peer).await.is_err());
    peer.abort();
    assert!(peer.await.unwrap_err().is_cancelled());
    Ok(())
}

#[tokio::test]
async fn production_api_old_signed_thinking_projects_changed_key_before_dispatch() -> TestResult {
    let thinking = concat!(
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"fixture reasoning\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"fixture-signature\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    );
    let source_sse = SUCCESS_SSE.replace("\"index\":0", "\"index\":1").replacen(
        "event: content_block_start",
        &format!("{thinking}event: content_block_start"),
        1,
    );
    let (client, origin, peer) = tls_exchange(vec![Some(response(&source_sse))], false).await?;
    let binding = configured(
        client,
        origin,
        Arc::new(crate::AnthropicApiKey::new("offline-one")?),
        crate::CLAUDE_SONNET_5_MODEL_ID,
        ReasoningEffort::Low,
    )?;
    let root = tempfile::tempdir()?;
    let source = agent(binding, root.path(), ReasoningEffort::Low)?;
    let result = timeout(Duration::from_secs(5), async {
        source.prompt("hello").await?.result().await
    })
    .await??;
    let output = result
        .response()
        .ok_or("missing source response")?
        .output()
        .to_vec();
    assert!(output.iter().flat_map(ConversationItem::blocks).any(
        |block| matches!(block, ContentBlock::Reasoning(reasoning) if reasoning.replay().is_some())
    ));
    source.shutdown().await?;
    assert_eq!(timeout(Duration::from_secs(5), peer).await???.len(), 1);
    for key in ["offline-one", "offline-two"] {
        let (client, origin, mut peer) = tls_exchange(vec![None], false).await?;
        let target = configured(
            client,
            origin,
            Arc::new(crate::AnthropicApiKey::new(key)?),
            crate::CLAUDE_SONNET_5_MODEL_ID,
            ReasoningEffort::Low,
        )?;
        let now = SystemClock.now();
        let mut history = output.clone();
        history.push(ConversationItem::new(
            EntryId::try_from(uuid::Uuid::from_u128(81))?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("continue", None)?)],
            Provenance::UserInput,
        )?);
        // Fresh target selectors model an explicitly admitted rebind, while the
        // individual historical signed item retains its original affinity.
        let request = ModelRequest::builder(
            target.configuration().selected().identity(),
            target.configuration().effective().identity(),
            ConversationView::from_items(history)?,
            RequestMetadata::for_turn(
                RunId::try_from(uuid::Uuid::from_u128(82))?,
                TurnId::try_from(uuid::Uuid::from_u128(83))?,
                AttemptId::try_from(uuid::Uuid::from_u128(84))?,
                now,
            ),
        )
        .reasoning(ReasoningPolicy::Enabled {
            effort: Some(ReasoningEffort::Low),
            requirement: CapabilityRequirement::Required,
        })
        .limits(GenerationLimits::new(NonZeroU32::new(64), vec![])?)
        .build()?;
        let plan = target
            .registered()
            .descriptor()
            .validate_request(request, ValidationMode::Strict)?;
        let prefix = plan
            .request()
            .input()
            .semantic_prefix_identity(plan.request().input().items().len() - 1)?;
        let unproven = target.model().prepare_call(
            plan.clone(),
            ModelPreparationContext::new(target.initial_authority().clone(), now),
        );
        assert_eq!(unproven.is_ok(), key == "offline-one");
        let prepared = target.model().prepare_call(
            plan,
            ModelPreparationContext::new(target.initial_authority().clone(), now)
                .with_completed_history_prefix(prefix),
        );
        assert!(prepared.is_ok());
        assert!(timeout(Duration::from_millis(50), &mut peer).await.is_err());
        peer.abort();
        assert!(peer.await.unwrap_err().is_cancelled());
    }
    Ok(())
}

/// A loopback Claude Sonnet tool-growth turn took a mid-stream
/// `rate_limit_error` and escalated the turn to `recovery_required`.
///
/// The stream is accepted and then carries only the provider's own overload
/// or rate-limit refusal: no content block, no tool call, no hosted effect.
/// Replaying it can only cost inference, so the turn replays it and
/// completes with the replay's output alone.
#[tokio::test]
async fn an_accepted_stream_that_only_refuses_is_replayed_for_the_turn() -> TestResult {
    for provider_kind in ["rate_limit_error", "overloaded_error"] {
        let refusal = response(&format!(
            "event: error\ndata: {{\"type\":\"error\",\"error\":{{\"type\":\"{provider_kind}\",\"message\":\"fixture refusal\"}}}}\n\n"
        ));
        let success = response(SUCCESS_SSE);
        let (client, origin, peer) =
            tls_exchange(vec![Some(refusal), Some(success)], false).await?;
        let binding = configured(
            client,
            origin,
            Arc::new(crate::AnthropicApiKey::new("offline-only")?),
            crate::CLAUDE_SONNET_5_MODEL_ID,
            ReasoningEffort::Low,
        )?;
        let root = tempfile::tempdir()?;
        let agent = agent(binding, root.path(), ReasoningEffort::Low)?;
        let result = timeout(Duration::from_secs(5), async {
            agent.prompt("hello").await?.result().await
        })
        .await??;
        assert!(result.error().is_none(), "{provider_kind}: {result:?}");
        assert_eq!(
            result.response().ok_or("response")?.output_text(),
            "local",
            "{provider_kind}"
        );
        agent.shutdown().await?;
        assert_eq!(timeout(Duration::from_secs(5), peer).await???.len(), 2);
    }
    Ok(())
}

#[tokio::test]
async fn api_turn_replays_a_rate_limit_on_the_same_exact_route() -> TestResult {
    let body =
        r#"{"type":"error","error":{"type":"rate_limit_error","message":"fixture refusal"}}"#;
    let refusal = format!(
        "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes();
    let (client, origin, peer) =
        tls_exchange(vec![Some(refusal), Some(response(SUCCESS_SSE))], false).await?;
    let binding = configured(
        client,
        origin,
        Arc::new(crate::AnthropicApiKey::new("offline-only")?),
        crate::CLAUDE_SONNET_5_MODEL_ID,
        ReasoningEffort::Low,
    )?;
    let root = tempfile::tempdir()?;
    let agent = agent(binding, root.path(), ReasoningEffort::Low)?;
    let result = timeout(Duration::from_secs(5), async {
        agent.prompt("hello").await?.result().await
    })
    .await??;
    assert_eq!(result.response().ok_or("response")?.output_text(), "local");
    assert_eq!(result.model_rounds(), 1);
    agent.shutdown().await?;
    let requests = timeout(Duration::from_secs(5), peer).await???;
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0], requests[1],
        "retry must keep the exact route and wire"
    );
    Ok(())
}

#[tokio::test]
async fn api_turn_rate_limits_retry_to_the_policy_budget_then_fail() -> TestResult {
    let body =
        r#"{"type":"error","error":{"type":"rate_limit_error","message":"fixture refusal"}}"#;
    let refusal = format!(
        "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\nretry-after: 0\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes();
    let (client, origin, peer) = tls_exchange(vec![Some(refusal); 4], false).await?;
    let binding = configured(
        client,
        origin,
        Arc::new(crate::AnthropicApiKey::new("offline-only")?),
        crate::CLAUDE_SONNET_5_MODEL_ID,
        ReasoningEffort::Low,
    )?;
    let root = tempfile::tempdir()?;
    let agent = agent(binding, root.path(), ReasoningEffort::Low)?;
    let (result, retries) = prompt_with_retries(&agent, "hello").await?;
    // Four physical requests in all: three announced replays, no fifth.
    assert_eq!(
        retries
            .iter()
            .map(|retry| (retry.next_physical(), retry.max_physical()))
            .collect::<Vec<_>>(),
        [(2, 4), (3, 4), (4, 4)]
    );
    assert_eq!(
        result
            .error()
            .ok_or("failed turn error")?
            .model_error()
            .ok_or("model error")?
            .kind(),
        ModelErrorKind::RateLimited
    );
    agent.shutdown().await?;
    assert_eq!(timeout(Duration::from_secs(5), peer).await???.len(), 4);
    Ok(())
}

/// A refusal the policy does not name (a `403`) and an ambiguous server
/// failure (a `500`) end the turn after one dispatch, whatever their body.
#[tokio::test]
async fn api_turn_does_not_retry_a_status_the_policy_does_not_name() -> TestResult {
    let body =
        r#"{"type":"error","error":{"type":"rate_limit_error","message":"fixture refusal"}}"#;
    for (status, retry_after) in [(403, ""), (400, "retry-after: 0\r\n")] {
        let wire = format!(
            "HTTP/1.1 {status} Rejected\r\ncontent-type: application/json\r\n{retry_after}content-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes();
        let (client, origin, peer) = tls_exchange(vec![Some(wire)], false).await?;
        let binding = configured(
            client,
            origin,
            Arc::new(crate::AnthropicApiKey::new("offline-only")?),
            crate::CLAUDE_SONNET_5_MODEL_ID,
            ReasoningEffort::Low,
        )?;
        let root = tempfile::tempdir()?;
        let agent = agent(binding, root.path(), ReasoningEffort::Low)?;
        let result = timeout(Duration::from_secs(5), async {
            agent.prompt("hello").await?.result().await
        })
        .await??;
        assert!(result.error().is_some(), "{status} must fail the turn");
        agent.shutdown().await?;
        assert_eq!(timeout(Duration::from_secs(5), peer).await???.len(), 1);
    }
    Ok(())
}

#[tokio::test]
async fn production_api_http_rejection_and_accepted_error_never_retry() -> TestResult {
    let rejected = |status, kind| {
        let body =
            serde_json::json!({"type":"error","error":{"type":kind,"message":"fixture refusal"}})
                .to_string();
        format!("HTTP/1.1 {status} Rejected\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).into_bytes()
    };
    for (wire, expected_kind, expected_status) in [
        (
            rejected(401, "authentication_error"),
            ModelErrorKind::Authentication,
            Some(401),
        ),
        (
            rejected(403, "permission_error"),
            ModelErrorKind::Authorization,
            Some(403),
        ),
    ] {
        let (client, origin, peer) = tls_exchange(vec![Some(wire)], false).await?;
        let binding = configured(
            client,
            origin,
            Arc::new(crate::AnthropicApiKey::new("offline-only")?),
            crate::CLAUDE_SONNET_5_MODEL_ID,
            ReasoningEffort::Low,
        )?;
        let root = tempfile::tempdir()?;
        let agent = agent(binding, root.path(), ReasoningEffort::Low)?;
        let result = timeout(Duration::from_secs(5), async {
            agent.prompt("hello").await?.result().await
        })
        .await??;
        let error = result
            .error()
            .ok_or("failed turn error")?
            .model_error()
            .ok_or("model error")?;
        assert_eq!(error.kind(), expected_kind);
        let context = error.context().ok_or("model error context")?;
        assert_eq!(context.phase(), ModelErrorPhase::Terminal);
        assert_eq!(
            context.http_status().map(ModelHttpStatus::get),
            expected_status
        );
        assert_eq!(context.retryability(), ModelRetryability::Permanent);
        assert!(error.dispatch_rejection().is_none());
        assert!(result.response().is_none());
        assert_eq!(result.model_rounds(), 1);
        agent.shutdown().await?;
        assert_eq!(timeout(Duration::from_secs(5), peer).await???.len(), 1);
    }
    Ok(())
}

struct CountingKey(std::sync::atomic::AtomicUsize);
impl AnthropicApiKeySource for CountingKey {
    fn current_key(&self) -> Result<crate::AnthropicApiKey, AnthropicError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        crate::AnthropicApiKey::new("offline-configuration-key")
    }
}

#[tokio::test]
async fn production_api_refuses_configuration_tampering_before_key_read_or_socket() -> TestResult {
    let (client, origin, mut peer) = tls_exchange(vec![None], false).await?;
    let source = Arc::new(CountingKey(std::sync::atomic::AtomicUsize::new(0)));
    let binding = configured(
        client,
        origin,
        source.clone(),
        crate::CLAUDE_SONNET_5_MODEL_ID,
        ReasoningEffort::Low,
    )?;
    let mut durable_behavior = behavior()?;
    durable_behavior.local_durability = LocalDurabilityDisposition::DurableRequired;
    let durable = AnthropicLanguageModel::configured(
        source.clone(),
        "fixture-account",
        &crate::AnthropicApiModel::bundled(crate::CLAUDE_SONNET_5_MODEL_ID)?,
        b"",
        durable_behavior,
        NonZeroU64::new(64_000).unwrap(),
    )?;
    let definition = ToolDefinition::new(
        ToolName::new("foreign-tool")?,
        "v1",
        "fixture",
        ToolInputContract::structured(
            serde_json::json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{},"required":[],"additionalProperties":false}),
        )?,
        ToolOutputContract::portable_model_content(),
        ToolConcurrency::Serial,
        ToolEffect::ReadOnly,
    )?;
    for case in [
        "selected",
        "effective",
        "durability",
        "medium",
        "high",
        "disabled",
        "output-bound",
        "tools",
        "tool-choice",
        "context",
        "authority",
        "cancelled",
        "deadline",
    ] {
        let reads = source.0.load(std::sync::atomic::Ordering::SeqCst);
        let selected = if matches!(case, "selected" | "durability") {
            durable.configuration().selected().identity()
        } else {
            binding.configuration().selected().identity()
        };
        let effective = if matches!(case, "effective" | "durability") {
            durable.configuration().effective().identity()
        } else {
            binding.configuration().effective().identity()
        };
        let now = SystemClock.now();
        let mut request = ModelRequest::builder(
            selected,
            effective,
            ConversationView::from_items(vec![ConversationItem::new(
                EntryId::try_from(uuid::Uuid::from_u128(91))?,
                Actor::User,
                vec![ContentBlock::Text(TextBlock::new("fixture", None)?)],
                Provenance::UserInput,
            )?])?,
            RequestMetadata::for_turn(
                RunId::try_from(uuid::Uuid::from_u128(92))?,
                TurnId::try_from(uuid::Uuid::from_u128(93))?,
                AttemptId::try_from(uuid::Uuid::from_u128(94))?,
                now,
            ),
        )
        .reasoning(ReasoningPolicy::Enabled {
            effort: Some(ReasoningEffort::Low),
            requirement: CapabilityRequirement::Required,
        })
        .limits(GenerationLimits::new(NonZeroU32::new(64), vec![])?)
        .context_admission_requirement(ContextAdmissionRequirement::AnyBounded);
        request = match case {
            "medium" => request.reasoning(ReasoningPolicy::Enabled {
                effort: Some(ReasoningEffort::Medium),
                requirement: CapabilityRequirement::Required,
            }),
            "high" => request.reasoning(ReasoningPolicy::Enabled {
                effort: Some(ReasoningEffort::High),
                requirement: CapabilityRequirement::Required,
            }),
            "disabled" => request.reasoning(ReasoningPolicy::Disabled),
            "output-bound" => request.limits(GenerationLimits::new(NonZeroU32::new(32), vec![])?),
            "tools" => request.tools(vec![ConfiguredTool::new(
                definition.clone(),
                ToolExecutionDisposition::Local,
                ToolCallerDisposition::DirectOnly,
            )]),
            "tool-choice" => request.tool_choice(ToolChoice::None),
            "context" => request.context_admission_requirement(
                ContextAdmissionRequirement::ReleaseVerifiedRequired,
            ),
            _ => request,
        };
        let plan = binding
            .registered()
            .descriptor()
            .validate_request(request.build()?, ValidationMode::Strict)?;
        let authority = if case == "authority" {
            RuntimeModelAuthority::new(
                binding.registered().descriptor(),
                binding.initial_authority().account_binding(),
                binding.initial_authority().credential_authority_binding(),
                2,
                now,
                None,
            )?
        } else {
            binding.initial_authority().clone()
        };
        let rejected = binding
            .model()
            .prepare_call(plan, ModelPreparationContext::new(authority, now));
        if matches!(case, "cancelled" | "deadline") {
            let prepared = rejected?;
            let cancellation = CancellationToken::new();
            if case == "cancelled" {
                cancellation.cancel();
            }
            let deadline = (case == "deadline").then_some(now);
            let result = prepared
                .start(ModelCallContext::new(
                    AttemptId::try_from(uuid::Uuid::from_u128(94))?,
                    cancellation,
                    deadline,
                ))
                .await;
            let expected = if case == "cancelled" {
                ModelErrorKind::Cancelled
            } else {
                ModelErrorKind::Timeout
            };
            assert!(matches!(result, Err(ref error) if error.kind() == expected));
            assert_eq!(
                source.0.load(std::sync::atomic::Ordering::SeqCst),
                reads + 1
            );
            continue;
        }
        if case == "tool-choice" {
            // Not tampering: withdrawing tool selection only narrows what the
            // round may do, and the runtime uses exactly this shape for the
            // wrap-up round it runs when a turn spends its operation budget.
            rejected?;
            continue;
        }
        assert!(rejected.is_err(), "tamper case {case} was accepted");
        assert_eq!(
            source.0.load(std::sync::atomic::Ordering::SeqCst),
            reads,
            "tamper case {case} read credentials"
        );
    }
    assert!(timeout(Duration::from_millis(50), &mut peer).await.is_err());
    peer.abort();
    assert!(peer.await.unwrap_err().is_cancelled());
    Ok(())
}

#[test]
fn production_api_complete_behavior_preflight_does_not_read_credentials() -> TestResult {
    let source = Arc::new(CountingKey(std::sync::atomic::AtomicUsize::new(0)));
    for case in ["context", "bound", "replay", "account"] {
        let mut policy = behavior()?;
        match case {
            "context" => {
                policy.context_evidence_requirement =
                    ContextAdmissionRequirement::ReleaseVerifiedRequired;
            }
            "bound" => {
                policy.output_bound =
                    OutputBoundDisposition::Enforced(NonZeroU32::new(128_001).unwrap());
            }
            "replay" => policy.replay = ReplayDisposition::Disabled,
            _ => {}
        }
        let account = if case == "account" {
            ""
        } else {
            "fixture-account"
        };
        assert!(
            AnthropicLanguageModel::configured(
                source.clone(),
                account,
                &crate::AnthropicApiModel::bundled(crate::CLAUDE_SONNET_5_MODEL_ID)?,
                b"",
                policy,
                NonZeroU64::new(64_000).unwrap()
            )
            .is_err()
        );
        assert_eq!(source.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
    Ok(())
}

#[test]
fn production_api_local_transport_refuses_nonloopback_before_key_read() -> TestResult {
    let source = Arc::new(CountingKey(std::sync::atomic::AtomicUsize::new(0)));
    for origin in ["https://example.invalid:8443", "https://127.0.0.1"] {
        assert!(
            AnthropicLanguageModel::configured_with_local_transport(
                source.clone(),
                "fixture-account",
                &crate::AnthropicApiModel::bundled(crate::CLAUDE_SONNET_5_MODEL_ID)?,
                b"",
                behavior()?,
                NonZeroU64::new(64_000).unwrap(),
                Client::new_http1(Limits::default())?,
                Origin::parse(origin)?,
            )
            .is_err()
        );
    }
    assert_eq!(source.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    Ok(())
}

struct WorkflowKey {
    key: std::sync::Mutex<&'static str>,
    reads: std::sync::atomic::AtomicUsize,
}
impl AnthropicApiKeySource for WorkflowKey {
    fn current_key(&self) -> Result<crate::AnthropicApiKey, AnthropicError> {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        crate::AnthropicApiKey::new(*self.key.lock().unwrap())
    }
}

#[tokio::test]
async fn production_api_workflow_connect_retry_revalidates_the_sealed_key() -> TestResult {
    for rotate in [false, true] {
        let (client, origin, mut peer) =
            tls_exchange(vec![Some(response(SUCCESS_SSE))], false).await?;
        let client =
            client.with_faults(Some(plakous_http::FaultPlan::parse("fault=connect,nth=1")?));
        let source = Arc::new(WorkflowKey {
            key: std::sync::Mutex::new("offline-workflow-key"),
            reads: std::sync::atomic::AtomicUsize::new(0),
        });
        let binding = configured(
            client,
            origin,
            source.clone(),
            crate::CLAUDE_SONNET_5_MODEL_ID,
            ReasoningEffort::Low,
        )?;
        let now = SystemClock.now();
        let attempt = AttemptId::try_from(uuid::Uuid::from_u128(94))?;
        let request = ModelRequest::builder(
            binding.configuration().selected().identity(),
            binding.configuration().effective().identity(),
            ConversationView::from_items(vec![ConversationItem::new(
                EntryId::try_from(uuid::Uuid::from_u128(91))?,
                Actor::User,
                vec![ContentBlock::Text(TextBlock::new("fixture", None)?)],
                Provenance::UserInput,
            )?])?,
            RequestMetadata::for_session_workflow(
                RunId::try_from(uuid::Uuid::from_u128(92))?,
                SessionWorkflowRootId::new([7; 32])?,
                attempt,
                now,
            ),
        )
        .reasoning(ReasoningPolicy::Enabled {
            effort: Some(ReasoningEffort::Low),
            requirement: CapabilityRequirement::Required,
        })
        .limits(GenerationLimits::new(NonZeroU32::new(64), vec![])?)
        .build()?;
        let plan = binding
            .registered()
            .descriptor()
            .validate_request(request, ValidationMode::Strict)?;
        let prepared = binding.model().prepare_call(
            plan,
            ModelPreparationContext::new(binding.initial_authority().clone(), now),
        )?;
        let mut stream = prepared
            .start(ModelCallContext::new(
                attempt,
                CancellationToken::new(),
                None,
            ))
            .await?;
        let mut retry_reads = None;
        let mut completed = false;
        let mut failed = false;
        while let Some(event) = timeout(Duration::from_secs(5), stream.next()).await? {
            match event {
                Ok(ModelEvent::Retry(_)) => {
                    retry_reads = Some(source.reads.load(std::sync::atomic::Ordering::SeqCst));
                    if rotate {
                        *source.key.lock().unwrap() = "offline-rotated-key";
                    }
                }
                Ok(ModelEvent::ResponseCompleted(_)) => completed = true,
                Err(error) => {
                    assert_eq!(error.kind(), ModelErrorKind::Transport);
                    failed = true;
                }
                _ => {}
            }
        }
        assert!(
            source.reads.load(std::sync::atomic::Ordering::SeqCst)
                > retry_reads.ok_or("retry marker")?,
            "retry must revalidate credentials before dispatch"
        );
        assert_eq!(completed, !rotate);
        assert_eq!(failed, rotate);
        if rotate {
            assert!(
                timeout(Duration::from_millis(100), &mut peer)
                    .await
                    .is_err()
            );
            peer.abort();
            assert!(peer.await.unwrap_err().is_cancelled());
        } else {
            assert_eq!(timeout(Duration::from_secs(5), peer).await???.len(), 1);
        }
    }
    Ok(())
}

/// Opt-in paid probe, never in a gate tier: one web search and one page fetch
/// side request on the Anthropic API with the operator's key. A pass is a
/// non-empty answer with at least one source per tool. Run with
/// `PLAK_LIVE_RETRIEVAL_PROBE=1 ANTHROPIC_API_KEY=... cargo nextest run -p
/// plakous-anthropic --run-ignored only -E 'test(live_retrieval_probe)'`.
#[tokio::test]
#[ignore = "paid live probe"]
async fn live_retrieval_probe_anthropic_api() -> TestResult {
    if std::env::var("PLAK_LIVE_RETRIEVAL_PROBE").as_deref() != Ok("1") {
        return Err("set PLAK_LIVE_RETRIEVAL_PROBE=1 to spend one probe per tool".into());
    }
    let key = std::env::var("ANTHROPIC_API_KEY")?;
    let (_, retrieval) = AnthropicLanguageModel::configured_with_retrieval(
        Arc::new(crate::AnthropicApiKey::new(key)?),
        "live-probe",
        &crate::AnthropicApiModel::bundled(crate::CLAUDE_SONNET_5_MODEL_ID)?,
        b"",
        behavior()?,
        NonZeroU64::new(64_000).ok_or("window")?,
        None,
    )?;
    let cancellation = CancellationToken::new();
    for query in [
        plakous_messages::MessagesRetrievalQuery::WebSearch {
            query: "Rust programming language latest stable release".to_owned(),
            allowed_domains: Vec::new(),
            blocked_domains: Vec::new(),
        },
        plakous_messages::MessagesRetrievalQuery::PageFetch {
            url: "https://www.rust-lang.org/".to_owned(),
            prompt: Some("what is the page's main heading".to_owned()),
        },
    ] {
        let tool = query.tool_name();
        let outcome = retrieval.retrieve(query, &cancellation).await?;
        eprintln!(
            "{tool}: exchanges={} sources={} usage={:?}",
            outcome.exchanges,
            outcome.sources.len(),
            outcome.usage
        );
        assert!(!outcome.text.trim().is_empty(), "{tool} answered nothing");
        assert!(!outcome.sources.is_empty(), "{tool} reported no source");
    }
    Ok(())
}

#[test]
fn api_side_request_uses_the_session_model_and_lowest_adaptive_effort() -> TestResult {
    let (_, retrieval) = AnthropicLanguageModel::configured_with_retrieval(
        Arc::new(crate::AnthropicApiKey::new("offline-only")?),
        "fixture-account",
        &crate::AnthropicApiModel::bundled(crate::CLAUDE_SONNET_5_MODEL_ID)?,
        b"",
        behavior()?,
        NonZeroU64::new(64_000).ok_or("window")?,
        None,
    )?;
    let body = retrieval
        .request(plakous_messages::MessagesRetrievalQuery::WebSearch {
            query: "q".to_owned(),
            allowed_domains: Vec::new(),
            blocked_domains: Vec::new(),
        })
        .body(&[])?;
    let value: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(value["model"], crate::CLAUDE_SONNET_5_MODEL_ID);
    assert_eq!(value["thinking"], serde_json::json!({"type": "adaptive"}));
    assert_eq!(value["output_config"], serde_json::json!({"effort": "low"}));
    assert_eq!(value.get("system"), None);
    assert_eq!(
        plakous_messages::classify_messages_retrieval_body(&body),
        plakous_messages::ReplaySafety::CostOnly
    );
    Ok(())
}
