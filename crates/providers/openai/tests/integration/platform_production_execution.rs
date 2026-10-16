//! The official Platform model and credential path exercised against explicit loopback TLS.
use plakous_http::{Client, Limits, TlsConfigIdentity};
use plakous_model::{
    Actor, AttemptId, CachePolicy, Clock, ContentBlock, ContextAdmissionRequirement,
    ConversationItem, ConversationView, CredentialSelectionId, Digest, DigestDomain,
    EffectiveProviderConfigurationId, EntryId, GenerationLimits, LocalDurabilityDisposition,
    ModelAdapterProfileId, ModelPreparationContext, ModelRequest, NativeOptions,
    OutputBoundDisposition, OutputPolicy, ParallelToolDisposition, ProtocolId, Provenance,
    ProviderAuthorityClassId, ProviderBehaviorConfiguration, ProviderProfileRevisionId,
    ProviderRouteSelection, ProviderStorageDisposition, ReasoningEffortDisposition,
    ReasoningIncludeDisposition, ReasoningSummaryDisposition, ReplayDisposition, RequestMetadata,
    ResponseStateDisposition, RetentionDisposition, RouteId, RunId, SelectedProviderConfiguration,
    SelectedProviderConfigurationId, SystemClock, TextBlock, ToolChoice, TurnId, ValidationMode,
};
use plakous_openai::{
    CredentialAuthorityId, CredentialReference, OPENAI_PLATFORM_AUTH_PROFILE_ID,
    OPENAI_PLATFORM_AUTH_REVISION, OPENAI_PLATFORM_AUTHORITY_CLASS,
    OPENAI_PLATFORM_CREDENTIAL_SELECTION, OPENAI_RESPONSES_ADAPTER_REVISION, OpenAiPlatformModel,
    OpenAiPlatformModelBuilder, OpenAiPlatformModelFacts, OpenAiProjectId, PlatformProjectPolicy,
    PlatformStoragePolicy, SecretString,
};
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use std::error::Error;
use std::io;
use std::num::NonZeroU32;
use std::str::FromStr as _;
use std::sync::Arc;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

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

use plakous_model::{
    ModelCallContext, ModelError, ModelErrorKind, ModelErrorPhase, ModelEvent, ModelHttpStatus,
    ModelRetryRisk, ModelRetryability, ReasoningEffort,
};
use plakous_openai::{OpenAiPlatformConfigurationAuthority, StaticPlatformCredentialSource};
use std::future::poll_fn;

fn selected_configuration(
    authority: &OpenAiPlatformConfigurationAuthority,
    effort: ReasoningEffort,
) -> TestResult<SelectedProviderConfiguration> {
    let descriptor = authority.descriptor();
    let revision = ProviderProfileRevisionId::new(OPENAI_RESPONSES_ADAPTER_REVISION)?;
    selected_with_behavior(
        authority,
        ProviderBehaviorConfiguration {
            processing_tier: plakous_model::ProcessingTierDisposition::Standard,
            tool_mode: plakous_model::ToolPresentationMode::Direct,
            reasoning_effort: ReasoningEffortDisposition::Enabled(effort),
            reasoning_summary: ReasoningSummaryDisposition::Auto,
            reasoning_include: ReasoningIncludeDisposition::EncryptedContent,
            replay: ReplayDisposition::StrictEncrypted,
            cache: CachePolicy::Disabled,
            tools: Vec::new(),
            hosted_tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            parallel_tools: ParallelToolDisposition::Disabled,
            output: OutputPolicy::default(),
            output_bound: OutputBoundDisposition::Enforced(
                NonZeroU32::new(128).ok_or_else(|| io::Error::other("nonzero bound"))?,
            ),
            storage: ProviderStorageDisposition::DisabledOnWire,
            retention: RetentionDisposition::RouteContract {
                profile: descriptor.profile().clone(),
                revision,
            },
            response_state: ResponseStateDisposition::Stateless,
            context_evidence_requirement: ContextAdmissionRequirement::AnyBounded,
            local_durability: LocalDurabilityDisposition::DurableRequired,
        },
    )
}

fn selected_with_behavior(
    authority: &OpenAiPlatformConfigurationAuthority,
    behavior: ProviderBehaviorConfiguration,
) -> TestResult<SelectedProviderConfiguration> {
    let descriptor = authority.descriptor();
    let revision = ProviderProfileRevisionId::new(OPENAI_RESPONSES_ADAPTER_REVISION)?;
    Ok(SelectedProviderConfiguration::new(
        ProviderRouteSelection::new(
            ProtocolId::new(plakous_openai::OPENAI_RESPONSES_PROTOCOL_ID)?,
            RouteId::new(plakous_openai::OPENAI_PLATFORM_ROUTE_ID)?,
            descriptor.profile().clone(),
            revision.clone(),
            ModelAdapterProfileId::new(OPENAI_PLATFORM_AUTH_PROFILE_ID)?,
            ProviderProfileRevisionId::new(OPENAI_PLATFORM_AUTH_REVISION)?,
            ProviderAuthorityClassId::new(OPENAI_PLATFORM_AUTHORITY_CLASS)?,
            CredentialSelectionId::new(OPENAI_PLATFORM_CREDENTIAL_SELECTION)?,
            descriptor.requested_model().clone(),
        ),
        behavior,
    )?)
}

fn request(
    selected: SelectedProviderConfigurationId,
    effective: EffectiveProviderConfigurationId,
    effort: ReasoningEffort,
) -> TestResult<ModelRequest> {
    request_with_bound(selected, effective, effort, NonZeroU32::new(128))
}

fn request_with_bound(
    selected: SelectedProviderConfigurationId,
    effective: EffectiveProviderConfigurationId,
    effort: ReasoningEffort,
    output_bound: Option<NonZeroU32>,
) -> TestResult<ModelRequest> {
    request_with_options(
        selected,
        effective,
        effort,
        output_bound,
        Vec::new(),
        NativeOptions::default(),
    )
}

