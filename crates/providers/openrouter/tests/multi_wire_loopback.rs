//! Offline end-to-end evidence for the three configured `OpenRouter` wire lanes.
#![cfg(feature = "test-clock")]

use std::error::Error;
use std::io;
use std::num::NonZeroU32;
use std::str::FromStr as _;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt as _;
use plakous_agent::{AgentBuilder, AgentEventKind, AgentRequestPolicy};
use plakous_http::{Client, FaultPlan, Limits, TlsConfigIdentity};
use plakous_model::{
    Actor, AttemptId, CachePolicy, CapacityCertificate, CapacityRule, Clock, ContentBlock,
    ContextAdmissionRequirement, ContextContract, ContextEvidenceSource, ConversationItem,
    ConversationView, CountCertificateKind, Digest, DigestDomain, EvidenceFreshness,
    GenerationLimits, LocalDurabilityDisposition, MeasurementCertificate, ModelAdapterProfileId,
    ModelCallContext, ModelError, ModelErrorKind, ModelEvent, ModelPreparationContext,
    ModelRequest, OutputBoundDisposition, OutputPolicy, ParallelToolDisposition, ProtocolId,
    Provenance, ProviderBehaviorConfiguration, ProviderProfileRevisionId,
    ProviderStorageDisposition, ReasoningEffortDisposition, ReasoningIncludeDisposition,
    ReasoningSummaryDisposition, ReplayDisposition, RequestMetadata, ResponseStateDisposition,
    RetentionDisposition, RunId, RuntimeModelAuthority, SequenceIdGenerator, TextBlock, Timestamp,
    ToolCallBlock, ToolCallState, ToolCallerLineage, ToolChoice, ToolCorrelation,
    ToolExecutionOwner, ToolName, ToolPresentationMode, ToolResultBlock, ToolResultKind,
    ToolResultPart, TurnId, ValidationMode,
};
use plakous_openrouter::{
    CHAT_COMPLETIONS_FAMILY_ID, MESSAGES_FAMILY_ID, OPENROUTER_CHAT_COMPLETIONS_PATH,
    OPENROUTER_MESSAGES_PATH, OPENROUTER_RESPONSES_PATH, OpenRouterApiKey,
    OpenRouterChatDiscoveryConfig, OpenRouterChatLanguageModel, OpenRouterChatRoute,
    OpenRouterMessagesLanguageModel, OpenRouterOrigin, OpenRouterResponsesLanguageModel,
    StaticOpenRouterApiKeySource,
};
use plakous_tools::{ToolRegistryBuilder, ToolRuntimeErrorKind, WorkspacePolicy};
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;
use uuid::Uuid;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const PRE_DISPATCH_DEADLINE: Duration = Duration::from_millis(400);

#[test]
fn pre_dispatch_deadline_excludes_even_the_shortest_jittered_retry() -> TestResult {
    use plakous_attempt::{
        FailurePoint, Jitter, ObservationSnapshot, ProviderAcceptance, ReplaySafety, RetryPolicy,
        TerminalFacts,
    };
    let facts = TerminalFacts::from_snapshot(ObservationSnapshot {
        physical_attempts: 1,
        provider_acceptance: ProviderAcceptance::NotAccepted,
        visible_output: false,
        replay_safety: ReplaySafety::CostOnly,
        hosted_effects_possible: false,
        logical_success: false,
    });
    for jitter in [Jitter::SHORTEST, Jitter::LONGEST] {
        let delay = RetryPolicy::TURN
            .next_delay(facts, FailurePoint::Refused, None, Duration::ZERO, jitter)
            .ok_or_else(|| io::Error::other("one retry is eligible"))?;
        assert!(
            delay > PRE_DISPATCH_DEADLINE,
            "jittered retry {delay:?} fits inside {PRE_DISPATCH_DEADLINE:?}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn key_usage_distinguishes_invalid_key_from_forbidden_access() -> TestResult {
    for (status, unauthorized) in [("401 Unauthorized", true), ("403 Forbidden", false)] {
        let (client, origin, peer) = loopback_with_status(
            b"key-usage-status",
            vec![(status, "application/json", "{}".to_owned(), "")],
        )
        .await?;
        let source = Arc::new(StaticOpenRouterApiKeySource::new(
            OpenRouterApiKey::new("offline-status-fixture")?,
            "offline-account",
            "offline-authority",
        )?);
        let route = OpenRouterChatRoute::new_local_fixture_with_retention(
            source,
            OpenRouterOrigin::new(origin)?,
            plakous_openrouter::OpenRouterRetentionPolicy::strict(),
        )?
        .with_clock(Arc::new(review_clock()));
        let error = route
            .key_usage(&client)
            .await
            .err()
            .ok_or_else(|| io::Error::other("status refuses usage"))?;
        assert!(matches!(
            (&error, unauthorized),
            (
                plakous_openrouter::OpenRouterKeyUsageError::Unauthorized,
                true
            ) | (
                plakous_openrouter::OpenRouterKeyUsageError::Forbidden,
                false
            )
        ));
        assert!(!error.to_string().contains("offline-status-fixture"));
        let requests = peer.await??;
        assert_eq!(requests.len(), 1);
        assert!(std::str::from_utf8(&requests[0])?.starts_with("GET /api/v1/key "));
    }
    Ok(())
}

fn with_imported_reference(request: &ModelRequest) -> TestResult<ModelRequest> {
    let mut items = vec![
        ConversationItem::new(
            plakous_model::EntryId::from_str("00000000-0000-4000-8000-000000000991")?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new(
                "Original user goal: fix the parser",
                None,
            )?)],
            Provenance::UserInput,
        )?,
        ConversationItem::new(
            plakous_model::EntryId::from_str("00000000-0000-4000-8000-000000000992")?,
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

const MODEL: &str = "vendor/model-v1:free";

fn behavior(profile: &str, revision: &str) -> TestResult<ProviderBehaviorConfiguration> {
    Ok(ProviderBehaviorConfiguration {
        processing_tier: plakous_model::ProcessingTierDisposition::Standard,
        tool_mode: ToolPresentationMode::Direct,
        reasoning_effort: ReasoningEffortDisposition::Disabled,
        reasoning_summary: ReasoningSummaryDisposition::Omit,
        reasoning_include: ReasoningIncludeDisposition::Omit,
        replay: ReplayDisposition::DropUnreplayableReasoning,
        cache: CachePolicy::Disabled,
        tools: Vec::new(),
        hosted_tools: Vec::new(),
        tool_choice: ToolChoice::None,
        parallel_tools: ParallelToolDisposition::Disabled,
        output: OutputPolicy::default(),
        output_bound: OutputBoundDisposition::Enforced(
            NonZeroU32::new(64).ok_or_else(|| io::Error::other("nonzero output bound"))?,
        ),
        storage: ProviderStorageDisposition::DisabledOnWire,
        retention: RetentionDisposition::RouteContract {
            profile: ModelAdapterProfileId::new(profile)?,
            revision: ProviderProfileRevisionId::new(revision)?,
        },
        response_state: ResponseStateDisposition::Stateless,
        context_evidence_requirement: ContextAdmissionRequirement::AnyBounded,
        local_durability: LocalDurabilityDisposition::Ephemeral,
    })
}

fn context(
    route: &OpenRouterChatRoute,
    family: &str,
    accounting: &str,
    projection: &str,
) -> TestResult<ContextContract> {
    let authority = RuntimeModelAuthority::authority_class_for(
        route.identity().account_binding(),
        route.identity().credential_authority_binding(),
    )?;
    let declaration = Digest::sha256(
        DigestDomain::ProviderSemanticEvidence,
        1,
        &[
            b"test/openrouter-multi-wire-context-v1",
            route.identity().fingerprint().as_bytes(),
            family.as_bytes(),
        ],
    )?;
    let freshness = EvidenceFreshness::new(Timestamp::new(1, 0)?, None)?;
    let capacity = CapacityCertificate::custom_envelope(
        "test-openrouter-multi-wire-capacity",
        "v1",
        CapacityRule::ProviderReservedInput {
            reported_context_window: Some(4_096),
            maximum_projected_input: 2_048,
            auto_compact_input: None,
        },
        authority,
        freshness,
        declaration,
        "v1",
    )?;
    let measurement = MeasurementCertificate::new(
        "test-openrouter-multi-wire-measurement",
        "v1",
        "complete-lowered-request-body-utf8-byte-upper-bound",
        "v1",
        ContextEvidenceSource::CallerDeclaration,
        CountCertificateKind::CustomEnvelopeUpperBound { declaration },
        Some(authority),
        freshness,
    )?;
    Ok(ContextContract::new(
        ProtocolId::new(family)?,
        accounting,
        projection,
        "test-openrouter-output-reservation-v1",
        capacity,
        vec![measurement],
    )?)
}

fn request(
    configured: &plakous_model::ConfiguredLanguageModel,
    ordinal: u128,
) -> TestResult<ModelRequest> {
    request_rooted(configured, ordinal, false)
}

/// A turn request, or one session-workflow request (as compaction sends),
/// which the retry policy replays only when proven unsent.
fn request_rooted(
    configured: &plakous_model::ConfiguredLanguageModel,
    ordinal: u128,
    workflow: bool,
) -> TestResult<ModelRequest> {
    let run = RunId::from_str(&format!("00000000-0000-4000-8000-{:012}", ordinal + 20))?;
    let attempt = AttemptId::from_str(&format!("00000000-0000-4000-8000-{:012}", ordinal + 40))?;
    let metadata = if workflow {
        RequestMetadata::for_session_workflow(
            run,
            plakous_model::SessionWorkflowRootId::new([7; 32])?,
            attempt,
            review_clock().now(),
        )
    } else {
        RequestMetadata::for_turn(
            run,
            TurnId::from_str(&format!("00000000-0000-4000-8000-{:012}", ordinal + 30))?,
            attempt,
            review_clock().now(),
        )
    };
    let input = ConversationItem::new(
        plakous_model::EntryId::from_str(&format!("00000000-0000-4000-8000-{:012}", ordinal + 10))?,
        Actor::User,
        vec![ContentBlock::Text(TextBlock::new(
            "Answer on the selected wire.",
            None,
        )?)],
        Provenance::UserInput,
    )?;
    Ok(ModelRequest::builder(
        configured.configuration().selected_identity(),
        configured.configuration().effective_identity(),
        ConversationView::from_items(vec![input])?,
        metadata,
    )
    .tool_choice(ToolChoice::None)
    .limits(GenerationLimits::new(NonZeroU32::new(64), Vec::new())?)
    .resolved_harness_plan_identity(Digest::sha256(
        DigestDomain::ResolvedHarnessPlan,
        1,
        &[
            b"test/openrouter-multi-wire-plan-v1",
            &ordinal.to_be_bytes(),
        ],
    )?)
    .build()?)
}

fn request_with_retired_responses_exec(
    configured: &plakous_model::ConfiguredLanguageModel,
    ordinal: u128,
) -> TestResult<ModelRequest> {
    let call_id = plakous_model::CallId::from_str("00000000-0000-4000-8000-000000000901")?;
    let correlation = ToolCorrelation::new(
        Some("call-retired-exec".to_owned()),
        None,
        ToolCallerLineage::Direct,
    )?;
    let source = "text(\"retired exec history\");";
    let call = ToolCallBlock::new(
        call_id,
        ToolName::new("exec")?,
        ToolExecutionOwner::Local,
        ToolCallState::Requested,
        correlation.clone(),
        serde_json::to_string(source)?,
    )?;
    assert_eq!(call.state(), ToolCallState::Requested);
    assert_eq!(
        call.arguments().and_then(serde_json::Value::as_str),
        Some(source)
    );
    let input = ConversationView::from_items(vec![
        ConversationItem::new(
            plakous_model::EntryId::from_str("00000000-0000-4000-8000-000000000902")?,
            Actor::Assistant,
            vec![ContentBlock::ToolCall(call)],
            Provenance::ModelOutput,
        )?,
        ConversationItem::new(
            plakous_model::EntryId::from_str("00000000-0000-4000-8000-000000000903")?,
            Actor::Tool,
            vec![ContentBlock::ToolResult(ToolResultBlock::new(
                call_id,
                ToolExecutionOwner::Local,
                ToolResultKind::Success,
                vec![ToolResultPart::Text(TextBlock::new(
                    "retired exec result",
                    None,
                )?)],
                correlation,
            )?)],
            Provenance::ToolExecution,
        )?,
        ConversationItem::new(
            plakous_model::EntryId::from_str("00000000-0000-4000-8000-000000000904")?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new(
                "Continue on the selected wire.",
                None,
            )?)],
            Provenance::UserInput,
        )?,
    ])?;
    Ok(ModelRequest::builder(
        configured.configuration().selected_identity(),
        configured.configuration().effective_identity(),
        input,
        RequestMetadata::for_turn(
            RunId::from_str(&format!("00000000-0000-4000-8000-{:012}", ordinal + 20))?,
            TurnId::from_str(&format!("00000000-0000-4000-8000-{:012}", ordinal + 30))?,
            AttemptId::from_str(&format!("00000000-0000-4000-8000-{:012}", ordinal + 40))?,
            review_clock().now(),
        ),
    )
    .tool_choice(ToolChoice::None)
    .limits(GenerationLimits::new(NonZeroU32::new(64), Vec::new())?)
    .resolved_harness_plan_identity(Digest::sha256(
        DigestDomain::ResolvedHarnessPlan,
        1,
        &[b"test/openrouter-retired-responses-exec-v1"],
    )?)
    .build()?)
}

async fn execute_request(
    configured: &plakous_model::ConfiguredLanguageModel,
    request: ModelRequest,
) -> TestResult<Vec<ModelEvent>> {
    let model = configured.model();
    let attempt = request.metadata().attempt_id();
    let plan = configured
        .registered()
        .descriptor()
        .validate_request(request, ValidationMode::Strict)?;
    let authority = model
        .clone()
        .revalidate_runtime_authority(review_clock().now())
        .await?;
    let prepared = model.prepare_call(
        plan,
        ModelPreparationContext::new(authority, review_clock().now()),
    )?;
    let mut stream = prepared
        .start(ModelCallContext::new(
            attempt,
            plakous_model::CancellationToken::new(),
            None,
        ))
        .await?;
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event?);
    }
    Ok(events)
}