fn request_with_options(
    selected: SelectedProviderConfigurationId,
    effective: EffectiveProviderConfigurationId,
    effort: ReasoningEffort,
    output_bound: Option<NonZeroU32>,
    stop_sequences: Vec<String>,
    native: NativeOptions,
) -> TestResult<ModelRequest> {
    let input = ConversationItem::new(
        EntryId::from_str("00000000-0000-4000-8000-000000000110")?,
        Actor::User,
        vec![ContentBlock::Text(TextBlock::new(
            "Dispatch the local fixture exactly once.",
            None,
        )?)],
        Provenance::UserInput,
    )?;
    Ok(ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![input])?,
        RequestMetadata::for_turn(
            RunId::from_str("00000000-0000-4000-8000-000000000101")?,
            TurnId::from_str("00000000-0000-4000-8000-000000000102")?,
            AttemptId::from_str("00000000-0000-4000-8000-000000000103")?,
            SystemClock.now(),
        ),
    )
    .reasoning(plakous_model::ReasoningPolicy::Enabled {
        effort: Some(effort),
        requirement: plakous_model::CapabilityRequirement::Required,
    })
    .limits(GenerationLimits::new(output_bound, stop_sequences)?)
    .context_admission_requirement(ContextAdmissionRequirement::AnyBounded)
    .resolved_harness_plan_identity(Digest::sha256(
        DigestDomain::ResolvedHarnessPlan,
        1,
        &[b"openai-platform-local-harness-v1"],
    )?)
    .native(native)
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

type FixtureEndpoint = (
    Client,
    plakous_http::Origin,
    tokio::task::JoinHandle<io::Result<Vec<u8>>>,
);
async fn endpoint(response: Vec<u8>) -> TestResult<FixtureEndpoint> {
    endpoint_mode(response, false).await
}
async fn endpoint_mode(response: Vec<u8>, stall: bool) -> TestResult<FixtureEndpoint> {
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
        TlsConfigIdentity::from_parts(1, &[b"openai-platform-local-tls-v1", certificate.as_ref()])?,
    )?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let origin = plakous_http::Origin::parse(&format!("https://{}/", listener.local_addr()?))?;
    let acceptor = TlsAcceptor::from(Arc::new(server_config));

    let peer = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut stream = acceptor.accept(stream).await?;
        let request = read_wire_request(&mut stream).await?;
        stream.write_all(&response).await?;
        if stall {
            let mut byte = [0_u8; 1];
            let _closed = stream.read(&mut byte).await;
        } else {
            stream.shutdown().await?;
        }
        Ok(request)
    });
    Ok((client, origin, peer))
}
fn authority(facts: OpenAiPlatformModelFacts) -> TestResult<OpenAiPlatformConfigurationAuthority> {
    authority_with_key(facts, "synthetic-platform-key")
}

fn authority_with_key(
    facts: OpenAiPlatformModelFacts,
    key: &str,
) -> TestResult<OpenAiPlatformConfigurationAuthority> {
    let reference = CredentialReference::application("platform-production-path-fixture")?;
    let source = Arc::new(StaticPlatformCredentialSource::new(
        SecretString::new(key)?,
        reference.clone(),
        CredentialAuthorityId::new("platform-production-path-test")?,
    ));
    Ok(OpenAiPlatformModelBuilder::new(
        reference,
        source,
        facts,
        PlatformStoragePolicy::Stateless,
        PlatformProjectPolicy::Project(OpenAiProjectId::new("proj_test")?),
    )
    .context_envelope(std::num::NonZeroU64::new(65536).ok_or("nonzero")?)
    .seal()?)
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
            serde_json::json!({"type":"response.output_text.done","output_index":0,"content_index":0,"text":"ready"}),
            serde_json::json!({"type":"response.content_part.done","output_index":0,"content_index":0,"part":text}),
            serde_json::json!({"type":"response.output_item.done","output_index":0,"item":message}),
            terminal(model, serde_json::json!([message])),
        ]);
    }
    let body =
        "{\"error\":{\"type\":\"invalid_request_error\",\"message\":\"synthetic rejection\"}}";
    format!("HTTP/1.1 {status} Fixture\r\ncontent-type: application/json\r\nx-request-id: req_test\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).into_bytes()
}
async fn start_model(
    model: Arc<OpenAiPlatformModel>,
    effort: ReasoningEffort,
) -> TestResult<plakous_model::ModelStream> {
    let configured = model.configured().await?;
    let request = request(
        configured.configuration().selected_identity(),
        configured.configuration().effective_identity(),
        effort,
    )?;
    let attempt = request.metadata().attempt_id();
    let plan = configured
        .registered()
        .descriptor()
        .validate_request(request, ValidationMode::Strict)?;
    let model = configured.model();
    let runtime = model
        .clone()
        .revalidate_runtime_authority(SystemClock.now())
        .await?;
    let prepared = model.prepare_call(
        plan,
        ModelPreparationContext::new(runtime, SystemClock.now()),
    )?;
    let stream = prepared
        .start(ModelCallContext::new(
            attempt,
            plakous_model::CancellationToken::new(),
            None,
        ))
        .await?;
    Ok(stream)
}
async fn execute(
    model: Arc<OpenAiPlatformModel>,
    effort: ReasoningEffort,
) -> TestResult<Vec<Result<ModelEvent, ModelError>>> {
    let mut stream = start_model(model, effort).await?;
    let mut events = Vec::new();
    while let Some(event) = poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
        events.push(event);
    }
    Ok(events)
}

#[tokio::test]
async fn documented_low_medium_corpus_uses_real_platform_credentials_and_encrypted_replay_policy()
-> TestResult {
    for (facts, effort) in [
        (OpenAiPlatformModelFacts::Gpt56Luna, ReasoningEffort::Low),
        (OpenAiPlatformModelFacts::Gpt56Luna, ReasoningEffort::Medium),
        (OpenAiPlatformModelFacts::Gpt56Sol, ReasoningEffort::Low),
    ] {
        let (client, origin, peer) = endpoint(response(facts.model_id(), 200)).await?;
        let authority = authority(facts)?;
        let selected = selected_configuration(&authority, effort)?;
        let model = Arc::new(OpenAiPlatformModel::with_local_transport(
            authority, selected, client, origin,
        )?);
        let configured = model.configured().await?;
        let request = request(
            configured.configuration().selected_identity(),
            configured.configuration().effective_identity(),
            effort,
        )?;
        let events = execute_request(
            &configured,
            with_imported_reference(&request)?,
            None,
            plakous_model::CancellationToken::new(),
        )
        .await?;
        assert!(events.iter().all(Result::is_ok), "{events:?}");
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, Ok(ModelEvent::ResponseCompleted(_))))
                .count(),
            1
        );
        let completed = events
            .iter()
            .find_map(|event| match event {
                Ok(ModelEvent::ResponseCompleted(response)) => Some(response),
                _ => None,
            })
            .ok_or("completion")?;
        assert_eq!(completed.output_text(), "ready");
        assert_eq!(
            (completed.usage().ok_or("usage")?.input_tokens()
                + completed.usage().ok_or("usage")?.output_tokens()),
            12
        );
        let wire = peer.await??;
        let end = wire
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .ok_or("head")?;
        let head = String::from_utf8_lossy(&wire[..end]).to_ascii_lowercase();
        assert!(head.contains("authorization: bearer synthetic-platform-key"));
        assert!(head.contains("openai-project: proj_test"));
        assert!(!head.contains("chatgpt"));
        let body: serde_json::Value = serde_json::from_slice(&wire[end + 4..])?;
        assert_eq!(body["model"], facts.model_id());
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][1]["role"], "assistant");
        assert!(
            body["input"][1]["content"]
                .to_string()
                .contains("work ledger")
        );
        assert_eq!(body["store"], false);
        assert_eq!(
            body["include"],
            serde_json::json!(["reasoning.encrypted_content"])
        );
        assert_eq!(
            body["reasoning"]["effort"],
            if effort == ReasoningEffort::Low {
                "low"
            } else {
                "medium"
            }
        );
    }
    Ok(())
}
#[tokio::test]
async fn production_http_rejection_never_becomes_completion() -> TestResult {
    // A 429 is retried under the kernel policy; see the Responses host tests.
    for status in [401, 403] {
        let facts = OpenAiPlatformModelFacts::Gpt56Luna;
        let (client, origin, peer) = endpoint(response(facts.model_id(), status)).await?;
        let source = Arc::new(RotatingSource {
            authority: CredentialAuthorityId::new("http-rejection-source")?,
            rotated: std::sync::atomic::AtomicBool::new(false),
            resolutions: std::sync::atomic::AtomicUsize::new(0),
            grants: std::sync::atomic::AtomicUsize::new(0),
        });
        let authority = OpenAiPlatformModelBuilder::new(
            CredentialReference::application("http-rejection")?,
            source.clone(),
            facts,
            PlatformStoragePolicy::Stateless,
            PlatformProjectPolicy::DefaultProject,
        )
        .context_envelope(std::num::NonZeroU64::new(65536).ok_or("nonzero")?)
        .seal()?;
        let selected = selected_configuration(&authority, ReasoningEffort::Low)?;
        let model = Arc::new(OpenAiPlatformModel::with_local_transport(
            authority, selected, client, origin,
        )?);
        let events = execute(model, ReasoningEffort::Low).await?;
        let errors: Vec<_> = events
            .iter()
            .filter_map(|event| event.as_ref().err())
            .collect();
        assert_eq!(errors.len(), 1, "{events:?}");
        let error = errors[0];
        assert_eq!(
            error.kind(),
            if status == 401 {
                ModelErrorKind::Authentication
            } else {
                ModelErrorKind::Authorization
            }
        );
        let context = error.context().ok_or("HTTP error context")?;
        assert_eq!(context.phase(), ModelErrorPhase::FirstEvent);
        assert_eq!(
            context.http_status().map(ModelHttpStatus::get),
            Some(status)
        );
        // Authentication rejection is definite; a 403 retains post-send uncertainty.
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
        assert!(matches!(events.last(), Some(Err(_))));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Ok(ModelEvent::ResponseStarted(_))))
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Ok(ModelEvent::ResponseCompleted(_))))
        );
        peer.await??;
        assert_eq!(
            source.resolutions.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }
    Ok(())
}

fn sse_response(events: Vec<serde_json::Value>) -> Vec<u8> {
    let mut body = String::new();
    for (index, mut event) in events.into_iter().enumerate() {
        event["sequence_number"] = serde_json::json!(index);
        body.push_str("data: ");
        body.push_str(&event.to_string());
        body.push_str("\n\n");
    }
    format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).into_bytes()
}
fn created(model: &str) -> serde_json::Value {
    serde_json::json!({"type":"response.created","response":{"id":"resp_test","object":"response","status":"in_progress","model":model}})
}
fn terminal(model: &str, output: serde_json::Value) -> serde_json::Value {
    let mut terminal = serde_json::json!({"type":"response.completed","response":{"id":"resp_test","object":"response","status":"completed","model":model,"output":[],"usage":{"input_tokens":8,"output_tokens":4,"total_tokens":12}}});
    terminal["response"]["output"] = output;
    terminal
}
#[tokio::test(start_paused = true)]
async fn accepted_truncation_wrong_model_and_stream_error_never_complete() -> TestResult {
    // Only observed retry waits advance time; real loopback I/O keeps its head budget.
    let (keep_clock, clock_held) = std::sync::mpsc::channel::<()>();
    let clock_task = tokio::task::spawn_blocking(move || {
        let _ = clock_held.recv();
    });
    let facts = OpenAiPlatformModelFacts::Gpt56Luna;
    for events in [
        vec![created(facts.model_id())],
        vec![
            created(facts.model_id()),
            terminal("foreign-model", serde_json::json!([])),
        ],
        vec![
            created(facts.model_id()),
            serde_json::json!({"type":"error","code":"server_error","message":"synthetic failure"}),
        ],
    ] {
        let (client, origin, peer) = endpoint(sse_response(events)).await?;
        let authority = authority(facts)?;
        let selected = selected_configuration(&authority, ReasoningEffort::Low)?;
        let model = Arc::new(OpenAiPlatformModel::with_local_transport(
            authority, selected, client, origin,
        )?);
        let mut stream = start_model(model, ReasoningEffort::Low).await?;
        let mut events = Vec::new();
        while let Some(event) = poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
            if let Ok(ModelEvent::Retry(retry)) = &event
                && retry.loss().declined_reason.is_none()
            {
                // Observation delays round nanosecond jitter down to whole milliseconds.
                tokio::time::advance(
                    retry
                        .delay()
                        .saturating_add(std::time::Duration::from_millis(1)),
                )
                .await;
            }
            events.push(event);
        }
        assert!(events.iter().any(Result::is_err), "{events:?}");
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Ok(ModelEvent::ResponseCompleted(_))))
        );
        let wire = peer.await??;
        assert!(
            wire.starts_with(b"POST "),
            "the malformed response followed a real request"
        );
    }
    drop(keep_clock);
    clock_task.await?;
    Ok(())
}