async fn execute_to_terminal(
    configured: &plakous_model::ConfiguredLanguageModel,
    ordinal: u128,
    cancel_after_response_started: bool,
    deadline: Option<Duration>,
    workflow: bool,
) -> TestResult<Vec<Result<ModelEvent, ModelError>>> {
    let model = configured.model();
    let request = request_rooted(configured, ordinal, workflow)?;
    let attempt = request.metadata().attempt_id();
    let plan = configured
        .registered()
        .descriptor()
        .validate_request(request, ValidationMode::Strict)?;
    let authority = model
        .clone()
        .revalidate_runtime_authority(review_clock().now())
        .await?;
    let prepared = model.prepare_call(
        plan,
        ModelPreparationContext::new(authority, review_clock().now()),
    )?;
    let cancellation = plakous_model::CancellationToken::new();
    let deadline = match deadline {
        Some(after) => {
            let now = review_clock().now();
            let at = Duration::new(now.seconds(), now.nanoseconds()) + after;
            Some(Timestamp::new(at.as_secs(), at.subsec_nanos())?)
        }
        None => None,
    };
    let mut stream = prepared
        .start(ModelCallContext::new(
            attempt,
            cancellation.clone(),
            deadline,
        ))
        .await?;
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        if cancel_after_response_started && matches!(event, Ok(ModelEvent::ResponseStarted(_))) {
            cancellation.cancel();
        }
        events.push(event);
    }
    Ok(events)
}

async fn read_request(stream: &mut TlsStream<TcpStream>) -> io::Result<Vec<u8>> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1_024];
    let header_end = loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "request head"));
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
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "request body"));
        }
        request.extend_from_slice(&buffer[..read]);
    }
    Ok(request)
}

/// Serves one scripted reply per accepted TLS connection and returns every
/// captured request in dispatch order.
async fn loopback(
    identity: &[u8],
    replies: Vec<(&'static str, String, &'static str)>,
) -> TestResult<(Client, String, JoinHandle<io::Result<Vec<Vec<u8>>>>)> {
    let replies = replies
        .into_iter()
        .map(|(content_type, body, headers)| ("200 OK", content_type, body, headers))
        .collect();
    loopback_with_status(identity, replies).await
}

/// One scripted reply whose body stays incomplete until `release` is notified.
///
/// The HTTP head and all but the last bytes are written first. The test waits
/// for a client-side response event before moving logical time and releasing it.
#[derive(Clone)]
struct HoldOneReply {
    index: usize,
    release: Arc<Notify>,
}

/// The same loopback with an explicit status line per reply.
///
/// A gateway refuses before the stream as well as inside it, and the two shapes
/// have to be answerable by the same peer.
async fn loopback_with_status(
    identity: &[u8],
    replies: Vec<(&'static str, &'static str, String, &'static str)>,
) -> TestResult<(Client, String, JoinHandle<io::Result<Vec<Vec<u8>>>>)> {
    loopback_with_status_holding(identity, replies, None).await
}

async fn loopback_with_status_holding(
    identity: &[u8],
    replies: Vec<(&'static str, &'static str, String, &'static str)>,
    hold: Option<HoldOneReply>,
) -> TestResult<(Client, String, JoinHandle<io::Result<Vec<Vec<u8>>>>)> {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["127.0.0.1".to_owned()])?;
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
        TlsConfigIdentity::from_parts(1, &[identity, certificate.as_ref()])?,
    )?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let origin = format!("https://{}", listener.local_addr()?);
    let acceptor = TlsAcceptor::from(Arc::new(server_config));
    let peer = tokio::spawn(async move {
        let mut requests = Vec::new();
        for (index, (status, content_type, body, extra_headers)) in replies.into_iter().enumerate()
        {
            let (stream, _) = listener.accept().await?;
            let mut stream = acceptor.accept(stream).await?;
            requests.push(read_request(&mut stream).await?);
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\n{extra_headers}content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            if hold.as_ref().is_some_and(|hold| hold.index == index) {
                let header_end = response
                    .find("\r\n\r\n")
                    .map_or(0, |offset| offset.saturating_add(4));
                let split = header_end
                    .max(response.len().saturating_sub(8))
                    .min(response.len());
                stream.write_all(&response.as_bytes()[..split]).await?;
                if let Some(hold) = hold.as_ref() {
                    hold.release.notified().await;
                }
                stream.write_all(&response.as_bytes()[split..]).await?;
            } else {
                stream.write_all(response.as_bytes()).await?;
            }
            stream.shutdown().await?;
        }
        Ok::<_, io::Error>(requests)
    });
    Ok((client, origin, peer))
}

/// The exact discovery pair every loopback chat lane consumes first.
fn discovery_replies() -> Vec<(&'static str, String, &'static str)> {
    vec![
        (
            "application/json",
            format!(
                "{{\"data\":[{{\"id\":\"{MODEL}\",\"canonical_slug\":\"{MODEL}\",\"context_length\":4096,\"supported_parameters\":[\"tools\",\"tool_choice\"]}}],\"links\":{{}},\"total_count\":1}}"
            ),
            "",
        ),
        (
            "application/json",
            format!(
                "{{\"data\":{{\"id\":\"{MODEL}\",\"endpoints\":[{{\"tag\":\"provider/model-v1\",\"provider_name\":\"Provider\",\"supported_parameters\":[\"tools\",\"tool_choice\"]}}]}}}}"
            ),
            "",
        ),
    ]
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one linear fixture audits catalog drift and all three exact dispatch paths"
)]
async fn catalog_drift_preserves_all_wire_bindings_and_refuses_changed_facts() -> TestResult {
    let endpoint = serde_json::json!({
        "tag": "provider/first", "provider_name": "First",
        "supported_parameters": ["tools", "tool_choice", "max_tokens", "reasoning"],
        "supports_tool_choice": {"auto": true},
        "context_length": 4096, "max_prompt_tokens": 3072,
        "max_completion_tokens": 1024, "supports_implicit_caching": false
    });
    let mut equal = endpoint.clone();
    equal["tag"] = serde_json::json!("provider/second");
    equal["provider_name"] = serde_json::json!("Second");
    let mut changed = equal.clone();
    changed["max_prompt_tokens"] = serde_json::json!(3000);
    let mut replies = Vec::new();
    for rows in [
        vec![endpoint.clone()],
        vec![equal, endpoint.clone()],
        vec![changed, endpoint],
    ] {
        let mut discovery = discovery_replies();
        discovery[1].1 = serde_json::json!({"data": {"id": MODEL, "endpoints": rows}}).to_string();
        replies.extend(discovery);
    }
    replies.extend([
        (
            "text/event-stream",
            chat_sse(),
            "x-request-id: drift-chat\r\n",
        ),
        (
            "text/event-stream",
            responses_sse(),
            "x-generation-id: drift-responses\r\n",
        ),
        (
            "text/event-stream",
            messages_sse()
                .split("event: data\ndata: [DONE]")
                .next()
                .ok_or("messages fixture")?
                .to_owned(),
            "x-request-id: drift-messages\r\n",
        ),
    ]);
    let (client, origin, peer) = loopback(b"catalog-drift", replies).await?;
    let route = OpenRouterChatRoute::new_local_fixture_with_retention(
        Arc::new(StaticOpenRouterApiKeySource::new(
            OpenRouterApiKey::new("offline-drift-key")?,
            "offline-account",
            "offline-authority",
        )?),
        OpenRouterOrigin::new(origin)?,
        plakous_openrouter::OpenRouterRetentionPolicy::strict(),
    )?
    .with_clock(Arc::new(review_clock()));
    let mut catalogs = Vec::new();
    for _ in 0..3 {
        catalogs.push(
            route
                .discover_chat_model(&client, MODEL, OpenRouterChatDiscoveryConfig::default())
                .await?,
        );
    }
    for (wire, family, accounting, projection, profile, revision) in [
        (
            0,
            CHAT_COMPLETIONS_FAMILY_ID,
            "openrouter-chat-custom-accounting-v1",
            "openrouter-chat-canonical-v1",
            "openrouter.chat.dynamic-v1",
            "openrouter-chat-retention-v1",
        ),
        (
            1,
            "responses-family",
            "responses-wire-bytes-openrouter-v1",
            "responses-canonical-openrouter-v1",
            "openrouter.responses.dynamic-custom-v1",
            "openrouter-responses-integration-v1",
        ),
        (
            2,
            MESSAGES_FAMILY_ID,
            "openrouter-messages-wire-v1",
            "openrouter-messages-canonical-v2",
            "openrouter.messages.dynamic-custom-v1",
            "openrouter-messages-integration-v1",
        ),
    ] {
        let mut configurations = Vec::new();
        let mut targets = Vec::new();
        for evidence in &catalogs {
            let contract = context(&route, family, accounting, projection)?;
            targets.push(match wire {
                0 => plakous_openrouter::OpenRouterChatTarget::gateway_managed(
                    route.clone(),
                    evidence.clone(),
                    contract.clone(),
                )?
                .target_identity(),
                1 => plakous_openrouter::OpenRouterResponsesTarget::gateway_managed(
                    route.clone(),
                    evidence.clone(),
                    contract.clone(),
                )?
                .target_identity(),
                _ => plakous_openrouter::OpenRouterMessagesTarget::gateway_managed(
                    route.clone(),
                    evidence.clone(),
                    contract.clone(),
                )?
                .target_identity(),
            });
            let behavior = behavior(profile, revision)?;
            let configured = match wire {
                0 => OpenRouterChatLanguageModel::configured_gateway_managed_behavior(
                    route.clone(),
                    evidence.clone(),
                    contract,
                    behavior,
                    client.clone(),
                )?,
                1 => OpenRouterResponsesLanguageModel::configured_gateway_managed_behavior(
                    route.clone(),
                    evidence.clone(),
                    contract,
                    behavior,
                    client.clone(),
                )?,
                _ => OpenRouterMessagesLanguageModel::configured_gateway_managed_behavior(
                    route.clone(),
                    evidence.clone(),
                    contract,
                    behavior,
                    client.clone(),
                )?,
            };
            configurations.push(configured);
        }
        assert_eq!(targets[0], targets[1], "target wire {wire}");
        assert_ne!(targets[0], targets[2], "target wire {wire}");
        assert_eq!(
            configurations[0].configuration().effective_identity(),
            configurations[1].configuration().effective_identity(),
            "wire {wire}"
        );
        assert_ne!(
            configurations[0].configuration().effective_identity(),
            configurations[2].configuration().effective_identity(),
            "wire {wire}"
        );
        let persisted_request = request(&configurations[0], 81 + wire)?;
        assert!(
            timeout(
                Duration::from_secs(5),
                execute_request(&configurations[2], persisted_request.clone())
            )
            .await?
            .is_err(),
            "changed admission refuses before dispatch"
        );
        execute_request(&configurations[1], persisted_request)
            .await
            .map_err(|error| io::Error::other(format!("wire {wire}: {error}")))?;
    }
    let requests = peer.await??;
    assert_eq!(requests.len(), 9, "changed facts never dispatch");
    for (index, path) in [
        OPENROUTER_CHAT_COMPLETIONS_PATH,
        OPENROUTER_RESPONSES_PATH,
        OPENROUTER_MESSAGES_PATH,
    ]
    .iter()
    .enumerate()
    {
        assert!(std::str::from_utf8(&requests[6 + index])?.starts_with(&format!("POST {path} ")));
    }
    for request in requests {
        let request = std::str::from_utf8(&request)?;
        assert!(
            request
                .lines()
                .any(|line| line.eq_ignore_ascii_case("authorization: Bearer offline-drift-key"))
        );
        assert!(request.starts_with("GET /api/v1/models") || request.starts_with("POST /api/v1/"));
    }
    Ok(())
}