async fn execute_request(
    configured: &plakous_model::ConfiguredLanguageModel,
    request: ModelRequest,
    active: Option<plakous_model::ActiveFamilyArtifact>,
    cancellation: plakous_model::CancellationToken,
) -> TestResult<Vec<Result<ModelEvent, ModelError>>> {
    let attempt = request.metadata().attempt_id();
    let plan = configured
        .registered()
        .descriptor()
        .validate_request(request, ValidationMode::Strict)?;
    let model = configured.model();
    let runtime = model
        .clone()
        .revalidate_runtime_authority(SystemClock.now())
        .await?;
    let mut context = ModelPreparationContext::new(runtime, SystemClock.now());
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
fn tool() -> TestResult<plakous_model::ConfiguredTool> {
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
    configured: &plakous_model::ConfiguredLanguageModel,
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
            SystemClock.now(),
        ),
    )
    .tools(vec![tool()?])
    .reasoning(plakous_model::ReasoningPolicy::Enabled {
        effort: Some(ReasoningEffort::Low),
        requirement: plakous_model::CapabilityRequirement::Required,
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
    reason = "one production exchange and reconstructed successor prove exact encrypted and tool-result replay together"
)]
async fn tool_result_and_encrypted_reasoning_replay_use_production_executor() -> TestResult {
    use plakous_model::{ToolExecutionOwner, ToolResultBlock, ToolResultKind, ToolResultPart};
    let facts = OpenAiPlatformModelFacts::Gpt56Luna;
    let reasoning = serde_json::json!({"type":"reasoning","id":"rs_test","status":"completed","summary":[],"encrypted_content":"synthetic-ciphertext"});
    let function = serde_json::json!({"type":"function_call","id":"fc_test","status":"completed","call_id":"call_test","name":"lookup","arguments":"{\"q\":\"fixture\"}"});
    let stream_events = vec![
        created(facts.model_id()),
        serde_json::json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_test","status":"in_progress","summary":[]}}),
        serde_json::json!({"type":"response.output_item.done","output_index":0,"item":reasoning}),
        serde_json::json!({"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc_test","status":"in_progress","call_id":"call_test","name":"lookup","arguments":""}}),
        serde_json::json!({"type":"response.function_call_arguments.delta","output_index":1,"item_id":"fc_test","delta":"{\"q\":\"fixture\"}"}),
        serde_json::json!({"type":"response.function_call_arguments.done","output_index":1,"item_id":"fc_test","arguments":"{\"q\":\"fixture\"}"}),
        serde_json::json!({"type":"response.output_item.done","output_index":1,"item":function}),
        terminal(facts.model_id(), serde_json::json!([reasoning, function])),
    ];
    let (client, origin, peer) = endpoint(sse_response(stream_events)).await?;
    let authority = authority(facts)?;
    let mut behavior = selected_configuration(&authority, ReasoningEffort::Low)?
        .behavior()
        .clone();
    behavior.tools = vec![tool()?];
    let selected = authority.selected_configuration(behavior)?;
    let configured = Arc::new(OpenAiPlatformModel::with_local_transport(
        authority.clone(),
        selected.clone(),
        client,
        origin,
    )?)
    .configured()
    .await?;
    let mut input = vec![ConversationItem::new(
        EntryId::from_str("00000000-0000-4000-8000-000000000201")?,
        Actor::User,
        vec![ContentBlock::Text(TextBlock::new("look up fixture", None)?)],
        Provenance::UserInput,
    )?];
    let events = execute_request(
        &configured,
        tool_request(&configured, input.clone(), 301)?,
        None,
        plakous_model::CancellationToken::new(),
    )
    .await?;
    assert!(events.iter().all(Result::is_ok), "{events:?}");
    let output = events
        .iter()
        .find_map(|event| {
            if let Ok(ModelEvent::ResponseCompleted(response)) = event {
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
        .find_map(|block| {
            if let ContentBlock::ToolCall(call) = block {
                Some(call)
            } else {
                None
            }
        })
        .ok_or("tool call")?;
    assert_eq!(call.arguments(), Some(&serde_json::json!({"q":"fixture"})));
    let candidate = output
        .family_artifact_candidate()
        .ok_or("managed artifact")?;
    let active = plakous_model::ActiveFamilyArtifact::new(
        candidate.family(),
        candidate.schema_revision(),
        candidate.codec_revision(),
        candidate.binding(),
        candidate.bytes().to_vec(),
    )?;
    let active: plakous_model::ActiveFamilyArtifact =
        serde_json::from_slice(&serde_json::to_vec(&active)?)?;
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
    // Explicit rebind retains the product account label but changes the key.
    // Neither the aggregate nor standalone encrypted item may cross that boundary.
    let changed_authority = authority_with_key(facts, "different-synthetic-key")?;
    let changed_selected = changed_authority.selected_configuration(selected.behavior().clone())?;
    let (client, origin, changed_peer) = endpoint(response(facts.model_id(), 200)).await?;
    let changed = Arc::new(OpenAiPlatformModel::with_local_transport(
        changed_authority,
        changed_selected,
        client,
        origin,
    )?)
    .configured()
    .await?;
    let result = execute_request(
        &changed,
        tool_request(&changed, input.clone(), 303)?,
        Some(active.clone()),
        plakous_model::CancellationToken::new(),
    )
    .await;
    assert!(
        result.is_err(),
        "foreign-key aggregate must fail preparation"
    );
    assert!(!changed_peer.is_finished());
    // Without the aggregate, canonical opaque fallback omits foreign items.
    // The ordinary canonical request still executes without the old ciphertext.
    let result = execute_request(
        &changed,
        tool_request(&changed, input.clone(), 304)?,
        None,
        plakous_model::CancellationToken::new(),
    )
    .await;
    assert!(result?.iter().all(Result::is_ok));
    let wire = changed_peer.await??;
    assert!(!String::from_utf8_lossy(&wire).contains("synthetic-ciphertext"));
    let (client, origin, peer) = endpoint(response(facts.model_id(), 200)).await?;
    // Reconstruct the immutable provider binding, as a durable reopen does.
    let reopened = Arc::new(OpenAiPlatformModel::with_local_transport(
        authority, selected, client, origin,
    )?)
    .configured()
    .await?;
    assert_eq!(configured.configuration(), reopened.configuration());
    let events = execute_request(
        &reopened,
        tool_request(&reopened, input, 302)?,
        Some(active),
        plakous_model::CancellationToken::new(),
    )
    .await?;
    assert!(events.iter().all(Result::is_ok), "{events:?}");
    let wire = peer.await??;
    let end = wire
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .ok_or("header")?;
    let body: serde_json::Value = serde_json::from_slice(&wire[end + 4..])?;
    let items = body["input"].as_array().ok_or("input")?;
    assert!(
        items
            .iter()
            .any(|item| item["encrypted_content"] == "synthetic-ciphertext")
    );
    assert!(
        items
            .iter()
            .any(|item| item["type"] == "function_call_output" && item["call_id"] == "call_test")
    );
    assert!(body.get("previous_response_id").is_none());
    Ok(())
}

struct RotatingSource {
    authority: CredentialAuthorityId,
    rotated: std::sync::atomic::AtomicBool,
    resolutions: std::sync::atomic::AtomicUsize,
    grants: std::sync::atomic::AtomicUsize,
}
impl plakous_openai::PlatformCredentialSource for RotatingSource {
    fn credential_authority(&self) -> &CredentialAuthorityId {
        &self.authority
    }
    fn api_key(
        &self,
        _reference: &CredentialReference,
        origin: &plakous_openai::EndpointOrigin,
    ) -> Result<SecretString, plakous_openai::AuthError> {
        if origin.as_str() != plakous_openai::OPENAI_PLATFORM_ORIGIN {
            return Err(plakous_openai::AuthError::BindingMismatch);
        }
        self.resolutions
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        SecretString::new(if self.rotated.load(std::sync::atomic::Ordering::SeqCst) {
            "second-key"
        } else {
            "first-key"
        })
        .map_err(|_| plakous_openai::AuthError::SourceFailure)
    }
    fn generation_grant(
        &self,
        _reference: &CredentialReference,
        origin: &plakous_openai::EndpointOrigin,
    ) -> Result<plakous_openai::OpenAiApiKeyGenerationGrant, plakous_openai::AuthError> {
        if origin.as_str() != plakous_openai::OPENAI_PLATFORM_ORIGIN {
            return Err(plakous_openai::AuthError::BindingMismatch);
        }
        self.grants
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        plakous_openai::OpenAiApiKeyGenerationGrant::from_generation_binding(
            if self.rotated.load(std::sync::atomic::Ordering::SeqCst) {
                b"second-key"
            } else {
                b"first-key"
            },
        )
        .map_err(|_| plakous_openai::AuthError::SourceFailure)
    }
}
#[tokio::test]
async fn generation_rotation_and_cancellation_reject_before_secret_or_connection() -> TestResult {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    for rotate in [false, true] {
        let facts = OpenAiPlatformModelFacts::Gpt56Luna;
        let (client, origin, peer) = endpoint(response(facts.model_id(), 200)).await?;
        let source = Arc::new(RotatingSource {
            authority: CredentialAuthorityId::new("rotating-test-source")?,
            rotated: AtomicBool::new(false),
            resolutions: AtomicUsize::new(0),
            grants: AtomicUsize::new(0),
        });
        let authority = OpenAiPlatformModelBuilder::new(
            CredentialReference::application("rotation-test")?,
            source.clone(),
            facts,
            PlatformStoragePolicy::Stateless,
            PlatformProjectPolicy::DefaultProject,
        )
        .context_envelope(std::num::NonZeroU64::new(65536).ok_or("bound")?)
        .seal()?;
        let selected = selected_configuration(&authority, ReasoningEffort::Low)?;
        let configured = Arc::new(OpenAiPlatformModel::with_local_transport(
            authority, selected, client, origin,
        )?)
        .configured()
        .await?;
        let request = request(
            configured.configuration().selected_identity(),
            configured.configuration().effective_identity(),
            ReasoningEffort::Low,
        )?;
        let attempt = request.metadata().attempt_id();
        let plan = configured
            .registered()
            .descriptor()
            .validate_request(request, ValidationMode::Strict)?;
        let model = configured.model();
        let runtime = model
            .clone()
            .revalidate_runtime_authority(SystemClock.now())
            .await?;
        let prepared = model.clone().prepare_call(
            plan,
            ModelPreparationContext::new(runtime, SystemClock.now()),
        )?;
        let cancellation = plakous_model::CancellationToken::new();
        if rotate {
            source.rotated.store(true, Ordering::SeqCst);
            assert!(
                model
                    .revalidate_runtime_authority(SystemClock.now())
                    .await
                    .is_err()
            );
        } else {
            cancellation.cancel();
        }
        let result = prepared
            .start(ModelCallContext::new(attempt, cancellation, None))
            .await;
        match result {
            Err(_) => {}
            Ok(mut stream) => {
                let mut failed = false;
                while let Some(event) = poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
                    assert!(!matches!(event, Ok(ModelEvent::ResponseCompleted(_))));
                    failed |= event.is_err();
                }
                assert!(failed);
            }
        }
        assert_eq!(source.resolutions.load(Ordering::SeqCst), 0);
        assert!(!peer.is_finished());
        peer.abort();
    }
    Ok(())
}

#[tokio::test]
async fn release_only_caller_envelope_refuses_before_credential_refresh_or_preparation()
-> TestResult {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let facts = OpenAiPlatformModelFacts::Gpt56Luna;
    let (client, origin, peer) = endpoint(response(facts.model_id(), 200)).await?;
    let source = Arc::new(RotatingSource {
        authority: CredentialAuthorityId::new("release-only-preflight-test")?,
        rotated: AtomicBool::new(false),
        resolutions: AtomicUsize::new(0),
        grants: AtomicUsize::new(0),
    });
    let authority = OpenAiPlatformModelBuilder::new(
        CredentialReference::application("release-only-test")?,
        source.clone(),
        facts,
        PlatformStoragePolicy::Stateless,
        PlatformProjectPolicy::DefaultProject,
    )
    .context_envelope(std::num::NonZeroU64::new(65536).ok_or("bound")?)
    .seal()?;
    assert_eq!(
        authority.descriptor().integration_class(),
        plakous_model::IntegrationClass::Standard
    );
    let mut behavior = selected_configuration(&authority, ReasoningEffort::Low)?
        .behavior()
        .clone();
    behavior.context_evidence_requirement = ContextAdmissionRequirement::ReleaseVerifiedRequired;
    let selected = authority.selected_configuration(behavior)?;
    assert!(
        OpenAiPlatformModel::with_local_transport(authority, selected, client, origin).is_err()
    );
    assert_eq!(source.grants.load(Ordering::SeqCst), 0);
    assert_eq!(source.resolutions.load(Ordering::SeqCst), 0);
    assert!(!peer.is_finished());
    peer.abort();
    Ok(())
}

#[tokio::test]
async fn disabled_reasoning_refuses_before_credential_refresh_or_preparation() -> TestResult {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let facts = OpenAiPlatformModelFacts::Gpt56Luna;
    let (client, origin, peer) = endpoint(response(facts.model_id(), 200)).await?;
    let source = Arc::new(RotatingSource {
        authority: CredentialAuthorityId::new("disabled-reasoning-preflight-test")?,
        rotated: AtomicBool::new(false),
        resolutions: AtomicUsize::new(0),
        grants: AtomicUsize::new(0),
    });
    let authority = OpenAiPlatformModelBuilder::new(
        CredentialReference::application("disabled-reasoning-test")?,
        source.clone(),
        facts,
        PlatformStoragePolicy::Stateless,
        PlatformProjectPolicy::DefaultProject,
    )
    .context_envelope(std::num::NonZeroU64::new(65536).ok_or("bound")?)
    .seal()?;
    assert_eq!(
        authority.descriptor().integration_class(),
        plakous_model::IntegrationClass::Standard
    );
    let mut behavior = selected_configuration(&authority, ReasoningEffort::Low)?
        .behavior()
        .clone();
    behavior.reasoning_effort = ReasoningEffortDisposition::Disabled;
    behavior.reasoning_summary = ReasoningSummaryDisposition::Omit;
    behavior.reasoning_include = ReasoningIncludeDisposition::Omit;
    let selected = authority.selected_configuration(behavior)?;
    assert!(
        OpenAiPlatformModel::with_local_transport(authority, selected, client, origin).is_err()
    );
    assert_eq!(source.grants.load(Ordering::SeqCst), 0);
    assert_eq!(source.resolutions.load(Ordering::SeqCst), 0);
    assert!(!peer.is_finished());
    peer.abort();
    Ok(())
}

#[tokio::test]
async fn reconstruction_binds_exact_key_generation_without_resolving_bearer() -> TestResult {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let source = Arc::new(RotatingSource {
        authority: CredentialAuthorityId::new("reconstruction-key-identity")?,
        rotated: AtomicBool::new(false),
        resolutions: AtomicUsize::new(0),
        grants: AtomicUsize::new(0),
    });
    let build = || -> TestResult<Arc<OpenAiPlatformModel>> {
        let authority = OpenAiPlatformModelBuilder::new(
            CredentialReference::application("stable-slot")?,
            source.clone(),
            OpenAiPlatformModelFacts::Gpt56Luna,
            PlatformStoragePolicy::Stateless,
            PlatformProjectPolicy::DefaultProject,
        )
        .context_envelope(std::num::NonZeroU64::new(65536).ok_or("bound")?)
        .seal()?;
        let selected = selected_configuration(&authority, ReasoningEffort::Low)?;
        Ok(Arc::new(OpenAiPlatformModel::new(authority, selected)?))
    };
    let first = build()?.configured().await?;
    let same_key = build()?.configured().await?;
    assert_eq!(first.configuration(), same_key.configuration());
    assert_eq!(
        first.initial_authority().account_binding(),
        same_key.initial_authority().account_binding()
    );
    source.rotated.store(true, Ordering::SeqCst);
    let changed_key = build()?.configured().await?;
    assert_eq!(
        first.configuration().selected_identity(),
        changed_key.configuration().selected_identity()
    );
    assert_ne!(
        first.configuration().effective_identity(),
        changed_key.configuration().effective_identity()
    );
    assert!(
        first
            .model()
            .revalidate_runtime_authority(SystemClock.now())
            .await
            .is_err()
    );
    assert_ne!(
        first.initial_authority().account_binding(),
        changed_key.initial_authority().account_binding()
    );
    assert_ne!(
        first.initial_authority().credential_authority_binding(),
        changed_key
            .initial_authority()
            .credential_authority_binding()
    );
    assert_eq!(source.resolutions.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn accepted_stream_cancellation_closes_transport_without_terminal_completion() -> TestResult {
    let facts = OpenAiPlatformModelFacts::Gpt56Luna;
    let mut created = created(facts.model_id());
    created["sequence_number"] = serde_json::json!(0);
    let body = format!("data: {created}\n\n");
    let response = format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 100000\r\nconnection: close\r\n\r\n{body}").into_bytes();
    let (client, origin, peer) = endpoint_mode(response, true).await?;
    let authority = authority(facts)?;
    let selected = selected_configuration(&authority, ReasoningEffort::Low)?;
    let configured = Arc::new(OpenAiPlatformModel::with_local_transport(
        authority, selected, client, origin,
    )?)
    .configured()
    .await?;
    let request = request(
        configured.configuration().selected_identity(),
        configured.configuration().effective_identity(),
        ReasoningEffort::Low,
    )?;
    let attempt = request.metadata().attempt_id();
    let plan = configured
        .registered()
        .descriptor()
        .validate_request(request, ValidationMode::Strict)?;
    let model = configured.model();
    let runtime = model
        .clone()
        .revalidate_runtime_authority(SystemClock.now())
        .await?;
    let prepared = model.prepare_call(
        plan,
        ModelPreparationContext::new(runtime, SystemClock.now()),
    )?;
    let cancellation = plakous_model::CancellationToken::new();
    let mut stream = prepared
        .start(ModelCallContext::new(attempt, cancellation.clone(), None))
        .await?;
    let mut accepted = false;
    let mut errors = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(event) = poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
            assert!(errors.is_empty(), "event followed the terminal error");
            assert!(!matches!(event, Ok(ModelEvent::ResponseCompleted(_))));
            if matches!(event, Ok(ModelEvent::ResponseStarted(_))) {
                assert!(!accepted, "duplicate response start");
                accepted = true;
                cancellation.cancel();
            }
            if let Err(error) = event {
                errors.push(error);
            }
        }
        peer.await
    })
    .await???;
    assert!(accepted);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].kind(), ModelErrorKind::Cancelled);
    let context = errors[0].context().ok_or("cancellation context")?;
    assert_eq!(context.phase(), ModelErrorPhase::Operation);
    // Accepted inference may have been billed; nothing may replay it.
    assert_eq!(
        context.retryability(),
        ModelRetryability::Unsafe {
            risk: ModelRetryRisk::DuplicateInferenceOrBilling
        }
    );
    assert!(context.http_status().is_none());
    assert!(errors[0].dispatch_rejection().is_none());
    Ok(())
}

#[tokio::test]
async fn semantic_compaction_workflow_retains_source_configuration_and_tools() -> TestResult {
    let facts = OpenAiPlatformModelFacts::Gpt56Luna;
    let authority = authority(facts)?;
    let mut behavior = selected_configuration(&authority, ReasoningEffort::Low)?
        .behavior()
        .clone();
    behavior.tools = vec![tool()?];
    let source_output_bound = NonZeroU32::new(8192).ok_or("bound")?;
    behavior.output_bound = OutputBoundDisposition::Enforced(source_output_bound);
    let compaction_output_bound = plakous_model::semantic_compaction_output_bound(
        Some(source_output_bound),
        plakous_model::ReasoningPolicy::Enabled {
            effort: Some(ReasoningEffort::Low),
            requirement: plakous_model::CapabilityRequirement::Required,
        },
    );
    let selected = authority.selected_configuration(behavior)?;
    let (client, origin, peer) = endpoint(response(facts.model_id(), 200)).await?;
    let configured = Arc::new(OpenAiPlatformModel::with_local_transport(
        authority, selected, client, origin,
    )?)
    .configured()
    .await?;
    let configuration_before = configured.configuration().clone();
    let request = ModelRequest::builder(
        configuration_before.selected_identity(),
        configuration_before.effective_identity(),
        ConversationView::from_items(vec![ConversationItem::new(
            EntryId::from_str("00000000-0000-4000-8000-000000000711")?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new(
                plakous_model::SEMANTIC_COMPACTION_INSTRUCTION,
                None,
            )?)],
            Provenance::UserInput,
        )?])?,
        RequestMetadata::for_session_workflow(
            RunId::from_str("00000000-0000-4000-8000-000000000712")?,
            plakous_model::SessionWorkflowRootId::new([7; 32])?,
            AttemptId::from_str("00000000-0000-4000-8000-000000000713")?,
            SystemClock.now(),
        ),
    )
    .tools(vec![tool()?])
    .reasoning(plakous_model::ReasoningPolicy::Enabled {
        effort: Some(ReasoningEffort::Low),
        requirement: plakous_model::CapabilityRequirement::Required,
    })
    .tool_choice(ToolChoice::None)
    .limits(GenerationLimits::new(
        Some(compaction_output_bound),
        Vec::new(),
    )?)
    .resolved_harness_plan_identity(Digest::sha256(
        DigestDomain::ResolvedHarnessPlan,
        1,
        &[b"compaction-source"],
    )?)
    .build()?;
    let events = execute_request(
        &configured,
        request,
        None,
        plakous_model::CancellationToken::new(),
    )
    .await?;
    assert!(events.iter().all(Result::is_ok));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Ok(ModelEvent::ResponseCompleted(_))))
            .count(),
        1
    );
    assert_eq!(configured.configuration(), &configuration_before);
    let wire = peer.await??;
    let end = wire
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .ok_or("header")?;
    let body: serde_json::Value = serde_json::from_slice(&wire[end + 4..])?;
    assert_eq!(body["max_output_tokens"], compaction_output_bound.get());
    assert_eq!(body["tool_choice"], "none");
    assert_eq!(body["tools"][0]["name"], "lookup");
    assert_eq!(body["reasoning"]["effort"], "low");
    Ok(())
}