fn chat_sse() -> String {
    let chunk = |choices: &str| {
        format!(
            "data: {{\"id\":\"chat-exact\",\"object\":\"chat.completion.chunk\",\"created\":1787623695,\"model\":\"{MODEL}\",\"choices\":{choices}}}\n\n"
        )
    };
    [
        chunk(r#"[{"index":0,"delta":{"role":"assistant","content":"chat"},"finish_reason":null}]"#),
        chunk(r#"[{"index":0,"delta":{},"finish_reason":"stop"}]"#),
        format!("data: {{\"id\":\"chat-exact\",\"object\":\"chat.completion.chunk\",\"created\":1787623695,\"model\":\"{MODEL}\",\"choices\":[],\"usage\":{{\"prompt_tokens\":4,\"completion_tokens\":1,\"total_tokens\":5}},\"openrouter_metadata\":{{\"attempt\":2,\"pipeline\":[{{\"type\":\"future_gateway_stage\",\"data\":{{\"opaque_future_member\":true}}}}],\"future_gateway_member\":{{\"opaque\":true}}}}}}\n\n"),
        "data: [DONE]\n\n".to_owned(),
    ]
    .concat()
}

fn responses_sse() -> String {
    format!(
        concat!(
            "data: {{\"type\":\"response.created\",\"sequence_number\":0,\"response\":{{\"id\":\"resp-exact\",\"model\":\"{}\",\"status\":\"in_progress\",\"output\":[]}}}}\n\n",
            "data: {{\"type\":\"response.done\",\"sequence_number\":1,\"response\":{{\"id\":\"resp-exact\",\"model\":\"{}\",\"object\":\"response\",\"status\":\"completed\",\"output\":[],\"usage\":{{\"input_tokens\":4,\"output_tokens\":1,\"total_tokens\":5}},\"incomplete_details\":null,\"error\":null}}}}\n\n",
            "data: [DONE]\n\n"
        ),
        MODEL, MODEL
    )
}

fn messages_sse() -> &'static str {
    concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg-exact\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"vendor/model-v1:free\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":4,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"messages\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":1}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
        "event: data\n",
        "data: [DONE]\n\n"
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalCase {
    AcceptedMalformedSse,
    AcceptedDisconnect,
    PreDispatchFailure,
    Cancellation,
    ChatHttpForbidden,
    ChatStreamForbidden,
    ChatHttpRateLimit,
    ChatStreamRateLimit,
    ChatHttpServerFailure,
    ChatBodyDeadline,
    MessagesBodyDeadline,
}

impl TerminalCase {
    const ALL: [Self; 4] = [
        Self::AcceptedMalformedSse,
        Self::AcceptedDisconnect,
        Self::PreDispatchFailure,
        Self::Cancellation,
    ];

    /// A turn replays these failures of a cost-only request; each case pins
    /// the classification of one physical failure, so it runs as a session
    /// workflow, which is never replayed.
    const fn replays_for_a_turn(self) -> bool {
        matches!(
            self,
            Self::AcceptedDisconnect
                | Self::ChatStreamRateLimit
                | Self::ChatHttpServerFailure
                | Self::ChatBodyDeadline
                | Self::MessagesBodyDeadline
        )
    }

    /// Every case is definite: none of them can have started a hosted
    /// effect. A refusal the gateway named keeps its own kind; a malformed
    /// stream, an in-band `403` or a disconnect is a failure the client had to
    /// infer, and reads as the break it was.
    const fn expected_kind(self) -> ModelErrorKind {
        match self {
            Self::AcceptedMalformedSse | Self::ChatStreamForbidden | Self::ChatHttpForbidden => {
                ModelErrorKind::Protocol
            }
            Self::AcceptedDisconnect | Self::PreDispatchFailure => ModelErrorKind::Transport,
            Self::ChatStreamRateLimit | Self::ChatHttpRateLimit => ModelErrorKind::RateLimited,
            Self::ChatHttpServerFailure => ModelErrorKind::Unavailable,
            Self::ChatBodyDeadline | Self::MessagesBodyDeadline => ModelErrorKind::Timeout,
            Self::Cancellation => ModelErrorKind::Cancelled,
        }
    }
}

/// Transport limits for one case.
///
/// A deadline case needs a body deadline it can actually reach inside a test,
/// and nothing else about the exchange changes.
fn case_limits(case: TerminalCase) -> Limits {
    let mut limits = Limits::default();
    if matches!(
        case,
        TerminalCase::ChatBodyDeadline | TerminalCase::MessagesBodyDeadline
    ) {
        limits.response_body_timeout = Duration::from_millis(250);
    }
    limits
}

#[allow(
    clippy::too_many_lines,
    reason = "one physical TLS fixture keeps each terminal classification auditable"
)]
async fn physical_terminal_case(case: TerminalCase, ordinal: u128) -> TestResult {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["127.0.0.1".to_owned()])?;
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
        case_limits(case),
        client_config,
        TlsConfigIdentity::from_parts(
            1,
            &[
                b"openrouter-messages-terminal-loopback-v1",
                certificate.as_ref(),
            ],
        )?,
    )?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let origin = format!("https://{}", listener.local_addr()?);
    let acceptor = TlsAcceptor::from(Arc::new(server_config));
    let models = format!(
        "{{\"data\":[{{\"id\":\"{MODEL}\",\"canonical_slug\":\"{MODEL}\",\"context_length\":4096,\"supported_parameters\":[\"tools\",\"tool_choice\"]}}],\"links\":{{}},\"total_count\":1}}"
    );
    let endpoints = format!(
        "{{\"data\":{{\"id\":\"{MODEL}\",\"endpoints\":[{{\"tag\":\"provider/model-v1\",\"provider_name\":\"Provider\",\"supported_parameters\":[\"tools\",\"tool_choice\"]}}]}}}}"
    );
    let peer = tokio::spawn(async move {
        let mut requests = Vec::new();
        for body in [models, endpoints] {
            let (stream, _) = listener.accept().await?;
            let mut stream = acceptor.accept(stream).await?;
            requests.push(read_request(&mut stream).await?);
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await?;
            stream.shutdown().await?;
        }
        if case == TerminalCase::PreDispatchFailure {
            return Ok::<_, io::Error>(requests);
        }
        if case == TerminalCase::ChatHttpRateLimit {
            // A turn retries a refused 429 to the policy budget of four
            // requests; the first three refusals ask for an immediate retry.
            let body = r#"{"error":{"code":429,"message":"rate limited"}}"#;
            for _ in 0..3 {
                let (stream, _) = listener.accept().await?;
                let mut stream = acceptor.accept(stream).await?;
                requests.push(read_request(&mut stream).await?);
                let response = format!(
                    "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\nretry-after: 0\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await?;
                stream.shutdown().await?;
            }
        }

        let (stream, _) = listener.accept().await?;
        let mut stream = acceptor.accept(stream).await?;
        requests.push(read_request(&mut stream).await?);
        match case {
            TerminalCase::AcceptedMalformedSse => {
                let body = "event: message_start\ndata: {malformed-json}\n\n";
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nx-request-id: req-malformed\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await?;
                stream.shutdown().await?;
            }
            TerminalCase::AcceptedDisconnect => {
                let body = "event: message_start\n";
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nx-request-id: req-disconnect\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len() + 128
                );
                stream.write_all(response.as_bytes()).await?;
                stream.shutdown().await?;
            }
            TerminalCase::Cancellation => {
                let body = concat!(
                    "event: message_start\n",
                    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg-cancel\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"vendor/model-v1:free\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":4,\"output_tokens\":0}}}\n\n"
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nx-request-id: req-cancel\r\nconnection: close\r\n\r\n{body}"
                );
                stream.write_all(response.as_bytes()).await?;
                let mut byte = [0_u8; 1];
                let _ = stream.read(&mut byte).await;
            }
            TerminalCase::ChatBodyDeadline | TerminalCase::MessagesBodyDeadline => {
                // The gateway accepted, started the stream, and then stopped
                // sending. It never says why, and it never closes: the client's
                // own body deadline is the only thing that ends this exchange.
                let body = "event: ping\n";
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nx-request-id: req-deadline\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len() + 4_096
                );
                stream.write_all(response.as_bytes()).await?;
                let mut byte = [0_u8; 1];
                let _ = stream.read(&mut byte).await;
            }
            TerminalCase::ChatHttpForbidden
            | TerminalCase::ChatStreamForbidden
            | TerminalCase::ChatHttpRateLimit
            | TerminalCase::ChatStreamRateLimit
            | TerminalCase::ChatHttpServerFailure => {
                let (status, content_type, body) = if case == TerminalCase::ChatHttpRateLimit {
                    (
                        "429 Too Many Requests",
                        "application/json",
                        r#"{"error":{"code":429,"message":"rate limited"}}"#.to_owned(),
                    )
                } else if case == TerminalCase::ChatHttpForbidden {
                    (
                        "403 Forbidden",
                        "application/json",
                        r#"{"error":{"code":403,"message":"agentic harness required"}}"#.to_owned(),
                    )
                } else if case == TerminalCase::ChatStreamForbidden {
                    (
                        "200 OK",
                        "text/event-stream",
                        "data: {\"error\":{\"code\":403,\"message\":\"policy blocked\"}}\n\n"
                            .to_owned(),
                    )
                } else if case == TerminalCase::ChatHttpServerFailure {
                    (
                        "502 Bad Gateway",
                        "application/json",
                        r#"{"error":{"code":502,"message":"upstream unavailable"}}"#.to_owned(),
                    )
                } else {
                    (
                        "200 OK",
                        "text/event-stream",
                        "data: {\"error\":{\"code\":429,\"message\":\"rate limited\"}}\n\n"
                            .to_owned(),
                    )
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\nx-request-id: req-rate-limit\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await?;
                stream.shutdown().await?;
            }
            TerminalCase::PreDispatchFailure => unreachable!(),
        }
        Ok::<_, io::Error>(requests)
    });

    let source = Arc::new(StaticOpenRouterApiKeySource::new(
        OpenRouterApiKey::new("offline-terminal-key")?,
        "offline-terminal-account",
        "offline-terminal-authority",
    )?);
    let route = OpenRouterChatRoute::new_local_fixture_with_retention(
        source,
        OpenRouterOrigin::new(origin)?,
        plakous_openrouter::OpenRouterRetentionPolicy::strict(),
    )?
    .with_clock(Arc::new(review_clock()));
    let evidence = route
        .discover_chat_model(
            &client,
            MODEL,
            OpenRouterChatDiscoveryConfig {
                operation_timeout: Duration::from_secs(5),
                maximum_body_bytes: 64 * 1_024,
                evidence_ttl: Duration::from_secs(60),
            },
        )
        .await?;
    let messages = if matches!(
        case,
        TerminalCase::ChatHttpForbidden
            | TerminalCase::ChatStreamForbidden
            | TerminalCase::ChatHttpRateLimit
            | TerminalCase::ChatStreamRateLimit
            | TerminalCase::ChatHttpServerFailure
            | TerminalCase::ChatBodyDeadline
    ) {
        OpenRouterChatLanguageModel::configured_gateway_managed_behavior(
            route.clone(),
            evidence,
            context(
                &route,
                CHAT_COMPLETIONS_FAMILY_ID,
                "openrouter-chat-custom-accounting-v1",
                "openrouter-chat-canonical-v1",
            )?,
            behavior("openrouter.chat.dynamic-v1", "openrouter-chat-retention-v1")?,
            client,
        )?
    } else {
        OpenRouterMessagesLanguageModel::configured_gateway_managed_behavior(
            route.clone(),
            evidence,
            context(
                &route,
                MESSAGES_FAMILY_ID,
                "openrouter-messages-wire-v1",
                "openrouter-messages-canonical-v2",
            )?,
            behavior(
                "openrouter.messages.dynamic-custom-v1",
                "openrouter-messages-integration-v1",
            )?,
            client,
        )?
    };

    let mut peer = Some(peer);
    let pre_dispatch_requests = if case == TerminalCase::PreDispatchFailure {
        Some(
            timeout(
                Duration::from_secs(5),
                peer.take()
                    .ok_or_else(|| io::Error::other("Messages terminal peer"))?,
            )
            .await???,
        )
    } else {
        None
    };
    let events = timeout(
        Duration::from_secs(5),
        // Equal jitter starts at 500 ms. A deadline below that minimum
        // excludes every retry sample, not merely the longest backoff.
        execute_to_terminal(
            &messages,
            ordinal,
            case == TerminalCase::Cancellation,
            (case == TerminalCase::PreDispatchFailure).then_some(PRE_DISPATCH_DEADLINE),
            case.replays_for_a_turn(),
        ),
    )
    .await??;
    let requests = match pre_dispatch_requests {
        Some(requests) => requests,
        None => {
            timeout(
                Duration::from_secs(5),
                peer.take()
                    .ok_or_else(|| io::Error::other("Messages terminal peer"))?,
            )
            .await???
        }
    };

    let errors = events
        .iter()
        .filter_map(|event| event.as_ref().err())
        .collect::<Vec<_>>();
    assert_eq!(errors.len(), 1, "{case:?}: {events:?}");
    assert_eq!(errors[0].kind(), case.expected_kind(), "{case:?}");
    if matches!(
        case,
        TerminalCase::ChatHttpRateLimit | TerminalCase::ChatHttpForbidden
    ) {
        assert!(errors[0].to_string().contains("rejected-before-acceptance"));
    }
    if case == TerminalCase::ChatHttpServerFailure {
        assert!(errors[0].to_string().contains("HTTP 502"));
    }
    assert!(
        matches!(events.last(), Some(Err(_))),
        "{case:?}: terminal item"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Ok(ModelEvent::ResponseCompleted(_))))
            .count(),
        0,
        "{case:?}: completion"
    );
    assert_eq!(
        requests.len(),
        match case {
            TerminalCase::PreDispatchFailure => 2,
            TerminalCase::ChatHttpRateLimit => 6,
            _ => 3,
        },
        "{case:?}: physical requests"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn messages_physical_terminal_errors_are_single_and_fact_bearing() -> TestResult {
    for (index, case) in TerminalCase::ALL.into_iter().enumerate() {
        physical_terminal_case(case, 100 + index as u128).await?;
    }
    Ok(())
}

/// A post-acceptance deadline is a definite end on both `OpenRouter` wires.
///
/// The classifier used to read acceptance alone here: the gateway had committed
/// HTTP 200, so a stream that then stopped arriving was an unknown outcome and
/// fenced the session. But a deadline is the client's own decision to stop
/// waiting, not something the provider left ambiguous, and the adapter now says
/// so by naming the refusal on the transport-failure path. These cases go
/// through the real adapter, not the classification helper: the peer accepts,
/// sends stream framing, and then holds the connection open forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_post_acceptance_deadline_is_definite_on_both_wires() -> TestResult {
    physical_terminal_case(TerminalCase::ChatBodyDeadline, 300).await?;
    physical_terminal_case(TerminalCase::MessagesBodyDeadline, 301).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_http_policy_and_rate_limits_reject_but_stream_errors_are_ambiguous() -> TestResult {
    physical_terminal_case(TerminalCase::ChatHttpRateLimit, 200).await?;
    physical_terminal_case(TerminalCase::ChatStreamRateLimit, 201).await?;
    physical_terminal_case(TerminalCase::ChatHttpForbidden, 203).await?;
    physical_terminal_case(TerminalCase::ChatStreamForbidden, 204).await?;
    physical_terminal_case(TerminalCase::ChatHttpServerFailure, 202).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_lanes_dispatch_only_the_selected_exact_path() -> TestResult {
    Box::pin(configured_lanes_case(false)).await?;
    Box::pin(configured_lanes_case(true)).await
}

#[allow(
    clippy::too_many_lines,
    reason = "one linear fixture keeps the discovery and three exact dispatches auditable"
)]
async fn configured_lanes_case(attributed: bool) -> TestResult {
    let mut replies = discovery_replies();
    replies.extend([
        (
            "text/event-stream",
            chat_sse(),
            "x-request-id: req-chat\r\n",
        ),
        (
            "text/event-stream",
            responses_sse(),
            "x-generation-id: req-responses\r\n",
        ),
        (
            "text/event-stream; charset=utf-8",
            messages_sse()
                .split("event: data\ndata: [DONE]")
                .next()
                .ok_or_else(|| io::Error::other("Messages fixture trailer"))?
                .to_owned(),
            "x-request-id: req-messages\r\n",
        ),
    ]);
    let (client, origin, peer) = loopback(b"openrouter-multi-wire-loopback-v1", replies).await?;

    let source = Arc::new(StaticOpenRouterApiKeySource::new(
        OpenRouterApiKey::new("offline-loopback-key")?,
        "offline-account",
        "offline-authority",
    )?);
    let route = OpenRouterChatRoute::new_local_fixture_with_retention(
        source,
        OpenRouterOrigin::new(origin)?,
        plakous_openrouter::OpenRouterRetentionPolicy::strict(),
    )?
    .with_clock(Arc::new(review_clock()));
    let route = if attributed {
        route.with_plak_attribution()?
    } else {
        route
    };
    let evidence = route
        .discover_chat_model(
            &client,
            MODEL,
            OpenRouterChatDiscoveryConfig {
                operation_timeout: Duration::from_secs(5),
                maximum_body_bytes: 64 * 1_024,
                evidence_ttl: Duration::from_secs(60),
            },
        )
        .await?;
    let chat = OpenRouterChatLanguageModel::configured_gateway_managed_behavior(
        route.clone(),
        evidence.clone(),
        context(
            &route,
            CHAT_COMPLETIONS_FAMILY_ID,
            "openrouter-chat-custom-accounting-v1",
            "openrouter-chat-canonical-v1",
        )?,
        behavior("openrouter.chat.dynamic-v1", "openrouter-chat-retention-v1")?,
        client.clone(),
    )?;
    let responses = OpenRouterResponsesLanguageModel::configured_gateway_managed_behavior(
        route.clone(),
        evidence.clone(),
        context(
            &route,
            "responses-family",
            "responses-wire-bytes-openrouter-v1",
            "responses-canonical-openrouter-v1",
        )?,
        behavior(
            "openrouter.responses.dynamic-custom-v1",
            "openrouter-responses-integration-v1",
        )?,
        client.clone(),
    )?;
    let messages = OpenRouterMessagesLanguageModel::configured_gateway_managed_behavior(
        route.clone(),
        evidence,
        context(
            &route,
            MESSAGES_FAMILY_ID,
            "openrouter-messages-wire-v1",
            "openrouter-messages-canonical-v2",
        )?,
        behavior(
            "openrouter.messages.dynamic-custom-v1",
            "openrouter-messages-integration-v1",
        )?,
        client,
    )?;

    let chat_request = request_with_retired_responses_exec(&chat, 1)?;
    let chat_events = timeout(
        Duration::from_secs(5),
        execute_request(&chat, with_imported_reference(&chat_request)?),
    )
    .await??;
    for events in [
        chat_events,
        timeout(
            Duration::from_secs(5),
            execute_request(
                &responses,
                with_imported_reference(&request(&responses, 2)?)?,
            ),
        )
        .await??,
        timeout(
            Duration::from_secs(5),
            execute_request(&messages, with_imported_reference(&request(&messages, 3)?)?),
        )
        .await??,
    ] {
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ModelEvent::ResponseCompleted(_)))
                .count(),
            1
        );
    }

    let requests = timeout(Duration::from_secs(5), peer).await???;
    let paths = requests
        .iter()
        .map(|request| {
            String::from_utf8_lossy(request)
                .lines()
                .next()
                .unwrap_or_default()
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        [
            "GET /api/v1/models HTTP/1.1",
            "GET /api/v1/models/vendor/model-v1%3Afree/endpoints HTTP/1.1",
            &format!("POST {OPENROUTER_CHAT_COMPLETIONS_PATH} HTTP/1.1"),
            &format!("POST {OPENROUTER_RESPONSES_PATH} HTTP/1.1"),
            &format!("POST {OPENROUTER_MESSAGES_PATH} HTTP/1.1"),
        ]
    );
    for request in &requests[2..] {
        let wire = String::from_utf8_lossy(request);
        for (name, value) in [
            ("http-referer", "https://github.com/N0xMare/plak"),
            ("x-openrouter-title", "Plak"),
            ("x-openrouter-categories", "cli-agent"),
        ] {
            assert_eq!(
                wire.matches(&format!("{name}: {value}\r\n")).count(),
                usize::from(attributed)
            );
            if !attributed {
                assert!(!wire.contains(name));
            }
        }
        assert_eq!(
            wire.matches("authorization: Bearer offline-loopback-key")
                .count(),
            1
        );
        assert!(wire.contains("\"model\":\"vendor/model-v1:free\""));
        let (_, body) = wire.split_once("\r\n\r\n").ok_or("request body")?;
        let body: serde_json::Value = serde_json::from_str(body)?;
        let items = if body.get("input").is_some() {
            &body["input"]
        } else {
            &body["messages"]
        };
        assert_eq!(items[0]["role"], "user");
        assert_eq!(items[1]["role"], "assistant");
        assert!(items[1]["content"].to_string().contains("work ledger"));
        assert!(!items[0]["content"].to_string().contains("work ledger"));
    }
    let chat_wire = String::from_utf8_lossy(&requests[2]);
    assert_eq!(
        chat_wire.matches("x-openrouter-metadata: enabled").count(),
        1
    );
    assert!(chat_wire.contains(
        "\"provider\":{\"allow_fallbacks\":false,\"require_parameters\":true,\"data_collection\":\"deny\",\"zdr\":true}"
    ));
    let chat_body = chat_wire
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .ok_or_else(|| io::Error::other("Chat request omitted its body"))?;
    let chat_body: serde_json::Value = serde_json::from_str(chat_body)?;
    assert!(chat_body.get("tools").is_none());
    let chat_messages = chat_body["messages"]
        .as_array()
        .ok_or_else(|| io::Error::other("Chat request omitted messages"))?;
    assert!(
        chat_messages
            .iter()
            .all(|message| { message.get("tool_calls").is_none() && message["role"] != "tool" })
    );
    let marker = "[Historical tool record; unavailable in the current target] ";
    let inert = chat_messages
        .iter()
        .filter(|message| message["role"] == "assistant")
        .filter_map(|message| message["content"].as_str())
        .filter(|content| content.starts_with(marker))
        .collect::<Vec<_>>();
    assert_eq!(inert.len(), 2);
    let inert = inert
        .into_iter()
        .map(|content| -> TestResult<serde_json::Value> {
            let record = content
                .strip_prefix(marker)
                .ok_or_else(|| io::Error::other("assistant history is not inert"))?;
            Ok(serde_json::from_str(record)?)
        })
        .collect::<TestResult<Vec<_>>>()?;
    assert_eq!(
        inert[0],
        serde_json::json!({
            "kind": "call",
            "name": "exec",
            "value": serde_json::to_string("text(\"retired exec history\");")?
        })
    );
    assert_eq!(
        inert[1],
        serde_json::json!({"kind": "result", "name": "exec", "value": "retired exec result"})
    );
    let messages_wire = String::from_utf8_lossy(&requests[4]);
    assert!(messages_wire.contains(
        "\"provider\":{\"allow_fallbacks\":false,\"require_parameters\":true,\"data_collection\":\"deny\",\"zdr\":true}"
    ));
    assert!(messages_wire.contains("\"stream\":true"));
    Ok(())
}