#[tokio::test]
async fn expired_route_authority_rejects_before_rereading_credentials() -> TestResult {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    struct ExpiringClock {
        expired: AtomicBool,
        fresh: plakous_model::Timestamp,
        stale: plakous_model::Timestamp,
    }
    impl Clock for ExpiringClock {
        fn now(&self) -> plakous_model::Timestamp {
            if self.expired.load(Ordering::SeqCst) {
                self.stale
            } else {
                self.fresh
            }
        }
    }
    let clock = Arc::new(ExpiringClock {
        expired: AtomicBool::new(false),
        fresh: SystemClock.now(),
        stale: plakous_model::Timestamp::new(
            plakous_openai::OPENAI_PLATFORM_MODEL_MANIFEST_EXPIRES_AT_UNIX_SECONDS,
            0,
        )?,
    });
    let source = Arc::new(RotatingSource {
        authority: CredentialAuthorityId::new("expiry-test-source")?,
        rotated: AtomicBool::new(false),
        resolutions: AtomicUsize::new(0),
        grants: AtomicUsize::new(0),
    });
    let authority = OpenAiPlatformModelBuilder::new(
        CredentialReference::application("expiry-test")?,
        source.clone(),
        OpenAiPlatformModelFacts::Gpt56Luna,
        PlatformStoragePolicy::Stateless,
        PlatformProjectPolicy::DefaultProject,
    )
    .clock(clock.clone())
    .context_envelope(std::num::NonZeroU64::new(65_536).ok_or("bound")?)
    .seal()?;
    let selected = selected_configuration(&authority, ReasoningEffort::Low)?;
    let configured = Arc::new(OpenAiPlatformModel::new(authority, selected)?)
        .configured()
        .await?;
    let grants_before = source.grants.load(Ordering::SeqCst);
    clock.expired.store(true, Ordering::SeqCst);
    assert!(
        configured
            .model()
            .revalidate_runtime_authority(clock.now())
            .await
            .is_err()
    );
    assert_eq!(source.grants.load(Ordering::SeqCst), grants_before);
    assert_eq!(source.resolutions.load(Ordering::SeqCst), 0);
    Ok(())
}