/// Consumes the discovery pair and returns the configured loopback chat lane.
async fn discovered_chat_model(
    client: Client,
    origin: String,
) -> TestResult<plakous_model::ConfiguredLanguageModel> {
    let source = Arc::new(StaticOpenRouterApiKeySource::new(
        OpenRouterApiKey::new("offline-loopback-key")?,
        "offline-account",
        "offline-authority",
    )?);
    let route = OpenRouterChatRoute::new_local_fixture_with_retention(
        source,
        OpenRouterOrigin::new(origin)?,
        plakous_openrouter::OpenRouterRetentionPolicy::strict(),
    )?
    .with_clock(Arc::new(review_clock()));
    let evidence = route
        .discover_chat_model(
            &client,
            MODEL,
            OpenRouterChatDiscoveryConfig {
                operation_timeout: Duration::from_secs(5),
                maximum_body_bytes: 64 * 1_024,
                evidence_ttl: Duration::from_secs(60),
            },
        )
        .await?;
    let chat_context = context(
        &route,
        CHAT_COMPLETIONS_FAMILY_ID,
        "openrouter-chat-custom-accounting-v1",
        "openrouter-chat-canonical-v1",
    )?;
    Ok(
        OpenRouterChatLanguageModel::configured_gateway_managed_behavior(
            route,
            evidence,
            chat_context,
            behavior("openrouter.chat.dynamic-v1", "openrouter-chat-retention-v1")?,
            client,
        )?,
    )
}