fn web_search_query() -> plakous_openai::ResponsesRetrievalQuery {
    plakous_openai::ResponsesRetrievalQuery::WebSearch {
        query: "Rust programming language latest stable release".to_owned(),
        allowed_domains: Vec::new(),
        blocked_domains: Vec::new(),
    }
}

#[tokio::test]
async fn web_search_side_request_uses_the_session_key_project_and_model() -> TestResult {
    let facts = OpenAiPlatformModelFacts::Gpt56Luna;
    let call = serde_json::json!({"type": "web_search_call", "id": "ws_1", "status": "completed",
        "action": {"type": "search", "sources": [{"type": "url", "url": "https://www.rust-lang.org/"}]}});
    let message = serde_json::json!({"type": "message", "role": "assistant", "content": [
        {"type": "output_text", "text": "Rust 1.99.", "annotations": []}]});
    let (client, origin, peer) = endpoint(sse_response(vec![
        created(facts.model_id()),
        terminal(facts.model_id(), serde_json::json!([call, message])),
    ]))
    .await?;
    let authority = authority(facts)?;
    let selected = selected_configuration(&authority, ReasoningEffort::Low)?;
    let model = Arc::new(OpenAiPlatformModel::with_local_transport(
        authority, selected, client, origin,
    )?);
    let retrieval = model.retrieval(Some("low"));
    let outcome = retrieval
        .retrieve(web_search_query(), &plakous_model::CancellationToken::new())
        .await?;
    assert_eq!(outcome.text, "Rust 1.99.");
    assert_eq!(outcome.sources.len(), 1);
    assert_eq!(outcome.usage.hosted_requests, 1);
    let wire = peer.await??;
    let head_end = wire
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .ok_or("request head")?;
    let head = String::from_utf8_lossy(&wire[..head_end]).to_ascii_lowercase();
    assert!(head.starts_with("post /v1/responses "), "{head}");
    assert!(head.contains("authorization: bearer synthetic-platform-key"));
    assert!(head.contains("openai-project: proj_test"));
    let body: serde_json::Value = serde_json::from_slice(&wire[head_end + 4..])?;
    assert_eq!(body["model"], facts.model_id());
    assert_eq!(body["reasoning"], serde_json::json!({"effort": "low"}));
    assert_eq!(
        body["tools"],
        serde_json::json!([{"type": "web_search", "search_context_size": "medium"}])
    );
    assert_eq!(body["store"], false);
    Ok(())
}