/// One chat SSE round whose only tool call names a tool outside the catalog.
fn chat_tool_call_sse(name: &str) -> String {
    let chunk = |choices: &str| {
        format!(
            "data: {{\"id\":\"chat-tool\",\"object\":\"chat.completion.chunk\",\"created\":1787623695,\"model\":\"{MODEL}\",\"choices\":{choices}}}\n\n"
        )
    };
    [
        chunk(r#"[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]"#),
        chunk(&format!(
            r#"[{{"index":0,"delta":{{"tool_calls":[{{"index":0,"id":"call-unadvertised","type":"function","function":{{"name":"{name}","arguments":"{{\"path\":\"notes.txt\"}}"}}}}]}},"finish_reason":null}}]"#
        )),
        chunk(r#"[{"index":0,"delta":{},"finish_reason":"tool_calls"}]"#),
        format!("data: {{\"id\":\"chat-tool\",\"object\":\"chat.completion.chunk\",\"created\":1787623695,\"model\":\"{MODEL}\",\"choices\":[],\"usage\":{{\"prompt_tokens\":4,\"completion_tokens\":1,\"total_tokens\":5}}}}\n\n"),
        "data: [DONE]\n\n".to_owned(),
    ]
    .concat()
}

/// A chat tool call outside the prepared catalog is a recoverable tool error.
///
/// The Chat stream projects the call exactly as the Messages and Responses
/// routes do; the tool runtime — not the wire — answers the model with
/// `UnknownTool`, so the turn keeps its next round instead of dying as an
/// unknown outcome.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unadvertised_chat_tool_call_becomes_an_unknown_tool_result_and_the_turn_completes()
-> TestResult {
    let mut replies = discovery_replies();
    replies.extend([
        (
            "text/event-stream",
            chat_tool_call_sse("not_a_tool"),
            "x-request-id: req-tool\r\n",
        ),
        (
            "text/event-stream",
            chat_sse(),
            "x-request-id: req-close\r\n",
        ),
    ]);
    let (client, origin, peer) = loopback(b"openrouter-unadvertised-tool-v1", replies).await?;
    let chat = discovered_chat_model(client, origin).await?;
    let workspace_dir = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(workspace_dir.path(), ".")?);
    let agent = AgentBuilder::new(chat, ToolRegistryBuilder::default().freeze(), workspace)
        .request_policy(
            AgentRequestPolicy::default()
                .with_generation_limits(GenerationLimits::new(NonZeroU32::new(64), Vec::new())?),
        )
        .identity_sources(
            Arc::new(review_clock()),
            Arc::new(SequenceIdGenerator::new(
                (100_u128..=400).map(Uuid::from_u128),
            )?),
        )
        .build()?;

    let mut turn = agent.prompt("read my notes").await?;
    let mut events = turn.take_events()?;
    let mut rejected = Vec::new();
    while let Ok(event) = events.recv().await {
        if let AgentEventKind::ToolPreparationRejected {
            attempt, reason, ..
        } = event.kind()
        {
            rejected.push((format!("{attempt:?}"), *reason));
        }
        if matches!(
            event.kind(),
            AgentEventKind::TurnCompleted { .. } | AgentEventKind::TurnFailed { .. }
        ) {
            break;
        }
    }
    assert_eq!(rejected.len(), 1, "rejections: {rejected:?}");
    assert!(rejected[0].0.contains("not_a_tool"), "{rejected:?}");
    assert_eq!(rejected[0].1, ToolRuntimeErrorKind::UnknownTool);

    let result = turn.result().await?;
    assert_eq!(result.model_rounds(), 2);
    assert_eq!(result.response().ok_or("response")?.output_text(), "chat");

    let requests = timeout(Duration::from_secs(5), peer).await???;
    assert_eq!(requests.len(), 4);
    let follow_up = String::from_utf8_lossy(&requests[3]);
    let body = follow_up
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .ok_or_else(|| io::Error::other("follow-up request omitted its body"))?;
    let body: serde_json::Value = serde_json::from_str(body)?;
    let history = body["messages"]
        .as_array()
        .ok_or_else(|| io::Error::other("follow-up request omitted messages"))?
        .iter()
        .filter_map(|message| message["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(history.contains("not_a_tool"), "history: {history}");
    assert!(history.contains("error: "), "history: {history}");
    agent.shutdown().await?;
    Ok(())
}

/// The chat SSE prefix a gateway sends before it gives up on its upstream:
/// framing only, no assistant content.
fn chat_sse_framing_then_in_band_error(code: u16) -> String {
    [
        format!(
            "data: {{\"id\":\"chat-inband\",\"object\":\"chat.completion.chunk\",\"created\":1787623695,\"model\":\"{MODEL}\",\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\"}},\"finish_reason\":null}}]}}\n\n"
        ),
        // The exact live payload shape: the edge relays its upstream's body and
        // the status appears only in the message. Nothing else is
        // machine-readable, which is why the status extraction reads it.
        format!("data: {{\"error\":{{\"message\":\"error code: {code}\"}}}}\n\n"),
    ]
    .concat()
}

/// An `OpenRouter` in-band HTTP 520 mid-exchange
/// escalated the `DeepSeek` CLI position to recovery-required and ended the case
/// `outcome_unknown`.
///
/// `OpenRouter` commits HTTP 200 at acceptance and reports an upstream failure
/// inside the stream it already committed to. The gateway named an overload or
/// rate limit before any content or tool call, so replaying the cost-only
/// request can only cost inference: the turn replays it and completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_in_band_gateway_overload_is_replayed_for_the_turn() -> TestResult {
    for code in [520_u16, 429] {
        let mut replies = discovery_replies();
        replies.extend([
            (
                "text/event-stream",
                chat_sse_framing_then_in_band_error(code),
                "x-request-id: req-inband\r\n",
            ),
            (
                "text/event-stream",
                chat_sse(),
                "x-request-id: req-after\r\n",
            ),
        ]);
        let (client, origin, peer) = loopback(b"openrouter-in-band-refusal-v1", replies).await?;
        let chat = discovered_chat_model(client, origin).await?;
        let workspace_dir = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(workspace_dir.path(), ".")?);
        let agent =
            AgentBuilder::new(chat, ToolRegistryBuilder::default().freeze(), workspace)
                .request_policy(AgentRequestPolicy::default().with_generation_limits(
                    GenerationLimits::new(NonZeroU32::new(64), Vec::new())?,
                ))
                .identity_sources(
                    Arc::new(review_clock()),
                    Arc::new(SequenceIdGenerator::new(
                        (100_u128..=400).map(Uuid::from_u128),
                    )?),
                )
                .build()?;

        let result = agent.prompt("hello").await?.result().await?;
        assert!(result.error().is_none(), "HTTP {code} in band: {result:?}");
        assert_eq!(result.response().ok_or("response")?.output_text(), "chat");
        agent.shutdown().await?;
        timeout(Duration::from_secs(5), peer).await???;
    }
    Ok(())
}

/// Chat buffers streamed tool calls until the terminal flush, so no tool-call
/// event reaches the turn before the cut. Its incomplete call is discarded;
/// only the complete replay can enter history.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stream_cut_after_a_local_tool_call_delta_is_replayed() -> TestResult {
    let tool_delta = format!(
        "data: {{\"id\":\"chat-inband\",\"object\":\"chat.completion.chunk\",\"created\":1787623695,\"model\":\"{MODEL}\",\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"call-1\",\"type\":\"function\",\"function\":{{\"name\":\"lookup\",\"arguments\":\"{{\"}}}}]}},\"finish_reason\":null}}]}}\n\n"
    );
    let stream = chat_sse_framing_then_in_band_error(520).replacen(
        "data: {\"error\"",
        &format!("{tool_delta}data: {{\"error\""),
        1,
    );
    let mut replies = discovery_replies();
    replies.push(("text/event-stream", stream, "x-request-id: req-inband\r\n"));
    replies.push((
        "text/event-stream",
        chat_sse(),
        "x-request-id: req-replay\r\n",
    ));
    let (client, origin, peer) = loopback(b"openrouter-tool-delta-cut-v1", replies).await?;
    let chat = discovered_chat_model(client, origin).await?;
    let workspace_dir = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(workspace_dir.path(), ".")?);
    let agent = AgentBuilder::new(chat, ToolRegistryBuilder::default().freeze(), workspace)
        .request_policy(
            AgentRequestPolicy::default()
                .with_generation_limits(GenerationLimits::new(NonZeroU32::new(64), Vec::new())?),
        )
        .identity_sources(
            Arc::new(review_clock()),
            Arc::new(SequenceIdGenerator::new(
                (100_u128..=400).map(Uuid::from_u128),
            )?),
        )
        .build()?;

    let result = agent.prompt("hello").await?.result().await?;
    assert!(result.error().is_none(), "{:?}", result.error());
    agent.shutdown().await?;
    // Two discovery calls and both physical inference requests.
    assert_eq!(timeout(Duration::from_secs(5), peer).await???.len(), 4);
    Ok(())
}

/// Each generation's head budget starts at its own dispatch. Logical time is
/// paused, so a loaded host's TLS handshake does not spend the budget. The
/// first body is held open, logical time moves past that generation's budget,
/// and only then is the body finished. A budget counted from the first
/// dispatch is already spent when the replay is sent; a fresh one is not.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn chat_replay_wait_gets_a_fresh_generation_head_budget() -> TestResult {
    // Transport also checks its std::Instant deadline eagerly. Leave real-time
    // setup slack: a frozen Tokio instant plus 200 ms can already be expired
    // on a loaded host, causing an unsent retry before the held reply exists.
    const HEAD_BUDGET: Duration = Duration::from_secs(30);
    // A paused current-thread clock auto-advances to the next timer whenever
    // the runtime has only I/O left, which spends the TLS and head budgets
    // immediately. A live `spawn_blocking` task inhibits that auto-advance
    // (tokio's paused-clock contract), so logical time moves only at the
    // explicit `advance` calls below.
    let (_keep_clock, clock_held) = std::sync::mpsc::channel::<()>();
    tokio::task::spawn_blocking(move || {
        let _ = clock_held.recv();
    });
    let tool_delta = format!(
        "data: {{\"id\":\"chat-inband\",\"object\":\"chat.completion.chunk\",\"created\":1787623695,\"model\":\"{MODEL}\",\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"call-1\",\"type\":\"function\",\"function\":{{\"name\":\"lookup\",\"arguments\":\"{{\"}}}}]}},\"finish_reason\":null}}]}}\n\n"
    );
    let stream = chat_sse_framing_then_in_band_error(520).replacen(
        "data: {\"error\"",
        &format!("{tool_delta}data: {{\"error\""),
        1,
    );
    let mut replies = discovery_replies();
    replies.push(("text/event-stream", stream, "x-request-id: req-inband\r\n"));
    replies.push((
        "text/event-stream",
        chat_sse(),
        "x-request-id: req-replay\r\n",
    ));
    let release = Arc::new(Notify::new());
    let replies = replies
        .into_iter()
        .map(|(content_type, body, headers)| ("200 OK", content_type, body, headers))
        .collect();
    let (client, origin, peer) = loopback_with_status_holding(
        b"openrouter-tool-delta-cut-v1",
        replies,
        Some(HoldOneReply {
            // Discovery is the first two replies. This is the cut generation.
            index: 2,
            release: Arc::clone(&release),
        }),
    )
    .await?;
    let chat = discovered_chat_model(client, origin).await?;
    let workspace_dir = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(workspace_dir.path(), ".")?);
    let agent = AgentBuilder::new(chat, ToolRegistryBuilder::default().freeze(), workspace)
        .config(plakous_agent::AgentConfig::default().with_model_operation_timeout(HEAD_BUDGET))
        .request_policy(
            AgentRequestPolicy::default()
                .with_generation_limits(GenerationLimits::new(NonZeroU32::new(64), Vec::new())?),
        )
        .identity_sources(
            Arc::new(review_clock()),
            Arc::new(SequenceIdGenerator::new(
                (100_u128..=400).map(Uuid::from_u128),
            )?),
        )
        .build()?;

    let mut turn = agent.prompt("hello").await?;
    let mut events = turn.take_events()?;
    let mut held = false;
    let mut retries = 0;
    while let Ok(event) = events.recv().await {
        match event.kind().model_event() {
            Some(ModelEvent::ResponseStarted(_)) if !held => {
                held = true;
                // This is client-side acceptance, not merely a socket write.
                tokio::time::advance(HEAD_BUDGET + Duration::from_millis(50)).await;
                release.notify_one();
            }
            Some(ModelEvent::Retry(retry)) => {
                retries += 1;
                assert!(held, "the first response head was not accepted");
                assert_eq!(
                    retry.next_physical(),
                    2,
                    "unexpected extra replay: {retry:?}"
                );
                // The producer has parked on its backoff before the driver
                // relays this event on the current-thread runtime. Advance
                // that wait once, never time spent waiting on replay I/O.
                // The notice truncates jitter to milliseconds, and Tokio's
                // timer wheel rounds up to the next millisecond tick.
                tokio::time::advance(retry.delay() + Duration::from_millis(2)).await;
            }
            _ => {}
        }
        if matches!(
            event.kind(),
            AgentEventKind::TurnCompleted { .. } | AgentEventKind::TurnFailed { .. }
        ) {
            break;
        }
    }
    let result = turn.result().await?;
    assert!(
        held,
        "the first generation's body was not held past its head budget"
    );
    assert_eq!(retries, 1, "the cut generation must be replayed once");
    assert!(result.error().is_none(), "{:?}", result.error());
    agent.shutdown().await?;
    // Two discovery calls and both physical inference requests. The peer
    // finishing is the event; a wall-clock timeout races the paused clock.
    assert_eq!(peer.await??.len(), 4);
    Ok(())
}

/// A `401` carried inside an accepted HTTP 200 stream is post-acceptance by
/// construction, so it can never take the pre-acceptance authentication
/// reading: it is a definite protocol failure and provokes no redispatch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_in_band_401_is_post_acceptance_and_never_redispatched() -> TestResult {
    let mut replies = discovery_replies();
    replies.push((
        "text/event-stream",
        chat_sse_framing_then_in_band_error(401),
        "x-request-id: req-inband\r\n",
    ));
    let (client, origin, peer) = loopback(b"openrouter-in-band-401-v1", replies).await?;
    let chat = discovered_chat_model(client, origin).await?;
    let workspace_dir = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(workspace_dir.path(), ".")?);
    let agent = AgentBuilder::new(chat, ToolRegistryBuilder::default().freeze(), workspace)
        .request_policy(
            AgentRequestPolicy::default()
                .with_generation_limits(GenerationLimits::new(NonZeroU32::new(64), Vec::new())?),
        )
        .identity_sources(
            Arc::new(review_clock()),
            Arc::new(SequenceIdGenerator::new(
                (100_u128..=400).map(Uuid::from_u128),
            )?),
        )
        .build()?;

    let result = agent.prompt("hello").await?.result().await?;
    assert!(
        matches!(
            result.outcome(),
            plakous_agent::TurnResultOutcome::Failed(_)
        ),
        "an in-band 401 after acceptance fails the turn without fencing the session"
    );
    let error = result
        .error()
        .ok_or("failed turn error")?
        .model_error()
        .ok_or("model error")?;
    assert_eq!(error.kind(), ModelErrorKind::Protocol, "in-band 401");

    agent.shutdown().await?;
    let requests = timeout(Duration::from_secs(5), peer).await???;
    // Two discovery calls plus the one inference the turn owned: no
    // automatic redispatch followed the accepted failure.
    assert_eq!(requests.len(), 3);
    Ok(())
}

/// The field shape: an accepted stream of keep-alives, then one chunk with no
/// choices whose error object names HTTP 504 and the upstream that timed out.
///
/// It is the provider's timeout, read like that status before acceptance:
/// typed `Timeout` with its status on the error context, and the operator
/// text names the provider's error rather than a stream validation failure.
/// No retry rule replays an in-band deadline, so the one inference stands.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_in_band_504_is_the_providers_timeout_with_its_status() -> TestResult {
    let stream = format!(
        ": OPENROUTER PROCESSING\n\n: OPENROUTER PROCESSING\n\ndata: {{\"id\":\"gen-inband\",\"object\":\"chat.completion.chunk\",\"created\":1787623695,\"model\":\"{MODEL}\",\"provider\":\"Upstream\",\"choices\":[],\"error\":{{\"code\":504,\"message\":\"error code: 504\",\"metadata\":{{\"provider_name\":\"Upstream\"}}}}}}\n\n"
    );
    let mut replies = discovery_replies();
    replies.push(("text/event-stream", stream, "x-request-id: req-inband\r\n"));
    let (client, origin, peer) = loopback(b"openrouter-in-band-504-v1", replies).await?;
    let chat = discovered_chat_model(client, origin).await?;
    let workspace_dir = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(workspace_dir.path(), ".")?);
    let agent = AgentBuilder::new(chat, ToolRegistryBuilder::default().freeze(), workspace)
        .request_policy(
            AgentRequestPolicy::default()
                .with_generation_limits(GenerationLimits::new(NonZeroU32::new(64), Vec::new())?),
        )
        .identity_sources(
            Arc::new(review_clock()),
            Arc::new(SequenceIdGenerator::new(
                (100_u128..=400).map(Uuid::from_u128),
            )?),
        )
        .build()?;

    let result = agent.prompt("hello").await?.result().await?;
    let error = result
        .error()
        .ok_or("failed turn error")?
        .model_error()
        .ok_or("model error")?;
    assert_eq!(error.kind(), ModelErrorKind::Timeout);
    assert_eq!(
        error
            .context()
            .and_then(plakous_model::ModelErrorContext::http_status)
            .map(plakous_model::ModelHttpStatus::get),
        Some(504)
    );
    assert_eq!(
        error.message(),
        "OpenRouter Chat operation failed: OpenRouter returned an in-band error after \
         accepting the request: HTTP 504 (timeout) from Upstream [1 requests; last: timeout]"
    );

    agent.shutdown().await?;
    let requests = timeout(Duration::from_secs(5), peer).await???;
    assert_eq!(requests.len(), 3, "two discovery calls and one inference");
    Ok(())
}

/// A replay that ends on a status naming no refusal is summarized by that
/// status, never by the rate limit an earlier request met.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replay_ending_on_a_400_is_not_summarized_as_the_earlier_rate_limit() -> TestResult {
    let mut replies = status_replies();
    replies.push((
        "429 Too Many Requests",
        "application/json",
        "{\"error\":{\"code\":429,\"message\":\"Rate limit exceeded\"}}".to_owned(),
        "x-request-id: req-limited\r\nretry-after: 0\r\n",
    ));
    replies.push((
        "400 Bad Request",
        "application/json",
        "{\"error\":{\"code\":400,\"message\":\"Invalid request\"}}".to_owned(),
        "x-request-id: req-invalid\r\n",
    ));
    let (client, origin, peer) =
        loopback_with_status(b"openrouter-replay-ends-400-v1", replies).await?;
    let chat = discovered_chat_model(client, origin).await?;
    let workspace_dir = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(workspace_dir.path(), ".")?);
    let agent = AgentBuilder::new(chat, ToolRegistryBuilder::default().freeze(), workspace)
        .request_policy(
            AgentRequestPolicy::default()
                .with_generation_limits(GenerationLimits::new(NonZeroU32::new(64), Vec::new())?),
        )
        .identity_sources(
            Arc::new(review_clock()),
            Arc::new(SequenceIdGenerator::new(
                (100_u128..=400).map(Uuid::from_u128),
            )?),
        )
        .build()?;

    let result = agent.prompt("hello").await?.result().await?;
    let error = result
        .error()
        .ok_or("failed turn error")?
        .model_error()
        .ok_or("model error")?;
    assert_ne!(error.kind(), ModelErrorKind::RateLimited);
    assert_eq!(
        error
            .context()
            .and_then(plakous_model::ModelErrorContext::http_status)
            .map(plakous_model::ModelHttpStatus::get),
        Some(400)
    );
    assert!(
        error.message().ends_with("[2 requests; last: after send]"),
        "{}",
        error.message()
    );

    agent.shutdown().await?;
    assert_eq!(timeout(Duration::from_secs(5), peer).await???.len(), 4);
    Ok(())
}