/// Opt-in paid probe, never in a gate tier: one web search side request on
/// the Platform API with the operator's key. A pass is a non-empty answer with
/// at least one source. Run with `PLAK_LIVE_RETRIEVAL_PROBE=1
/// OPENAI_API_KEY=... cargo nextest run -p plakous-openai --run-ignored only
/// -E 'test(live_retrieval_probe)'`.
#[tokio::test]
#[ignore = "paid live probe"]
async fn live_retrieval_probe_openai_api() -> TestResult {
    if std::env::var("PLAK_LIVE_RETRIEVAL_PROBE").as_deref() != Ok("1") {
        return Err("set PLAK_LIVE_RETRIEVAL_PROBE=1 to spend one probe".into());
    }
    let key = std::env::var("OPENAI_API_KEY")?;
    let facts = OpenAiPlatformModelFacts::Gpt56Luna;
    let reference = CredentialReference::application("live-probe")?;
    let source = Arc::new(StaticPlatformCredentialSource::new(
        SecretString::new(key)?,
        reference.clone(),
        CredentialAuthorityId::new("live-probe")?,
    ));
    let authority = OpenAiPlatformModelBuilder::new(
        reference,
        source,
        facts,
        PlatformStoragePolicy::Stateless,
        PlatformProjectPolicy::DefaultProject,
    )
    .context_envelope(std::num::NonZeroU64::new(65536).ok_or("nonzero")?)
    .seal()?;
    let selected = selected_configuration(&authority, ReasoningEffort::Low)?;
    let model = Arc::new(OpenAiPlatformModel::new(authority, selected)?);
    let outcome = model
        .retrieval(Some("low"))
        .retrieve(web_search_query(), &plakous_model::CancellationToken::new())
        .await?;
    eprintln!(
        "web_search: sources={} usage={:?}",
        outcome.sources.len(),
        outcome.usage
    );
    assert!(
        !outcome.text.trim().is_empty(),
        "web_search answered nothing"
    );
    assert!(!outcome.sources.is_empty(), "web_search reported no source");
    Ok(())
}

/// Opt-in paid probe, never in a gate tier: one image generation on the
/// Platform API with the operator's key. A pass is image bytes with a PNG,
/// JPEG or WebP signature. Run with `PLAK_LIVE_IMAGE_PROBE=1
/// OPENAI_API_KEY=... cargo nextest run -p plakous-openai --run-ignored only
/// -E 'test(live_image_probe)'`.
#[tokio::test]
#[ignore = "paid live probe"]
async fn live_image_probe_openai_api() -> TestResult {
    if std::env::var("PLAK_LIVE_IMAGE_PROBE").as_deref() != Ok("1") {
        return Err("set PLAK_LIVE_IMAGE_PROBE=1 to spend one probe".into());
    }
    let key = std::env::var("OPENAI_API_KEY")?;
    let facts = OpenAiPlatformModelFacts::Gpt56Luna;
    let reference = CredentialReference::application("live-probe")?;
    let source = Arc::new(StaticPlatformCredentialSource::new(
        SecretString::new(key)?,
        reference.clone(),
        CredentialAuthorityId::new("live-probe")?,
    ));
    let authority = OpenAiPlatformModelBuilder::new(
        reference,
        source,
        facts,
        PlatformStoragePolicy::Stateless,
        PlatformProjectPolicy::DefaultProject,
    )
    .context_envelope(std::num::NonZeroU64::new(65536).ok_or("nonzero")?)
    .seal()?;
    let selected = selected_configuration(&authority, ReasoningEffort::Low)?;
    let model = Arc::new(OpenAiPlatformModel::new(authority, selected)?);
    let images = model.images();
    let request = images
        .request("A small red fox sitting in snow, flat illustration")
        .with_size(Some("1024x1024".to_owned()));
    let bytes = images
        .generate(&request, &plakous_model::CancellationToken::new())
        .await?;
    eprintln!("image_gen: bytes={}", bytes.len());
    assert!(
        bytes.starts_with(b"\x89PNG")
            || bytes.starts_with(&[0xff, 0xd8, 0xff])
            || bytes.starts_with(b"RIFF"),
        "image_gen returned no image"
    );
    Ok(())
}