/// A gateway that refuses before the stream reads the same way.
///
/// `520` through `527` are the statuses an edge invents for an upstream it
/// could not reach. Only `401`, `403`, `429`, `503` and `529` are treated as
/// definite rejections; every other status, `520` included, is an ambiguous
/// post-send. A turn retries a `429` to the policy budget before it fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pre_stream_gateway_refusal_is_definite_and_keeps_the_session() -> TestResult {
    for (status, code, expected) in [
        ("520 Origin Error", 520_u16, ModelErrorKind::Unavailable),
        ("429 Too Many Requests", 429, ModelErrorKind::RateLimited),
        ("401 Unauthorized", 401, ModelErrorKind::Authentication),
    ] {
        let mut replies = discovery_replies()
            .into_iter()
            .map(|(content_type, body, headers)| ("200 OK", content_type, body, headers))
            .collect::<Vec<_>>();
        let refusals = if code == 429 { 4 } else { 1 };
        for _ in 0..refusals {
            replies.push((
                status,
                "application/json",
                format!(
                    "{{\"error\":{{\"code\":{code},\"message\":\"Provider returned error\"}}}}"
                ),
                "x-request-id: req-prestream\r\nretry-after: 0\r\n",
            ));
        }
        replies.extend([(
            "200 OK",
            "text/event-stream",
            chat_sse(),
            "x-request-id: req-after\r\n",
        )]);
        let (client, origin, peer) =
            loopback_with_status(b"openrouter-pre-stream-refusal-v1", replies).await?;
        let chat = discovered_chat_model(client, origin).await?;
        let workspace_dir = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(workspace_dir.path(), ".")?);
        let agent =
            AgentBuilder::new(chat, ToolRegistryBuilder::default().freeze(), workspace)
                .request_policy(AgentRequestPolicy::default().with_generation_limits(
                    GenerationLimits::new(NonZeroU32::new(64), Vec::new())?,
                ))
                .identity_sources(
                    Arc::new(review_clock()),
                    Arc::new(SequenceIdGenerator::new(
                        (100_u128..=400).map(Uuid::from_u128),
                    )?),
                )
                .build()?;

        let result = agent.prompt("hello").await?.result().await?;
        let error = result
            .error()
            .ok_or("failed turn error")?
            .model_error()
            .ok_or("model error")?;
        assert_eq!(error.kind(), expected, "pre-stream HTTP {status}");
        assert_ne!(error.kind(), ModelErrorKind::OutcomeUnknown);

        let next = agent.prompt("again").await?.result().await?;
        assert_eq!(next.response().ok_or("response")?.output_text(), "chat");
        agent.shutdown().await?;
        timeout(Duration::from_secs(5), peer).await???;
    }
    Ok(())
}

/// The first turn's model error kind and the requests the loopback peer saw.
///
/// `settle` waits for the peer to serve every reply and close its listener
/// before the turn, so a request past the replies meets a refused connection.
async fn first_turn_error(
    replies: Vec<(&'static str, &'static str, String, &'static str)>,
    faults: Option<&str>,
    body_timeout: Option<Duration>,
    settle: bool,
) -> TestResult<(ModelErrorKind, usize)> {
    let (client, origin, peer) =
        loopback_with_status(b"openrouter-fault-injection-v1", replies).await?;
    let mut client = client.with_faults(faults.map(FaultPlan::parse).transpose()?);
    if let Some(body_timeout) = body_timeout {
        let mut limits = client.limits().clone();
        limits.response_body_timeout = body_timeout;
        client = client.with_limits(limits)?;
    }
    let chat = discovered_chat_model(client, origin).await?;
    let mut peer = Some(peer);
    let mut requests = 0;
    if settle {
        requests = timeout(Duration::from_secs(5), peer.take().ok_or("peer")?)
            .await???
            .len();
    }
    let workspace_dir = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(workspace_dir.path(), ".")?);
    let agent = AgentBuilder::new(chat, ToolRegistryBuilder::default().freeze(), workspace)
        .request_policy(
            AgentRequestPolicy::default()
                .with_generation_limits(GenerationLimits::new(NonZeroU32::new(64), Vec::new())?),
        )
        .identity_sources(
            Arc::new(review_clock()),
            Arc::new(SequenceIdGenerator::new(
                (100_u128..=400).map(Uuid::from_u128),
            )?),
        )
        .build()?;
    let result = agent.prompt("hello").await?.result().await?;
    let kind = result
        .error()
        .ok_or("failed turn error")?
        .model_error()
        .ok_or("model error")?
        .kind();
    agent.shutdown().await?;
    if let Some(peer) = peer {
        requests = timeout(Duration::from_secs(5), peer).await???.len();
    }
    Ok((kind, requests))
}

fn status_replies() -> Vec<(&'static str, &'static str, String, &'static str)> {
    discovery_replies()
        .into_iter()
        .map(|(content_type, body, headers)| ("200 OK", content_type, body, headers))
        .collect()
}

fn chat_fault(fault: &str) -> String {
    format!("path={OPENROUTER_CHAT_COMPLETIONS_PATH},{fault}")
}

/// An injected status never reaches the gateway, and the turn fails exactly as
/// it does when the gateway itself answers with that status and body.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn injected_statuses_classify_like_the_gateway_answering_them() -> TestResult {
    for (status, line, error_type, expected) in [
        (
            429_u16,
            "429 Too Many Requests",
            "rate_limit_error",
            Some(ModelErrorKind::RateLimited),
        ),
        (500, "500 Internal Server Error", "api_error", None),
        (
            502,
            "502 Bad Gateway",
            "api_error",
            Some(ModelErrorKind::Unavailable),
        ),
        (503, "503 Service Unavailable", "api_error", None),
        (529, "529 Overloaded", "overloaded_error", None),
    ] {
        let body = format!(
            r#"{{"type":"error","error":{{"type":"{error_type}","code":{status},"message":"injected by PLAK_TEST_FAULTS"}}}}"#
        );
        // A refusal, or a 5xx on this cost-only request, is retried to the
        // policy budget of four requests; `Retry-After: 0` asks for each retry
        // immediately.
        let sends = 4;
        let mut replies = status_replies();
        for _ in 0..sends {
            replies.push((line, "application/json", body.clone(), "retry-after: 0\r\n"));
        }
        let (real, real_requests) = first_turn_error(replies, None, None, false).await?;
        let (injected, requests) = first_turn_error(
            status_replies(),
            Some(&chat_fault(&format!("fault={status},retry-after=0"))),
            None,
            false,
        )
        .await?;
        assert_eq!(injected, real, "HTTP {status}");
        assert_eq!(
            (real_requests, requests),
            (2 + sends, 2),
            "HTTP {status}: physical requests"
        );
        if let Some(expected) = expected {
            assert_eq!(injected, expected, "HTTP {status}");
        }
    }
    Ok(())
}

/// A refused connection sent nothing, so a turn retries it after the
/// policy's backoff and completes on the next request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_connection_is_retried_and_the_turn_completes() -> TestResult {
    let mut replies = status_replies();
    replies.push((
        "200 OK",
        "text/event-stream",
        chat_sse(),
        "x-request-id: req-after-connect\r\n",
    ));
    let (client, origin, peer) =
        loopback_with_status(b"openrouter-connect-retry-v1", replies).await?;
    let client = client.with_faults(Some(FaultPlan::parse(&chat_fault("fault=connect,nth=1"))?));
    let chat = discovered_chat_model(client, origin).await?;
    let workspace_dir = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(workspace_dir.path(), ".")?);
    let agent = AgentBuilder::new(chat, ToolRegistryBuilder::default().freeze(), workspace)
        .request_policy(
            AgentRequestPolicy::default()
                .with_generation_limits(GenerationLimits::new(NonZeroU32::new(64), Vec::new())?),
        )
        .identity_sources(
            Arc::new(review_clock()),
            Arc::new(SequenceIdGenerator::new(
                (100_u128..=400).map(Uuid::from_u128),
            )?),
        )
        .build()?;
    let result = agent.prompt("hello").await?.result().await?;
    assert_eq!(result.response().ok_or("response")?.output_text(), "chat");
    agent.shutdown().await?;
    // Two discovery calls and the one inference that reached the gateway.
    assert_eq!(timeout(Duration::from_secs(5), peer).await???.len(), 3);
    Ok(())
}

/// Stream faults cut or pause a real accepted stream after its first event,
/// before any tool call. The cost-only turn replays it, and the turn
/// completes with the replay's output alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn injected_stream_faults_are_replayed_and_the_turn_completes() -> TestResult {
    for (fault, body_timeout) in [
        ("fault=reset,after-events=1,nth=1", None),
        ("fault=eof,after-events=1,nth=1", None),
        (
            "fault=stall,seconds=30,after-events=1,nth=1",
            Some(Duration::from_millis(250)),
        ),
    ] {
        let mut replies = status_replies();
        for request_id in [
            "x-request-id: req-fault\r\n",
            "x-request-id: req-replay\r\n",
        ] {
            replies.push(("200 OK", "text/event-stream", chat_sse(), request_id));
        }
        let (client, origin, peer) =
            loopback_with_status(b"openrouter-stream-replay-v1", replies).await?;
        let mut client = client.with_faults(Some(FaultPlan::parse(&chat_fault(fault))?));
        if let Some(body_timeout) = body_timeout {
            let mut limits = client.limits().clone();
            limits.response_body_timeout = body_timeout;
            client = client.with_limits(limits)?;
        }
        let chat = discovered_chat_model(client, origin).await?;
        let workspace_dir = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(workspace_dir.path(), ".")?);
        let agent =
            AgentBuilder::new(chat, ToolRegistryBuilder::default().freeze(), workspace)
                .request_policy(AgentRequestPolicy::default().with_generation_limits(
                    GenerationLimits::new(NonZeroU32::new(64), Vec::new())?,
                ))
                .identity_sources(
                    Arc::new(review_clock()),
                    Arc::new(SequenceIdGenerator::new(
                        (100_u128..=400).map(Uuid::from_u128),
                    )?),
                )
                .build()?;
        let result = agent.prompt("hello").await?.result().await?;
        assert!(result.error().is_none(), "{fault}: {result:?}");
        assert_eq!(
            result.response().ok_or("response")?.output_text(),
            "chat",
            "{fault}"
        );
        agent.shutdown().await?;
        // Two discovery calls, the cut inference and its replay.
        assert_eq!(
            timeout(Duration::from_secs(5), peer).await???.len(),
            4,
            "{fault}"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_terminal_retry_advice_and_quota_bodies_never_replay() -> TestResult {
    for (status, code, headers) in [
        (
            "429 Fixture",
            "rate_limit_exceeded",
            "retry-after: 0\r\nx-should-retry: false\r\n",
        ),
        ("429 Fixture", "insufficient_quota", "retry-after: 0\r\n"),
        ("429 Fixture", "payment_required", "retry-after: 0\r\n"),
        (
            "400 Fixture",
            "rate_limit_exceeded",
            "x-should-retry: true\r\n",
        ),
    ] {
        let mut replies = discovery_replies()
            .into_iter()
            .map(|(content_type, body, header)| ("200 OK", content_type, body, header))
            .collect::<Vec<_>>();
        replies.push((
            status,
            "application/json",
            format!(r#"{{"error":{{"code":"{code}","message":"fixture refusal"}}}}"#),
            headers,
        ));
        let (client, origin, peer) = loopback_with_status(b"chat-terminal-quota", replies).await?;
        let chat = discovered_chat_model(client, origin).await?;
        let workspace_dir = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(workspace_dir.path(), ".")?);
        let agent =
            AgentBuilder::new(chat, ToolRegistryBuilder::default().freeze(), workspace)
                .request_policy(AgentRequestPolicy::default().with_generation_limits(
                    GenerationLimits::new(NonZeroU32::new(64), Vec::new())?,
                ))
                .identity_sources(
                    Arc::new(review_clock()),
                    Arc::new(SequenceIdGenerator::new(
                        (100_u128..=400).map(Uuid::from_u128),
                    )?),
                )
                .build()?;
        let mut turn = agent.prompt("hello").await?;
        let mut events = turn.take_events()?;
        while let Ok(event) = events.recv().await {
            assert!(
                !matches!(event.kind().model_event(), Some(ModelEvent::Retry(_))),
                "{code} must be terminal"
            );
            if matches!(
                event.kind(),
                AgentEventKind::TurnCompleted { .. } | AgentEventKind::TurnFailed { .. }
            ) {
                break;
            }
        }
        assert!(turn.result().await?.error().is_some());
        agent.shutdown().await?;
        assert_eq!(timeout(Duration::from_secs(5), peer).await???.len(), 3);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_terminal_retry_advice_and_quota_on_success_heads_suppress_stream_replay() -> TestResult
{
    for (stream, headers) in [
        (
            chat_sse_framing_then_in_band_error(520),
            "x-request-id: req-terminal\r\nx-should-retry: false\r\n",
        ),
        (
            chat_sse_framing_then_in_band_error(429)
                .replace("\"error\":{", "\"error\":{\"type\":\"insufficient_quota\","),
            "x-request-id: req-quota\r\n",
        ),
    ] {
        let mut replies = discovery_replies();
        replies.push(("text/event-stream", stream, headers));
        let (client, origin, peer) = loopback(b"chat-terminal-stream", replies).await?;
        let chat = discovered_chat_model(client, origin).await?;
        let workspace_dir = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(workspace_dir.path(), ".")?);
        let agent =
            AgentBuilder::new(chat, ToolRegistryBuilder::default().freeze(), workspace)
                .request_policy(AgentRequestPolicy::default().with_generation_limits(
                    GenerationLimits::new(NonZeroU32::new(64), Vec::new())?,
                ))
                .identity_sources(
                    Arc::new(review_clock()),
                    Arc::new(SequenceIdGenerator::new(
                        (100_u128..=400).map(Uuid::from_u128),
                    )?),
                )
                .build()?;
        let mut turn = agent.prompt("hello").await?;
        let mut events = turn.take_events()?;
        while let Ok(event) = events.recv().await {
            assert!(!matches!(
                event.kind().model_event(),
                Some(ModelEvent::Retry(retry)) if retry.loss().declined_reason.is_none()
            ));
            if matches!(
                event.kind(),
                AgentEventKind::TurnCompleted { .. } | AgentEventKind::TurnFailed { .. }
            ) {
                break;
            }
        }
        assert!(turn.result().await?.error().is_some());
        agent.shutdown().await?;
        assert_eq!(timeout(Duration::from_secs(5), peer).await???.len(), 3);
    }
    Ok(())
}

struct WorkflowKeySource {
    inner: StaticOpenRouterApiKeySource,
    physical_keys: std::sync::atomic::AtomicUsize,
    revoked: std::sync::atomic::AtomicBool,
}

impl plakous_openrouter::OpenRouterApiKeySource for WorkflowKeySource {
    fn credential_binding(&self) -> Option<&plakous_openrouter::OpenRouterCredentialBinding> {
        self.inner.credential_binding()
    }

    fn current_generation_grant(
        &self,
    ) -> Result<
        plakous_openrouter::OpenRouterApiKeyGenerationGrant,
        plakous_openrouter::OpenRouterAuthError,
    > {
        if self.revoked.load(std::sync::atomic::Ordering::SeqCst)
            && self.physical_keys.load(std::sync::atomic::Ordering::SeqCst) > 0
        {
            return Err(plakous_openrouter::OpenRouterAuthError::CredentialSource);
        }
        self.inner.current_generation_grant()
    }

    fn resolve(
        &self,
        request: &plakous_openrouter::OpenRouterCredentialRequest<'_>,
    ) -> Result<OpenRouterApiKey, plakous_openrouter::OpenRouterAuthError> {
        self.physical_keys
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.resolve(request)
    }

    fn resolve_chat(
        &self,
        request: &plakous_openrouter::OpenRouterChatCredentialRequest<'_>,
    ) -> Result<OpenRouterApiKey, plakous_openrouter::OpenRouterAuthError> {
        self.physical_keys
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.resolve_chat(request)
    }

    fn resolve_messages(
        &self,
        request: &plakous_openrouter::OpenRouterMessagesCredentialRequest<'_>,
    ) -> Result<OpenRouterApiKey, plakous_openrouter::OpenRouterAuthError> {
        assert_eq!(
            request.physical_binding().generation() as usize,
            self.physical_keys
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1
        );
        self.inner.resolve_messages(request)
    }

    fn resolve_catalog(
        &self,
        request: &plakous_openrouter::OpenRouterCatalogCredentialRequest<'_>,
    ) -> Result<OpenRouterApiKey, plakous_openrouter::OpenRouterAuthError> {
        self.inner.resolve_catalog(request)
    }
}

async fn execute_workflow_with_retry_revocation(
    configured: &plakous_model::ConfiguredLanguageModel,
    source: &WorkflowKeySource,
    revoke: bool,
) -> TestResult<Vec<Result<ModelEvent, ModelError>>> {
    let request = request_rooted(configured, 1, true)?;
    let attempt = request.metadata().attempt_id();
    let plan = configured
        .registered()
        .descriptor()
        .validate_request(request, ValidationMode::Strict)?;
    let model = configured.model();
    let authority = model
        .clone()
        .revalidate_runtime_authority(review_clock().now())
        .await?;
    let prepared = model.prepare_call(
        plan,
        ModelPreparationContext::new(authority, review_clock().now()),
    )?;
    let mut stream = prepared
        .start(ModelCallContext::new(
            attempt,
            plakous_model::CancellationToken::new(),
            None,
        ))
        .await?;
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        if matches!(event, Ok(ModelEvent::Retry(_))) && revoke {
            source
                .revoked
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        events.push(event);
    }
    Ok(events)
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "qualifies the same revalidation fence on all three gateway kernels"
)]
async fn workflow_unsent_retry_revalidates_authorization_on_every_wire() -> TestResult {
    for family in [
        CHAT_COMPLETIONS_FAMILY_ID,
        MESSAGES_FAMILY_ID,
        "responses-family",
    ] {
        for refuse_retry in [false, true] {
            let mut replies = status_replies();
            let (path, body) = match family {
                CHAT_COMPLETIONS_FAMILY_ID => (OPENROUTER_CHAT_COMPLETIONS_PATH, chat_sse()),
                MESSAGES_FAMILY_ID => (OPENROUTER_MESSAGES_PATH, messages_sse().to_owned()),
                _ => (OPENROUTER_RESPONSES_PATH, responses_sse()),
            };
            replies.push((
                "200 OK",
                "text/event-stream",
                body,
                "x-request-id: req-workflow\r\n",
            ));
            let (client, origin, mut peer) =
                loopback_with_status(b"workflow-revalidation", replies).await?;
            let client = client.with_faults(Some(FaultPlan::parse(&format!(
                "path={path},fault=connect,nth=1"
            ))?));
            let source = Arc::new(WorkflowKeySource {
                inner: StaticOpenRouterApiKeySource::new(
                    OpenRouterApiKey::new("offline-loopback-key")?,
                    "offline-account",
                    "offline-authority",
                )?,
                physical_keys: std::sync::atomic::AtomicUsize::new(0),
                revoked: std::sync::atomic::AtomicBool::new(false),
            });
            let route = OpenRouterChatRoute::new_local_fixture_with_retention(
                source.clone(),
                OpenRouterOrigin::new(origin)?,
                plakous_openrouter::OpenRouterRetentionPolicy::strict(),
            )?
            .with_clock(Arc::new(review_clock()));
            let evidence = route
                .discover_chat_model(
                    &client,
                    MODEL,
                    OpenRouterChatDiscoveryConfig {
                        operation_timeout: Duration::from_secs(5),
                        maximum_body_bytes: 64 * 1_024,
                        evidence_ttl: Duration::from_secs(60),
                    },
                )
                .await?;
            let configured = match family {
                CHAT_COMPLETIONS_FAMILY_ID => {
                    OpenRouterChatLanguageModel::configured_gateway_managed_behavior(
                        route.clone(),
                        evidence,
                        context(
                            &route,
                            family,
                            "openrouter-chat-custom-accounting-v1",
                            "openrouter-chat-canonical-v1",
                        )?,
                        behavior("openrouter.chat.dynamic-v1", "openrouter-chat-retention-v1")?,
                        client,
                    )?
                }
                MESSAGES_FAMILY_ID => {
                    OpenRouterMessagesLanguageModel::configured_gateway_managed_behavior(
                        route.clone(),
                        evidence,
                        context(
                            &route,
                            family,
                            "openrouter-messages-wire-v1",
                            "openrouter-messages-canonical-v2",
                        )?,
                        behavior(
                            "openrouter.messages.dynamic-custom-v1",
                            "openrouter-messages-integration-v1",
                        )?,
                        client,
                    )?
                }
                _ => OpenRouterResponsesLanguageModel::configured_gateway_managed_behavior(
                    route.clone(),
                    evidence,
                    context(
                        &route,
                        family,
                        "responses-wire-bytes-openrouter-v1",
                        "responses-canonical-openrouter-v1",
                    )?,
                    behavior(
                        "openrouter.responses.dynamic-custom-v1",
                        "openrouter-responses-integration-v1",
                    )?,
                    client,
                )?,
            };
            // A hang guard only: each case includes a real retry delay, and a
            // loaded gate runner has pushed one case past 5 s.
            let events = timeout(
                Duration::from_secs(30),
                execute_workflow_with_retry_revocation(&configured, &source, refuse_retry),
            )
            .await??;
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, Ok(ModelEvent::Retry(_))))
                    .count(),
                1
            );
            assert_eq!(
                events
                    .iter()
                    .any(|event| matches!(event, Ok(ModelEvent::ResponseCompleted(_)))),
                !refuse_retry
            );
            assert_eq!(events.iter().any(Result::is_err), refuse_retry);
            assert_eq!(
                source
                    .physical_keys
                    .load(std::sync::atomic::Ordering::SeqCst),
                if refuse_retry { 1 } else { 2 },
                "{family}"
            );
            if refuse_retry {
                assert!(
                    timeout(Duration::from_millis(100), &mut peer)
                        .await
                        .is_err()
                );
                peer.abort();
                let result = peer.await;
                assert!(matches!(result, Err(error) if error.is_cancelled()));
            } else {
                assert_eq!(timeout(Duration::from_secs(5), peer).await???.len(), 3);
            }
        }
    }
    Ok(())
}

#[allow(clippy::expect_used, reason = "fixed review-window fixture")]
fn review_clock() -> plakous_model::FixedClock {
    plakous_model::FixedClock::new(
        Timestamp::new(
            plakous_openrouter::OPENROUTER_CHAT_REVIEW_EXPIRES_AT - 30 * 86_400 + 60,
            0,
        )
        .expect("review time"),
    )
}
