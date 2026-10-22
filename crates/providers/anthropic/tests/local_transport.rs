//! Deterministic wire evidence through the neutral Messages attempt host.

#![allow(
    clippy::expect_used,
    reason = "integration tests assert closed failure paths"
)]

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::io;
use std::num::{NonZeroU32, NonZeroUsize};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use std::time::Duration;

use futures_util::StreamExt as _;
use plakous_agent::{AgentBuilder, AgentRequestPolicy};
use plakous_anthropic::{
    ANTHROPIC_API_VERSION, AnthropicAccountIdentity, AnthropicApiKey, AnthropicError,
    AnthropicErrorKind, AnthropicErrorPhase, AnthropicHttpDeadline,
    AnthropicLocalHttpServiceFactory, AnthropicMessagesDialect, AnthropicModel,
    AnthropicPlatformRoute, AnthropicRetryGrant, ClaudeCredentialRenewal, ClaudeProfile,
    MessagesRequestTranslator, ResolvedAnthropicMessages, ThinkingSupport, ToolOwnership,
    call_local_anthropic_operation, start_anthropic_messages_model_call,
};
use plakous_http::{Client, FaultPlan, Limits, Origin, TlsConfigIdentity};
use plakous_messages::{MessagesAttempt, MessagesAttemptError};
use plakous_model::{
    Actor, AttemptId, CancellationToken, Capability, CapabilityConstraints, CapabilitySet,
    CapacityCertificate, CapacityRule, CompiledProviderConfiguration, ConfiguredLanguageModel,
    ContentBlock, ContextAdmissionRequirement, ContextContract, ContextEvidenceSource,
    ConversationItem, ConversationView, CountCertificateKind, CountObservation,
    CredentialSelectionId, Digest, DigestDomain, EffectiveProviderConfiguration,
    EffectiveProviderConfigurationId, EntryId, EvidenceFreshness, FactProvenance, FactSource,
    FamilyPolicyEnvelope, FeatureSupport, GenerationLimits, LanguageModel,
    LocalDurabilityDisposition, MeasurementCertificate, ModelAdapterProfileId, ModelCallContext,
    ModelEvent, ModelFamilyId, ModelId, ModelImplementationIdentity, ModelPreparationContext,
    ModelPreparationError, ModelPreparationErrorKind, ModelRequest, OutputBoundDisposition,
    OutputPolicy, ParallelToolDisposition, PreparedCacheUse, PreparedCallRecord, PreparedModelCall,
    PreparedModelProjection, PreparedOutputLimit, PreparedTokenProjection, ProtocolId, Provenance,
    ProviderAuthorityClassId, ProviderBehaviorConfiguration, ProviderProfileRevisionId,
    ProviderRouteSelection, ProviderSemanticEvidence, ProviderStorageDisposition,
    ReasoningEffortDisposition, ReasoningIncludeDisposition, ReasoningSummaryDisposition,
    ReplayDisposition, RequestMetadata, ResolvedModelDescriptor, ResolvedProviderAuthorityBinding,
    ResponseStateDisposition, RetentionDisposition, RouteId, RunId, RuntimeModelAuthority,
    RuntimeModelAuthorityOperation, SafeLimit, SelectedProviderConfiguration,
    SelectedProviderConfigurationId, SequenceIdGenerator, Support, SystemClock, SystemIdGenerator,
    TextBlock, Timestamp, ToolChoice, TurnId, ValidatedRequestPlan, ValidationMode,
};
use plakous_tools::{ToolRegistryBuilder, WorkspacePolicy};
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

struct HookDropSignal(Arc<AtomicUsize>);

impl Drop for HookDropSignal {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

const FIXTURE_KEY: &str = "sk-ant-local-fixture-never-production";
const FIXTURE_LOWERING_REVISION: &str = "anthropic-messages-loopback-fixture-lowering-v1";
const SUCCESS_SSE: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_local_01\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-fixture\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
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

fn resolved_fixture() -> TestResult<ResolvedAnthropicMessages> {
    resolved_fixture_model("claude-fixture")
}

fn resolved_fixture_model(model: &str) -> TestResult<ResolvedAnthropicMessages> {
    let provenance = FactProvenance::new(FactSource::TestFixture, Timestamp::new(1, 0)?, None);
    let support = FeatureSupport::new(
        Support::Native,
        CapabilityConstraints::new(None, Some("local fixture".to_owned())),
        provenance,
    );
    let descriptor = ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        ProtocolId::new("anthropic.messages")?,
        RouteId::new("anthropic.platform")?,
        ModelId::new(model)?,
        None,
        Some(ModelFamilyId::new("claude-fixture")?),
        ModelAdapterProfileId::new("claude.fixture.v1")?,
        CapabilitySet::new(
            BTreeMap::from([
                (Capability::Streaming, support.clone()),
                (Capability::Tools, support),
            ]),
            BTreeMap::new(),
        ),
    )
    .with_context_contract(context_contract()?);
    Ok(ResolvedAnthropicMessages::new(
        AnthropicMessagesDialect::stable(),
        AnthropicPlatformRoute::for_account(
            AnthropicApiKey::new(FIXTURE_KEY)?,
            AnthropicAccountIdentity::new("local-fixture-account")?,
        )?,
        AnthropicModel::new(
            ModelId::new(model)?,
            ModelFamilyId::new("claude-fixture")?,
            1024,
            ThinkingSupport::Disabled,
        )?,
        ClaudeProfile::new(ModelAdapterProfileId::new("claude.fixture.v1")?),
        descriptor,
    )?)
}

fn resolved_oauth_fixture() -> TestResult<ResolvedAnthropicMessages> {
    resolved_oauth_fixture_model("claude-fixture")
}

fn resolved_oauth_fixture_model(model: &str) -> TestResult<ResolvedAnthropicMessages> {
    let provenance = FactProvenance::new(FactSource::TestFixture, Timestamp::new(1, 0)?, None);
    let support = FeatureSupport::new(
        Support::Native,
        CapabilityConstraints::new(None, Some("local OAuth fixture".to_owned())),
        provenance,
    );
    let descriptor = ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        ProtocolId::new("anthropic.messages")?,
        RouteId::new("anthropic.claude_oauth")?,
        ModelId::new(model)?,
        None,
        Some(ModelFamilyId::new("claude-fixture")?),
        ModelAdapterProfileId::new("claude.oauth.fixture.v1")?,
        CapabilitySet::new(
            BTreeMap::from([(Capability::Streaming, support)]),
            BTreeMap::new(),
        ),
    )
    .with_context_contract(context_contract()?);
    Ok(ResolvedAnthropicMessages::new(
        AnthropicMessagesDialect::claude_oauth(),
        AnthropicPlatformRoute::claude_oauth(
            AnthropicApiKey::new("oauth-loopback-access-token-fixture")?,
            AnthropicAccountIdentity::new("oauth-loopback-account")?,
        )?,
        AnthropicModel::new(
            ModelId::new(model)?,
            ModelFamilyId::new("claude-fixture")?,
            1024,
            ThinkingSupport::Disabled,
        )?,
        ClaudeProfile::new(ModelAdapterProfileId::new("claude.oauth.fixture.v1")?),
        descriptor,
    )?)
}

fn context_contract() -> TestResult<ContextContract> {
    let freshness = EvidenceFreshness::new(Timestamp::new(1, 0)?, None)?;
    let corpus = Digest::sha256(
        DigestDomain::Blob,
        1,
        &[b"anthropic-messages-loopback-complete-wire-byte-bound-corpus-v1"],
    )?;
    Ok(ContextContract::new(
        ProtocolId::new("messages")?,
        "messages.accounting.v1",
        "messages.projection.v3",
        "messages.output.v1",
        CapacityCertificate::new(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            "messages-local-capacity",
            "messages-local-capacity-v1",
            ContextEvidenceSource::BuiltInCatalog,
            CapacityRule::SharedWindow {
                total: SafeLimit::Exact(1_000_000),
            },
            None,
            freshness,
        )?,
        vec![MeasurementCertificate::standard(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            "messages-local-counter",
            "messages-local-counter-v1",
            "complete-lowered-messages-request-body-utf8-byte-upper-bound",
            "utf8-wire-byte-upper-bound-v1",
            ContextEvidenceSource::ReleaseCertificate,
            CountCertificateKind::ReleaseVerifiedUpperBound { corpus },
            None,
            freshness,
        )?],
    )?)
}

fn runtime_authority(descriptor: &ResolvedModelDescriptor) -> TestResult<RuntimeModelAuthority> {
    Ok(RuntimeModelAuthority::new(
        descriptor,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"messages-local-account"])?,
        Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[b"messages-local-credential"],
        )?,
        1,
        Timestamp::new(1, 0)?,
        None,
    )?)
}

fn configuration(
    descriptor: &ResolvedModelDescriptor,
    authority: &RuntimeModelAuthority,
    tools: Vec<plakous_model::ToolDefinition>,
) -> TestResult<CompiledProviderConfiguration> {
    let behavior = ProviderBehaviorConfiguration {
        processing_tier: plakous_model::ProcessingTierDisposition::Standard,
        tool_mode: plakous_model::ToolPresentationMode::Direct,
        reasoning_effort: ReasoningEffortDisposition::Disabled,
        reasoning_summary: ReasoningSummaryDisposition::Omit,
        reasoning_include: ReasoningIncludeDisposition::Omit,
        replay: ReplayDisposition::Disabled,
        cache: plakous_model::CachePolicy::Disabled,
        tools: tools
            .into_iter()
            .map(|definition| {
                plakous_model::ConfiguredTool::new(
                    definition,
                    plakous_model::ToolExecutionDisposition::Local,
                    plakous_model::ToolCallerDisposition::DirectOnly,
                )
            })
            .collect(),
        hosted_tools: Vec::new(),
        tool_choice: ToolChoice::Auto,
        parallel_tools: ParallelToolDisposition::Disabled,
        output: OutputPolicy::default(),
        output_bound: OutputBoundDisposition::Enforced(NonZeroU32::new(64).ok_or("output")?),
        storage: ProviderStorageDisposition::OmittedRouteContract,
        retention: RetentionDisposition::RouteContract {
            profile: descriptor.profile().clone(),
            revision: ProviderProfileRevisionId::new("messages-local-retention-v1")?,
        },
        response_state: ResponseStateDisposition::Stateless,
        context_evidence_requirement: ContextAdmissionRequirement::AnyBounded,
        local_durability: LocalDurabilityDisposition::Ephemeral,
    };
    let selected = SelectedProviderConfiguration::new(
        ProviderRouteSelection::new(
            descriptor.protocol().clone(),
            descriptor.route().clone(),
            descriptor.profile().clone(),
            ProviderProfileRevisionId::new("messages-local-route-v1")?,
            ModelAdapterProfileId::new("messages.local.auth")?,
            ProviderProfileRevisionId::new("messages-local-auth-v1")?,
            ProviderAuthorityClassId::new("messages-local-fixture")?,
            CredentialSelectionId::new("messages-local-credential")?,
            descriptor.requested_model().clone(),
        ),
        behavior.clone(),
    )?;
    let stable = ResolvedProviderAuthorityBinding::new(
        descriptor,
        authority,
        ProviderSemanticEvidence::new(1, b"messages-local-provider-v1".to_vec())?,
    )?;
    let family = FamilyPolicyEnvelope::new(
        descriptor.model_family().cloned().ok_or("family")?,
        1,
        1,
        1,
        b"messages-local-family-v1".to_vec(),
    )?;
    let effective = EffectiveProviderConfiguration::compile(
        &selected,
        descriptor.clone(),
        ModelImplementationIdentity::release_owned(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            descriptor,
            1,
        )?,
        stable,
        behavior,
        family,
    )?;
    Ok(CompiledProviderConfiguration::new(selected, effective)?)
}

#[derive(Clone)]
struct LoopbackAnthropicLanguageModel {
    resolved: ResolvedAnthropicMessages,
    client: Client,
    origin: Origin,
    authority: RuntimeModelAuthority,
}

impl LanguageModel for LoopbackAnthropicLanguageModel {
    fn revalidate_runtime_authority(
        self: Arc<Self>,
        checked_at: Timestamp,
    ) -> RuntimeModelAuthorityOperation {
        let authority = self.authority.clone();
        let descriptor = self.resolved.descriptor().clone();
        Box::pin(async move {
            authority.validate(&descriptor, checked_at)?;
            Ok(authority)
        })
    }

    fn prepare_call(
        self: Arc<Self>,
        plan: ValidatedRequestPlan,
        context: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError> {
        let prepared = MessagesRequestTranslator::new(self.resolved.clone())
            .with_oauth_identity(fixture_oauth_identity())
            .with_session_id(
                context
                    .cache_affinity()
                    .map(plakous_model::ModelCacheAffinity::session),
            )
            .translate_with_provenance(
                &plan,
                ToolOwnership::default(),
                context.active_family_artifact(),
                context.previous_response_metadata(),
            )
            .map_err(|error| rejection(ModelPreparationErrorKind::Projection, error))?;
        let wire_size = u64::try_from(prepared.body().len()).map_err(|_| {
            rejection(
                ModelPreparationErrorKind::ResourceLimit,
                "Messages request exceeded the loopback fixture size bound",
            )
        })?;
        let wire_digest = Digest::sha256(
            DigestDomain::Blob,
            1,
            &[
                b"plakous-anthropic/loopback-messages-wire-v1",
                prepared.body(),
            ],
        )
        .map_err(|error| rejection(ModelPreparationErrorKind::Projection, error))?;
        let certificate = self
            .resolved
            .descriptor()
            .context_contract()
            .and_then(|contract| contract.measurements().first())
            .ok_or_else(|| {
                rejection(
                    ModelPreparationErrorKind::InvalidConfiguration,
                    "loopback fixture measurement certificate was unavailable",
                )
            })?;
        let observation = CountObservation::new(
            certificate.certificate_id(),
            certificate.revision(),
            wire_size,
        )
        .map_err(|error| rejection(ModelPreparationErrorKind::InvalidConfiguration, error))?;
        let max_tokens = serde_json::from_slice::<serde_json::Value>(prepared.body())
            .ok()
            .and_then(|body| body.get("max_tokens").and_then(serde_json::Value::as_u64))
            .and_then(|value| u32::try_from(value).ok())
            .and_then(NonZeroU32::new)
            .ok_or_else(|| {
                rejection(
                    ModelPreparationErrorKind::Projection,
                    "Messages output limit was absent",
                )
            })?;
        let token_projection =
            PreparedTokenProjection::new(wire_digest, wire_size, Some(observation))?;
        let output_limit = PreparedOutputLimit::enforced_wire_maximum(
            max_tokens,
            "max_tokens",
            wire_digest,
            FIXTURE_LOWERING_REVISION,
        )?;
        let projection = PreparedModelProjection::new(
            self.resolved.descriptor(),
            &plan,
            &context,
            ProtocolId::new("messages").map_err(|error| {
                rejection(ModelPreparationErrorKind::InvalidConfiguration, error)
            })?,
            FIXTURE_LOWERING_REVISION,
            "messages.projection.v3",
            "messages.accounting.v1",
            wire_digest,
            wire_size,
            token_projection,
            PreparedCacheUse::None,
            Some(output_limit),
        )?;
        let record = PreparedCallRecord::for_prepared(
            self.resolved.descriptor(),
            &plan,
            projection,
            &context,
        )?;
        let factory = AnthropicLocalHttpServiceFactory::new(
            self.client.clone(),
            self.origin.clone(),
            self.resolved.route().clone(),
            prepared,
        )
        .map_err(|error| rejection(ModelPreparationErrorKind::InvalidConfiguration, error))?;
        let factory = with_retry_authority(factory);
        let descriptor = self.resolved.descriptor().clone();
        Ok(PreparedModelCall::new(record, move |call_context| {
            start_anthropic_messages_model_call(factory, descriptor, call_context)
        }))
    }
}

fn with_retry_authority(
    factory: AnthropicLocalHttpServiceFactory,
) -> AnthropicLocalHttpServiceFactory {
    factory.with_retry(Arc::new(|route| {
        Box::pin(async move { Ok(AnthropicRetryGrant::for_api_key(route)) })
    }))
}

fn rejection(kind: ModelPreparationErrorKind, error: impl fmt::Display) -> ModelPreparationError {
    ModelPreparationError::rejected(kind, error.to_string())
}

fn prepared(
    resolved: &ResolvedAnthropicMessages,
) -> TestResult<plakous_anthropic::PreparedMessagesRequest> {
    prepared_for_session(resolved, 99)
}

fn prepared_for_session(
    resolved: &ResolvedAnthropicMessages,
    session_id: u128,
) -> TestResult<plakous_anthropic::PreparedMessagesRequest> {
    prepared_with_metadata(
        resolved,
        session_id,
        "hello",
        RequestMetadata::for_turn(
            RunId::try_from(Uuid::from_u128(2))?,
            TurnId::try_from(Uuid::from_u128(3))?,
            AttemptId::try_from(Uuid::from_u128(4))?,
            Timestamp::new(2, 0)?,
        ),
    )
}

fn prepared_with_metadata(
    resolved: &ResolvedAnthropicMessages,
    session_id: u128,
    text: &str,
    metadata: RequestMetadata,
) -> TestResult<plakous_anthropic::PreparedMessagesRequest> {
    let selected: SelectedProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::SelectedProviderConfiguration,
            5,
            &[b"local-selected"],
        )?)?)?;
    let effective: EffectiveProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"local-effective"],
        )?)?)?;
    let request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![ConversationItem::new(
            EntryId::try_from(Uuid::from_u128(1))?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new(text, None)?)],
            Provenance::UserInput,
        )?])?,
        metadata,
    )
    .build()?;
    let plan = resolved
        .descriptor()
        .validate_request(request, ValidationMode::Strict)?;
    Ok(MessagesRequestTranslator::new(resolved.clone())
        .with_oauth_identity(fixture_oauth_identity())
        .with_session_id(Some(plakous_model::SessionId::try_from(Uuid::from_u128(
            session_id,
        ))?))
        .translate(&plan, ToolOwnership::default())?)
}

fn attempt(
    factory: &AnthropicLocalHttpServiceFactory,
    cancellation: CancellationToken,
) -> TestResult<(
    MessagesAttempt,
    plakous_messages::MessagesAttemptEventReceiver,
)> {
    Ok(MessagesAttempt::with_event_channel(
        factory.operation_binding().clone(),
        ModelCallContext::new(
            AttemptId::try_from(Uuid::from_u128(10))?,
            cancellation,
            None,
        ),
        NonZeroUsize::new(32).ok_or_else(|| io::Error::other("nonzero"))?,
    ))
}

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
    response: Option<Vec<u8>>,
    stall_after_response: bool,
) -> TestResult<(Client, Origin, JoinHandle<io::Result<Vec<u8>>>)> {
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
        let (stream, _) = listener.accept().await?;
        let mut stream = acceptor.accept(stream).await?;
        let request = read_wire_request(&mut stream).await?;
        if let Some(response) = response {
            let delivery = async {
                for chunk in response.chunks(7) {
                    stream.write_all(chunk).await?;
                    tokio::task::yield_now().await;
                }
                if stall_after_response {
                    tokio::time::sleep(Duration::from_millis(250)).await;
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
        Ok(request)
    });
    Ok((client, origin, peer))
}

async fn run(
    factory: &AnthropicLocalHttpServiceFactory,
) -> TestResult<(
    Result<plakous_messages::MessagesAttemptResponse, MessagesAttemptError>,
    Vec<Result<ModelEvent, plakous_messages::MessagesAttemptProtocolError>>,
)> {
    let (attempt, mut events) = attempt(factory, CancellationToken::new())?;
    let mut service = factory.make_service();
    let operation = call_local_anthropic_operation(&mut service, attempt);
    let observations = async move {
        let mut values = Vec::new();
        while let Some(event) = events.next().await {
            values.push(event);
        }
        values
    };
    Ok(tokio::join!(operation, observations))
}

#[tokio::test]
async fn exact_request_fragmented_sse_and_provider_request_id_use_neutral_host() -> TestResult {
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nrequest-id: req_local_01\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        SUCCESS_SSE.len(), SUCCESS_SSE,
    ).into_bytes();
    let (client, origin, peer) = tls_exchange(Some(response), false).await?;
    let resolved = resolved_fixture()?;
    let prepared = prepared(&resolved)?;
    let expected_body = prepared.body().to_vec();
    let factory =
        AnthropicLocalHttpServiceFactory::new(client, origin, resolved.route().clone(), prepared)?;
    let (result, events) = timeout(Duration::from_secs(5), run(&factory)).await??;
    let result = result?;
    assert_eq!(result.request_id().as_str(), "req_local_01");
    assert!(result.response().family_artifact_candidate().is_none());
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Ok(ModelEvent::ResponseCompleted(_))))
            .count(),
        1
    );
    let captured = timeout(Duration::from_secs(5), peer).await???;
    let split = captured
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .ok_or_else(|| io::Error::other("head"))?
        + 4;
    let head = String::from_utf8_lossy(&captured[..split]).to_ascii_lowercase();
    assert!(head.starts_with("post /v1/messages http/1.1\r\n"));
    assert!(head.contains(&format!("anthropic-version: {ANTHROPIC_API_VERSION}\r\n")));
    assert!(head.contains("x-api-key: sk-ant-local-fixture-never-production\r\n"));
    assert!(head.contains("accept: text/event-stream\r\n"));
    assert!(!head.contains("anthropic-beta:"));
    assert_eq!(&captured[split..], expected_body);
    Ok(())
}

#[tokio::test]
async fn claude_oauth_capture_uses_exact_route_headers_and_omitted_reasoning() -> TestResult {
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nrequest-id: req_oauth_01\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        SUCCESS_SSE.len(),
        SUCCESS_SSE,
    )
    .into_bytes();
    let (client, origin, peer) = tls_exchange(Some(response), false).await?;
    let resolved = resolved_oauth_fixture()?;
    let prepared = prepared(&resolved)?;
    let expected_body = prepared.body().to_vec();
    let factory =
        AnthropicLocalHttpServiceFactory::new(client, origin, resolved.route().clone(), prepared)?;
    let (result, _) = run(&factory).await?;
    assert_eq!(result?.request_id().as_str(), "req_oauth_01");
    let captured = timeout(Duration::from_secs(5), peer).await???;
    let split = captured
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .ok_or_else(|| io::Error::other("request head"))?
        + 4;
    let head = String::from_utf8_lossy(&captured[..split]).to_ascii_lowercase();
    assert!(head.starts_with("post /v1/messages?beta=true http/1.1\r\n"));
    assert!(head.contains("\r\naccept: application/json\r\n"));
    assert!(head.contains("\r\nanthropic-version: 2023-06-01\r\n"));
    assert!(head.contains(&format!(
        "\r\nanthropic-beta: {}\r\n",
        plakous_anthropic::CLAUDE_OAUTH_BASELINE_BETAS
    )));
    assert!(
        head.lines()
            .any(|line| line.starts_with("authorization: bearer "))
    );
    assert!(head.contains(&format!(
        "\r\nuser-agent: {}\r\n",
        plakous_anthropic::CLAUDE_OAUTH_MESSAGES_USER_AGENT.to_ascii_lowercase()
    )));
    assert!(head.contains("\r\nx-app: cli\r\n"));
    assert!(head.contains("\r\nanthropic-dangerous-direct-browser-access: true\r\n"));
    assert!(head.contains("\r\nx-stainless-lang: js\r\n"));
    assert!(head.contains("\r\nx-stainless-runtime: node\r\n"));
    assert!(head.contains("\r\nx-stainless-package-version: 0.128.0\r\n"));
    assert!(head.contains("\r\nx-client-request-id: "));
    assert!(head.contains("\r\nx-claude-code-session-id: "));
    assert!(!head.contains("x-api-key"));
    assert_eq!(&captured[split..], expected_body);
    let body: serde_json::Value = serde_json::from_slice(&captured[split..])?;
    assert!(body.get("thinking").is_none());
    assert!(body.get("output_config").is_none());
    let metadata: serde_json::Value = serde_json::from_str(
        body["metadata"]["user_id"]
            .as_str()
            .ok_or_else(|| io::Error::other("OAuth metadata"))?,
    )?;
    let session_id = metadata["session_id"]
        .as_str()
        .ok_or_else(|| io::Error::other("OAuth session"))?;
    assert_eq!(session_id, Uuid::from_u128(99).to_string());
    assert!(head.contains(&format!("\r\nx-claude-code-session-id: {session_id}\r\n")));
    assert!(head.contains(&format!(
        "\r\nx-client-request-id: {}\r\n",
        Uuid::from_u128(10)
    )));
    assert_ne!(session_id, Uuid::from_u128(10).to_string());
    Ok(())
}

#[tokio::test]
async fn oauth_http_session_identity_survives_distinct_attempts() -> TestResult {
    for (session, dispatch) in [(99, 10), (99, 11), (100, 12)] {
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nrequest-id: req_session_fixture\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            SUCCESS_SSE.len(), SUCCESS_SSE,
        ).into_bytes();
        let (client, origin, peer) = tls_exchange(Some(response), false).await?;
        let resolved = resolved_oauth_fixture()?;
        let factory = AnthropicLocalHttpServiceFactory::new(
            client,
            origin,
            resolved.route().clone(),
            prepared_for_session(&resolved, session)?,
        )?;
        let (attempt, mut events) = MessagesAttempt::with_event_channel(
            factory.operation_binding().clone(),
            ModelCallContext::new(
                AttemptId::try_from(Uuid::from_u128(dispatch))?,
                CancellationToken::new(),
                None,
            ),
            NonZeroUsize::new(32).ok_or("event capacity")?,
        );
        let mut service = factory.make_service();
        let (result, ()) = tokio::join!(
            call_local_anthropic_operation(&mut service, attempt),
            async { while events.next().await.is_some() {} }
        );
        result?;
        let captured = timeout(Duration::from_secs(5), peer).await???;
        let split = captured
            .windows(4)
            .position(|part| part == b"\r\n\r\n")
            .ok_or("request head")?
            + 4;
        let head = String::from_utf8_lossy(&captured[..split]).to_ascii_lowercase();
        let body: serde_json::Value = serde_json::from_slice(&captured[split..])?;
        let metadata: serde_json::Value =
            serde_json::from_str(body["metadata"]["user_id"].as_str().ok_or("metadata")?)?;
        assert_eq!(metadata["session_id"], Uuid::from_u128(session).to_string());
        assert!(head.contains(&format!(
            "\r\nx-claude-code-session-id: {}\r\n",
            Uuid::from_u128(session)
        )));
        assert!(head.contains(&format!(
            "\r\nx-client-request-id: {}\r\n",
            Uuid::from_u128(dispatch)
        )));
    }
    Ok(())
}

#[tokio::test]
async fn loopback_language_model_executes_through_public_agent() -> TestResult {
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nrequest-id: req_agent_01\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        SUCCESS_SSE.len(), SUCCESS_SSE,
    )
    .into_bytes();
    let (client, origin, peer) = tls_exchange(Some(response), false).await?;
    let workspace_dir = tempfile::tempdir()?;
    let agent = loopback_agent(
        client,
        origin,
        (ToolRegistryBuilder::default().freeze(), Vec::new()),
        workspace_dir.path(),
    )?;

    let turn = agent.prompt("hello").await?;
    let result = turn.result().await?;
    assert_eq!(result.response().ok_or("response")?.output_text(), "local");
    assert_eq!(result.model_rounds(), 1);
    timeout(Duration::from_secs(5), peer).await???;
    agent.shutdown().await?;
    Ok(())
}

/// A public agent over the real Anthropic model stream and classifier.
fn loopback_agent(
    client: Client,
    origin: Origin,
    (tools, definitions): (
        plakous_tools::FrozenToolRegistry,
        Vec<plakous_model::ToolDefinition>,
    ),
    workspace: &std::path::Path,
) -> TestResult<plakous_agent::Agent> {
    let resolved = resolved_fixture()?;
    let authority = runtime_authority(resolved.descriptor())?;
    let model = Arc::new(LoopbackAnthropicLanguageModel {
        resolved: resolved.clone(),
        client,
        origin,
        authority: authority.clone(),
    });
    let configured = ConfiguredLanguageModel::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        model,
        configuration(resolved.descriptor(), &authority, definitions)?,
        authority,
    )?;
    let workspace = Arc::new(WorkspacePolicy::host(workspace, ".")?);
    let ids = Arc::new(SequenceIdGenerator::new(
        (100_u128..=400).map(Uuid::from_u128),
    )?);
    Ok(AgentBuilder::new(configured, tools, workspace)
        .request_policy(
            AgentRequestPolicy::default()
                .with_generation_limits(GenerationLimits::new(NonZeroU32::new(64), Vec::new())?),
        )
        .identity_sources(Arc::new(SystemClock), ids)
        .build()?)
}

/// Counts executions so an incomplete response cannot silently run a tool.
struct CountingTool(Mutex<Vec<Option<String>>>);

impl plakous_tools::ToolExecutor for CountingTool {
    fn execute(
        self: Arc<Self>,
        invocation: plakous_tools::ToolInvocation,
        _context: plakous_tools::ToolContext,
    ) -> plakous_tools::ToolFuture {
        self.0
            .lock()
            .expect("record calls")
            .push(invocation.correlation().remote_call_id().map(str::to_owned));
        Box::pin(async { plakous_tools::ToolOutcome::success_text("ran") })
    }
}

fn probe_registry(
    tool: Arc<CountingTool>,
) -> TestResult<(
    plakous_tools::FrozenToolRegistry,
    Vec<plakous_model::ToolDefinition>,
)> {
    let definition = plakous_model::ToolDefinition::new(
        plakous_model::ToolName::new("probe")?,
        "1.0.0",
        "probe",
        plakous_model::ToolInputContract::structured(serde_json::json!({
            "$schema":"https://json-schema.org/draft/2020-12/schema",
            "type":"object", "properties":{}, "required":[],
            "additionalProperties":false
        }))?,
        plakous_model::ToolOutputContract::portable_model_content(),
        plakous_model::ToolConcurrency::Serial,
        plakous_model::ToolEffect::ReadOnly,
    )?;
    let mut builder = ToolRegistryBuilder::default();
    builder.register(plakous_tools::ToolRegistration::new(
        plakous_tools::RegisteredToolId::new("probe-id")?,
        plakous_tools::ToolExecutorId::new("probe-executor-v1")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"probe-executor-v1"])?,
        definition.clone(),
        tool,
    ))?;
    Ok((builder.freeze(), vec![definition]))
}

fn sse_response(body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nrequest-id: req_break_01\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len(),
    )
    .into_bytes()
}

/// A stream reset after text streamed, before any tool call, is replayed for
/// the turn: the partial text is discarded, the replay's output alone is kept,
/// and the session takes the next prompt.
#[tokio::test]
async fn a_reset_after_streamed_text_is_replayed_and_the_session_continues() -> TestResult {
    let (client, origin, peer) = tls_exchange_sequence(vec![
        accepted_response(),
        accepted_response(),
        accepted_response(),
    ])
    .await?;
    let client = client.with_faults(Some(FaultPlan::parse("fault=reset,after-events=3,nth=1")?));
    let workspace = tempfile::tempdir()?;
    let agent = loopback_agent(
        client,
        origin,
        (ToolRegistryBuilder::default().freeze(), Vec::new()),
        workspace.path(),
    )?;

    let (first, retries) = prompt_with_retries(&agent, "hello").await?;
    assert_eq!(first.response().ok_or("response")?.output_text(), "local");
    assert_eq!(retries.len(), 1, "{retries:?}");
    assert_eq!(retries[0].cause().kind(), "stream_interrupted");
    assert!(retries[0].output_discarded());
    assert_eq!(
        (retries[0].next_physical(), retries[0].max_physical()),
        (2, 4)
    );

    let second = agent.prompt("again").await?.result().await?;
    assert_eq!(second.response().ok_or("response")?.output_text(), "local");
    agent.shutdown().await?;
    // The cut request, its replay, and the next turn's request.
    assert_eq!(timeout(Duration::from_secs(5), peer).await???, 3);
    Ok(())
}

/// A complete local `tool_use` block followed by EOF before `message_stop`
/// is discarded and replayed. Only the complete replay may execute its tool.
#[tokio::test]
async fn eof_after_a_complete_tool_use_block_replays_and_only_the_replay_runs_the_tool()
-> TestResult {
    const TOOL_USE_THEN_EOF: &str = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_local_02\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-fixture\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_01\",\"name\":\"probe\",\"input\":{}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    );
    let (client, origin, peer) =
        tls_exchange_sequence(vec![
            sse_response(TOOL_USE_THEN_EOF),
            sse_response(&format!("{}event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"tool_use\",\"stop_sequence\":null}},\"usage\":{{\"output_tokens\":1}}}}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n", TOOL_USE_THEN_EOF.replace("toolu_01", "toolu_replay"))),
            accepted_response(),
        ]).await?;
    let tool = Arc::new(CountingTool(Mutex::new(Vec::new())));
    let workspace = tempfile::tempdir()?;
    let agent = loopback_agent(
        client,
        origin,
        probe_registry(Arc::clone(&tool))?,
        workspace.path(),
    )?;

    let (result, retries) = prompt_with_retries(&agent, "call probe").await?;
    assert!(
        matches!(
            result.outcome(),
            plakous_agent::TurnResultOutcome::Completed(_)
        ),
        "{:?}",
        result.outcome()
    );
    assert_eq!(retries.len(), 1);
    assert!(retries[0].output_discarded());
    assert_eq!(retries[0].loss().completed_items, 1);
    assert_eq!(
        *tool.0.lock().expect("recorded calls"),
        [Some("toolu_replay".to_owned())],
        "only the replay's tool ran"
    );
    agent.shutdown().await?;
    assert_eq!(timeout(Duration::from_secs(5), peer).await???, 3);
    Ok(())
}

/// Runs one prompt to its terminal result and collects its retry markers.
async fn prompt_with_retries(
    agent: &plakous_agent::Agent,
    prompt: &str,
) -> TestResult<(plakous_agent::TurnResult, Vec<plakous_model::ModelRetry>)> {
    let mut turn = agent.prompt(prompt).await?;
    let mut events = turn.take_events()?;
    let mut retries = Vec::new();
    loop {
        let event = timeout(Duration::from_secs(10), events.recv()).await??;
        if let Some(ModelEvent::Retry(retry)) = event.kind().model_event() {
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

/// The hosted Sonnet 5 programmatic-tool fixture: its request names the
/// provider's code-execution caller, so the provider may act on it.
fn hosted_ptc_prepared() -> TestResult<(
    ResolvedAnthropicMessages,
    plakous_anthropic::PreparedMessagesRequest,
)> {
    let provenance = FactProvenance::new(FactSource::TestFixture, Timestamp::new(1, 0)?, None);
    let native = |label: &str| {
        FeatureSupport::new(
            Support::Native,
            CapabilityConstraints::new(None, Some(label.to_owned())),
            provenance,
        )
    };
    let profile = plakous_anthropic::CLAUDE_SONNET_5_HOSTED_PTC_PROFILE_ID;
    let descriptor = ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        ProtocolId::new("anthropic.messages")?,
        RouteId::new("anthropic.platform")?,
        ModelId::new("claude-sonnet-5")?,
        None,
        Some(ModelFamilyId::new("claude-5-sonnet")?),
        ModelAdapterProfileId::new(profile)?,
        CapabilitySet::new(
            BTreeMap::from([
                (Capability::Streaming, native("streaming")),
                (Capability::Tools, native("tools")),
                (Capability::HostedProgrammaticToolCalling, native("ptc")),
            ]),
            BTreeMap::new(),
        ),
    );
    let resolved = ResolvedAnthropicMessages::new(
        AnthropicMessagesDialect::stable(),
        AnthropicPlatformRoute::for_account(
            AnthropicApiKey::new(FIXTURE_KEY)?,
            AnthropicAccountIdentity::new("local-fixture-account")?,
        )?,
        AnthropicModel::new(
            ModelId::new("claude-sonnet-5")?,
            ModelFamilyId::new("claude-5-sonnet")?,
            8_192,
            ThinkingSupport::Disabled,
        )?,
        ClaudeProfile::new(ModelAdapterProfileId::new(profile)?),
        descriptor,
    )?;
    let schema = serde_json::json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{"city":{"type":"string"}},"required":["city"],"additionalProperties":false});
    let tool = plakous_model::ConfiguredTool::new(
        plakous_model::ToolDefinition::new(
            plakous_model::ToolName::new("weather")?,
            "1.0.0",
            "Gets weather",
            plakous_model::ToolInputContract::structured(schema)?,
            plakous_model::ToolOutputContract::portable_model_content(),
            plakous_model::ToolConcurrency::Parallel,
            plakous_model::ToolEffect::ReadOnly,
        )?,
        plakous_model::ToolExecutionDisposition::Local,
        plakous_model::ToolCallerDisposition::DirectAndProviderProgrammatic,
    );
    let selected: SelectedProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::SelectedProviderConfiguration,
            5,
            &[b"hosted-selected"],
        )?)?)?;
    let effective: EffectiveProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"hosted-effective"],
        )?)?)?;
    let request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![ConversationItem::new(
            EntryId::try_from(Uuid::from_u128(1))?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("weather", None)?)],
            Provenance::UserInput,
        )?])?,
        RequestMetadata::for_turn(
            RunId::try_from(Uuid::from_u128(2))?,
            TurnId::try_from(Uuid::from_u128(3))?,
            AttemptId::try_from(Uuid::from_u128(4))?,
            Timestamp::new(2, 0)?,
        ),
    )
    .tools(vec![tool])
    .hosted_tools(vec![plakous_model::HostedTool::ProgrammaticComposition])
    .build()?;
    let plan = resolved
        .descriptor()
        .validate_request(request, ValidationMode::Strict)?;
    let prepared = MessagesRequestTranslator::new(resolved.clone())
        .translate(&plan, ToolOwnership::default())?;
    Ok((resolved, prepared))
}

fn retries_of(
    events: &[Result<ModelEvent, plakous_messages::MessagesAttemptProtocolError>],
) -> Vec<plakous_model::ModelRetry> {
    events
        .iter()
        .filter_map(|event| match event {
            Ok(ModelEvent::Retry(retry)) if retry.loss().declined_reason.is_none() => {
                Some(retry.clone())
            }
            _ => None,
        })
        .collect()
}

/// The peer reads the turn's request and drops TLS without a byte: the
/// request may have reached the provider. A cost-only turn replays it; a
/// request the provider may act on (a hosted code-execution caller) never.
#[tokio::test]
async fn a_dropped_exchange_is_replayed_only_when_the_request_is_cost_only() -> TestResult {
    let resolved = resolved_fixture()?;
    let (hosted, hosted_request) = hosted_ptc_prepared()?;
    for (resolved, prepared, cost_only) in [
        (resolved.clone(), prepared(&resolved)?, true),
        (hosted, hosted_request, false),
    ] {
        let responses = if cost_only {
            vec![Vec::new(), accepted_response()]
        } else {
            vec![Vec::new()]
        };
        let (client, origin, peer) = tls_exchange_sequence(responses).await?;
        let factory = AnthropicLocalHttpServiceFactory::new(
            client,
            origin,
            resolved.route().clone(),
            prepared,
        )?
        .with_retry(Arc::new(|route| {
            Box::pin(async move { Ok(AnthropicRetryGrant::for_api_key(route)) })
        }));
        let (result, events) = timeout(Duration::from_secs(10), run(&factory)).await??;
        let retries = retries_of(&events);
        if cost_only {
            let response = result.expect("the replay was accepted");
            assert_eq!(response.facts().physical_attempts(), 2);
            assert_eq!(retries.len(), 1, "{retries:?}");
            assert_eq!(retries[0].cause().kind(), "transport_after_send");
            assert_eq!(timeout(Duration::from_secs(5), peer).await???, 2);
        } else {
            let error = result.expect_err("a server effect is never replayed");
            assert_eq!(error.facts().ok_or("facts")?.physical_attempts(), 1);
            assert!(retries.is_empty(), "{retries:?}");
            assert_eq!(timeout(Duration::from_secs(5), peer).await???, 1);
        }
    }
    Ok(())
}

/// Cancelling a turn while it waits out a retry backoff ends the logical
/// attempt promptly and sends nothing more.
#[tokio::test]
async fn cancellation_during_the_retry_wait_sends_nothing_more() -> TestResult {
    // No `Retry-After`, so the replay waits out a real backoff.
    let overloaded = {
        let body = br#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#;
        let mut wire = format!(
            "HTTP/1.1 529 Overloaded\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        wire.extend_from_slice(body);
        wire
    };
    let (client, origin, peer) = tls_exchange_sequence(vec![overloaded]).await?;
    let resolved = resolved_fixture()?;
    let factory = AnthropicLocalHttpServiceFactory::new(
        client,
        origin,
        resolved.route().clone(),
        prepared(&resolved)?,
    )?
    .with_retry(Arc::new(|route| {
        Box::pin(async move { Ok(AnthropicRetryGrant::for_api_key(route)) })
    }));
    let cancellation = CancellationToken::new();
    let (attempt, mut events) = attempt(&factory, cancellation.clone())?;
    let mut service = factory.make_service();
    let operation =
        tokio::spawn(async move { call_local_anthropic_operation(&mut service, attempt).await });
    loop {
        let event = timeout(Duration::from_secs(5), events.next())
            .await?
            .ok_or("events ended before the retry marker")?;
        if let Ok(ModelEvent::Retry(retry)) = event {
            assert!(retry.delay() >= Duration::from_millis(500), "{retry:?}");
            break;
        }
    }
    cancellation.cancel();
    let error = timeout(Duration::from_millis(250), operation)
        .await??
        .expect_err("cancelled during the wait");
    assert_eq!(error.facts().ok_or("facts")?.physical_attempts(), 1);
    while events.next().await.is_some() {}
    assert_eq!(timeout(Duration::from_secs(5), peer).await???, 1);
    Ok(())
}

/// A wire peer that answers one connection per queued response, in order, and
/// reports how many requests it served. An empty response reads the request
/// and drops the connection without a byte or a TLS `close_notify`.
async fn tls_exchange_sequence(
    responses: Vec<Vec<u8>>,
) -> TestResult<(Client, Origin, JoinHandle<io::Result<usize>>)> {
    tls_reply_sequence(responses.into_iter().map(TlsReply::Http).collect()).await
}

enum TlsReply {
    Http(Vec<u8>),
    BadMacMidBody,
    BadMacBeforeHead,
    BadMacAfterHead,
    DecodeErrorBeforeHead,
}

async fn tls_reply_sequence(
    replies: Vec<TlsReply>,
) -> TestResult<(Client, Origin, JoinHandle<io::Result<usize>>)> {
    tls_reply_sequence_with_hold(replies, None).await
}

async fn tls_reply_sequence_with_hold(
    replies: Vec<TlsReply>,
    mut release: Option<tokio::sync::mpsc::Receiver<()>>,
) -> TestResult<(Client, Origin, JoinHandle<io::Result<usize>>)> {
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
        Limits {
            response_body_timeout: if release.is_some() {
                Duration::from_secs(600)
            } else {
                Limits::default().response_body_timeout
            },
            ..Limits::default()
        },
        client_config,
        TlsConfigIdentity::from_parts(1, &[b"anthropic-local-tls-v1", certificate.as_ref()])?,
    )?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let origin = Origin::parse(&format!("https://{}", listener.local_addr()?))?;
    let acceptor = TlsAcceptor::from(Arc::new(server));
    let peer = tokio::spawn(async move {
        let mut answered = 0;
        for reply in replies {
            let (stream, _) = listener.accept().await?;
            let mut stream = acceptor.accept(stream).await?;
            if matches!(reply, TlsReply::BadMacMidBody) {
                let mut prefix = [0_u8; 1_024];
                stream.read_exact(&mut prefix).await?;
            } else {
                let _request = read_wire_request(&mut stream).await?;
            }
            answered += 1;
            match reply {
                TlsReply::Http(response) => {
                    if response.is_empty() {
                        // Read the request, then drop TLS without `close_notify`.
                        drop(stream);
                        continue;
                    }
                    stream.write_all(&response).await?;
                    if let Some(release) = &mut release {
                        let _ = release.recv().await;
                    }
                    let _ = stream.shutdown().await;
                }
                alert => {
                    if matches!(alert, TlsReply::BadMacAfterHead) {
                        stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 100\r\n\r\n").await?;
                        stream.flush().await?;
                    }
                    let (mut tcp, mut tls) = stream.into_inner();
                    // Rustls encrypts the fatal alert generated by this bad
                    // input, so the client receives AlertReceived, not a local
                    // decrypt failure from a corrupted server record.
                    let record = if matches!(alert, TlsReply::DecodeErrorBeforeHead) {
                        vec![0xff, 0x03, 0x03, 0x00, 0x01, 0x00]
                    } else {
                        let mut record = vec![0x17, 0x03, 0x03, 0x00, 0x20];
                        record.resize(64 * 1_024, 0x5a);
                        record
                    };
                    let mut input = io::Cursor::new(record);
                    let error = loop {
                        if tls.read_tls(&mut input)? == 0 {
                            return Err(io::Error::other("fixture did not reject the record"));
                        }
                        if let Err(error) = tls.process_new_packets() {
                            break error;
                        }
                    };
                    if !matches!(alert, TlsReply::DecodeErrorBeforeHead) {
                        assert_eq!(error, rustls::Error::DecryptError);
                    }
                    let mut wire = Vec::new();
                    while tls.wants_write() {
                        tls.write_tls(&mut wire)?;
                    }
                    tcp.write_all(&wire).await?;
                    // Avoid a TCP reset hiding the alert on a partial upload.
                    let mut buffer = vec![0_u8; 16 * 1_024];
                    let _ = timeout(Duration::from_secs(5), async {
                        while tcp.read(&mut buffer).await.is_ok_and(|read| read > 0) {}
                    })
                    .await;
                }
            }
        }
        Ok(answered)
    });
    Ok((client, origin, peer))
}

/// Counts forced renewals and reports whether each one is granted.
struct CountingRenewal {
    calls: Arc<AtomicUsize>,
    granted: bool,
}

impl ClaudeCredentialRenewal for CountingRenewal {
    fn renew_after_rejection<'a>(
        &'a self,
        route: &'a AnthropicPlatformRoute,
    ) -> Pin<Box<dyn Future<Output = Result<AnthropicRetryGrant, AnthropicError>> + Send + 'a>>
    {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.granted {
                Ok(AnthropicRetryGrant::for_api_key(route.clone()))
            } else {
                Err(AnthropicError::new(
                    AnthropicErrorKind::Authentication,
                    AnthropicErrorPhase::Prepare,
                    "renewal refused",
                ))
            }
        })
    }
}

struct PausedRenewal {
    entered: Arc<Notify>,
    release: Arc<Notify>,
    dropped: Arc<AtomicUsize>,
}

impl ClaudeCredentialRenewal for PausedRenewal {
    fn renew_after_rejection<'a>(
        &'a self,
        route: &'a AnthropicPlatformRoute,
    ) -> Pin<Box<dyn Future<Output = Result<AnthropicRetryGrant, AnthropicError>> + Send + 'a>>
    {
        Box::pin(async move {
            let _drop = HookDropSignal(Arc::clone(&self.dropped));
            self.entered.notify_one();
            self.release.notified().await;
            Ok(AnthropicRetryGrant::for_api_key(route.clone()))
        })
    }
}

fn unauthorized_response() -> Vec<u8> {
    let body =
        br#"{"type":"error","error":{"type":"authentication_error","message":"invalid bearer"}}"#;
    format!(
        "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\nrequest-id: req_denied_01\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes()
    .into_iter()
    .chain(body.iter().copied())
    .collect()
}

/// A 429 whose `Retry-After: 0` asks for an immediate retry, so retry tests
/// need no real backoff.
fn rate_limited_response() -> Vec<u8> {
    let body =
        br#"{"type":"error","error":{"type":"rate_limit_error","message":"fixture rate limit"}}"#;
    format!(
        "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\nretry-after: 0\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes()
    .into_iter()
    .chain(body.iter().copied())
    .collect()
}

fn accepted_response() -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nrequest-id: req_retry_01\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        SUCCESS_SSE.len(),
        SUCCESS_SSE,
    )
    .into_bytes()
}

#[tokio::test]
async fn fable_included_limit_requires_exact_oauth_model_and_header_claim() -> TestResult {
    let included =
        "anthropic-ratelimit-unified-representative-claim: seven_day_overage_included\r\n";
    let unknown = "anthropic-ratelimit-unified-representative-claim: unknown_limit\r\n";
    let duplicate = format!("{included}{included}");
    for (oauth, model, status, headers, expected) in [
        (true, "claude-fable-5-1", 429, included, true),
        (true, "claude-opus-5", 429, included, false),
        (false, "claude-fable-5-1", 429, included, false),
        (true, "claude-fable-5-1", 429, "", false),
        (true, "claude-fable-5-1", 429, unknown, false),
        (true, "claude-fable-5-1", 429, duplicate.as_str(), false),
        (true, "claude-fable-5-1", 400, included, false),
    ] {
        let body = br#"{"type":"error","error":{"type":"rate_limit_error","message":"fixture rate limit"}}"#;
        let mut response = format!(
            "HTTP/1.1 {status} Fixture\r\ncontent-type: application/json\r\nrequest-id: req_quota_fixture\r\nretry-after: 60\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n",
            body.len(),
        ).into_bytes();
        response.extend_from_slice(body);
        let (client, origin, peer) = tls_exchange_sequence(vec![response]).await?;
        let resolved = if oauth {
            resolved_oauth_fixture_model(model)?
        } else {
            resolved_fixture_model(model)?
        };
        let factory = AnthropicLocalHttpServiceFactory::new(
            client,
            origin,
            resolved.route().clone(),
            prepared(&resolved)?,
        )?;
        let (result, _) = timeout(Duration::from_secs(5), run(&factory)).await??;
        let error = result.expect_err("a quota response must not become a successful turn");
        assert_eq!(
            error
                .to_string()
                .contains("You've reached your Fable limit."),
            expected,
            "oauth={oauth}, model={model}, status={status}: {error}",
        );
        // Only the attested claim is typed quota exhaustion; every other
        // response keeps its own category.
        assert_eq!(
            error.usage_limit(),
            expected,
            "oauth={oauth}, model={model}, status={status}: {error}",
        );
        assert_eq!(
            error.http_status().map(plakous_model::ModelHttpStatus::get),
            Some(status)
        );
        assert_eq!(
            error
                .retry_after()
                .map(plakous_model::ModelRetryDelay::as_millis),
            Some(60_000)
        );
        assert_eq!(timeout(Duration::from_secs(5), peer).await???, 1);
    }
    Ok(())
}

#[tokio::test]
async fn a_401_triggers_one_refresh_and_retry() -> TestResult {
    // With the renewal seam installed, an HTTP 401 head forces one refresh and
    // exactly one re-dispatch of the same logical operation; the renewed grant
    // then completes normally and the caller sees one clean attempt.
    let (client, origin, peer) =
        tls_exchange_sequence(vec![unauthorized_response(), accepted_response()]).await?;
    let resolved = resolved_oauth_fixture()?;
    let first = prepared(&resolved)?;
    let calls = Arc::new(AtomicUsize::new(0));
    let factory =
        AnthropicLocalHttpServiceFactory::new(client, origin, resolved.route().clone(), first)?
            .with_credential_renewal(Arc::new(CountingRenewal {
                calls: Arc::clone(&calls),
                granted: true,
            }));
    let (result, events) = timeout(Duration::from_secs(5), run(&factory)).await??;
    let result = result?;
    assert_eq!(result.request_id().as_str(), "req_retry_01");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(events.iter().all(Result::is_ok));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Ok(ModelEvent::ResponseCompleted(_))))
            .count(),
        1
    );
    assert_eq!(timeout(Duration::from_secs(5), peer).await???, 2);

    // A second 401 is returned as-is: the hook runs once, no further dispatch.
    let (client, origin, peer) =
        tls_exchange_sequence(vec![unauthorized_response(), unauthorized_response()]).await?;
    let resolved = resolved_oauth_fixture()?;
    let repeated = prepared(&resolved)?;
    let calls = Arc::new(AtomicUsize::new(0));
    let factory =
        AnthropicLocalHttpServiceFactory::new(client, origin, resolved.route().clone(), repeated)?
            .with_credential_renewal(Arc::new(CountingRenewal {
                calls: Arc::clone(&calls),
                granted: true,
            }));
    let (result, _events) = timeout(Duration::from_secs(5), run(&factory)).await??;
    let error = result.expect_err("a repeated 401 must remain terminal");
    assert!(error.to_string().contains("401"), "{error}");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(timeout(Duration::from_secs(5), peer).await???, 2);
    Ok(())
}

#[tokio::test]
async fn cancellation_during_401_renewal_preserves_cancelled_result() -> TestResult {
    let (client, origin, peer) = tls_exchange_sequence(vec![unauthorized_response()]).await?;
    let resolved = resolved_oauth_fixture()?;
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let dropped = Arc::new(AtomicUsize::new(0));
    let factory = AnthropicLocalHttpServiceFactory::new(
        client,
        origin,
        resolved.route().clone(),
        prepared(&resolved)?,
    )?
    .with_credential_renewal(Arc::new(PausedRenewal {
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
        dropped: Arc::clone(&dropped),
    }));
    let cancellation = CancellationToken::new();
    let (attempt, mut events) = attempt(&factory, cancellation.clone())?;
    let mut service = factory.make_service();
    let operation =
        tokio::spawn(async move { call_local_anthropic_operation(&mut service, attempt).await });
    timeout(Duration::from_secs(5), entered.notified()).await?;
    cancellation.cancel();
    let result = timeout(Duration::from_millis(250), operation).await??;
    assert!(matches!(
        result,
        Err(MessagesAttemptError::Cancelled { .. })
    ));
    release.notify_one();
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    while events.next().await.is_some() {}
    assert_eq!(timeout(Duration::from_secs(5), peer).await???, 1);
    Ok(())
}

/// The 401 renewal and the rate-limit retries share one budget of four
/// physical requests, in either order, and the renewal runs once.
#[tokio::test]
async fn mixed_401_and_429_share_the_policy_budget() -> TestResult {
    for responses in [
        vec![
            unauthorized_response(),
            rate_limited_response(),
            rate_limited_response(),
            rate_limited_response(),
        ],
        vec![
            rate_limited_response(),
            unauthorized_response(),
            rate_limited_response(),
            rate_limited_response(),
        ],
    ] {
        let (client, origin, peer) = tls_exchange_sequence(responses).await?;
        let resolved = resolved_oauth_fixture()?;
        let calls = Arc::new(AtomicUsize::new(0));
        let factory = AnthropicLocalHttpServiceFactory::new(
            client,
            origin,
            resolved.route().clone(),
            prepared(&resolved)?,
        )?
        .with_credential_renewal(Arc::new(CountingRenewal {
            calls: Arc::clone(&calls),
            granted: true,
        }))
        .with_retry(Arc::new(|route| {
            Box::pin(async move { Ok(AnthropicRetryGrant::for_api_key(route)) })
        }));
        let (result, _) = timeout(Duration::from_secs(5), run(&factory)).await??;
        let error = result.expect_err("every generation was refused");
        assert_eq!(error.facts().ok_or("facts")?.physical_attempts(), 4);
        assert_eq!(timeout(Duration::from_secs(5), peer).await???, 4);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
    Ok(())
}

/// Refusals before acceptance are retried under the shared policy with a
/// visible notice, and the successful generation is the only one accepted.
#[tokio::test]
async fn refusals_before_acceptance_retry_with_a_notice_until_accepted() -> TestResult {
    let overloaded = {
        let body = br#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#;
        let mut wire = format!(
            "HTTP/1.1 529 Overloaded\r\ncontent-type: application/json\r\nretry-after: 0\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        wire.extend_from_slice(body);
        wire
    };
    let (client, origin, peer) = tls_exchange_sequence(vec![
        rate_limited_response(),
        overloaded,
        accepted_response(),
    ])
    .await?;
    let resolved = resolved_fixture()?;
    let factory = AnthropicLocalHttpServiceFactory::new(
        client,
        origin,
        resolved.route().clone(),
        prepared(&resolved)?,
    )?
    .with_retry(Arc::new(|route| {
        Box::pin(async move { Ok(AnthropicRetryGrant::for_api_key(route)) })
    }));
    let (result, events) = timeout(Duration::from_secs(5), run(&factory)).await??;
    let response = result.expect("the third generation was accepted");
    assert_eq!(response.facts().physical_attempts(), 3);
    let notices = events
        .iter()
        .filter_map(|event| match event {
            Ok(ModelEvent::Retry(retry)) => Some(format!(
                "{} {}/{} in {:?}",
                retry.cause(),
                retry.next_physical(),
                retry.max_physical(),
                retry.delay()
            )),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        notices,
        [
            "provider rate limited (HTTP 429) 2/4 in 0ns",
            "provider busy (HTTP 529) 3/4 in 0ns",
        ]
    );
    assert_eq!(timeout(Duration::from_secs(5), peer).await???, 3);
    Ok(())
}

/// A connection reset while the request is still being written cannot have
/// reached Anthropic whole, so it is retried before acceptance like a refusal.
#[tokio::test]
async fn a_reset_during_the_request_write_is_retried_before_acceptance() -> TestResult {
    let (client, origin, peer) = tls_exchange_sequence(vec![accepted_response()]).await?;
    let client = client.with_faults(Some(FaultPlan::parse("fault=write-reset,nth=1")?));
    let resolved = resolved_fixture()?;
    let factory = AnthropicLocalHttpServiceFactory::new(
        client,
        origin,
        resolved.route().clone(),
        prepared(&resolved)?,
    )?
    .with_retry(Arc::new(|route| {
        Box::pin(async move { Ok(AnthropicRetryGrant::for_api_key(route)) })
    }));
    let (result, _events) = timeout(Duration::from_secs(5), run(&factory)).await??;
    let response = result.expect("the retried generation was accepted");
    assert_eq!(response.facts().physical_attempts(), 2);
    assert_eq!(timeout(Duration::from_secs(5), peer).await???, 1);
    Ok(())
}

#[tokio::test]
async fn a_long_messages_stream_failure_allows_only_one_replay() -> TestResult {
    let prefix = SUCCESS_SSE
        .split("event: content_block_stop")
        .next()
        .ok_or("SSE boundary")?;
    let incomplete = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nrequest-id: req_local_01\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{prefix}",
        prefix.len() + 1,
    ).into_bytes();
    let (release, holds) = tokio::sync::mpsc::channel(1);
    let (client, origin, peer) = tls_reply_sequence_with_hold(
        vec![
            TlsReply::Http(incomplete.clone()),
            TlsReply::Http(incomplete),
        ],
        Some(holds),
    )
    .await?;
    let resolved = resolved_fixture()?;
    let factory = with_retry_authority(AnthropicLocalHttpServiceFactory::new(
        client,
        origin,
        resolved.route().clone(),
        prepared(&resolved)?,
    )?);
    let (attempt, mut events) = attempt(&factory, CancellationToken::new())?;
    let mut service = factory.make_service();
    let operation =
        tokio::spawn(async move { call_local_anthropic_operation(&mut service, attempt).await });
    for physical in 1..=2 {
        loop {
            let event = timeout(Duration::from_secs(5), events.next())
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
            let event = timeout(Duration::from_secs(5), events.next())
                .await?
                .ok_or("retry channel closed")??;
            if let ModelEvent::Retry(retry) = event {
                break retry;
            }
        };
        assert_eq!(retry.next_physical(), physical + 1);
        assert_eq!(retry.loss().assistant_text_bytes, 5);
        if physical == 1 {
            assert!(retry.loss().elapsed_ms > 300_000);
            assert!(retry.output_discarded());
            assert_eq!(retry.loss().declined_reason, None);
        } else {
            assert!(retry.loss().elapsed_ms < 300_000);
            assert!(!retry.output_discarded());
            assert_eq!(retry.delay(), Duration::ZERO);
            let (host, route, bytes) = retry.request().ok_or("request logging facts")?;
            assert_eq!(host, "127.0.0.1");
            assert_eq!(route, "/v1/messages");
            assert!(bytes > 0);
            assert_eq!(
                retry.loss().declined_reason,
                Some(plakous_model::ModelRetryDeclineReason::Budget)
            );
        }
    }
    let error = timeout(Duration::from_secs(5), operation)
        .await??
        .expect_err("second failure must be terminal");
    assert_eq!(
        error
            .facts()
            .ok_or("admitted failure facts")?
            .physical_attempts(),
        2
    );
    assert_eq!(timeout(Duration::from_secs(5), peer).await???, 2);
    Ok(())
}

/// Without retry authorization a refusal is returned after one physical request.
#[tokio::test]
async fn a_session_workflow_refusal_is_single_dispatch() -> TestResult {
    let (client, origin, peer) = tls_exchange_sequence(vec![rate_limited_response()]).await?;
    let resolved = resolved_fixture()?;
    let factory = AnthropicLocalHttpServiceFactory::new(
        client,
        origin,
        resolved.route().clone(),
        prepared(&resolved)?,
    )?;
    let (result, _) = timeout(Duration::from_secs(5), run(&factory)).await??;
    let error = result.expect_err("refused");
    assert_eq!(error.facts().ok_or("facts")?.physical_attempts(), 1);
    assert_eq!(timeout(Duration::from_secs(5), peer).await???, 1);
    Ok(())
}

struct TerminalCapture {
    required: bool,
    fail_terminal: bool,
    failed: bool,
    completed: Arc<AtomicUsize>,
}

impl plakous_http::ExchangeObserver for TerminalCapture {
    fn required(&self) -> bool {
        self.required
    }

    fn start(&self, _: &[u8]) -> Option<Box<dyn plakous_http::ExchangeObservation>> {
        Some(Box::new(Self {
            required: self.required,
            fail_terminal: self.fail_terminal,
            failed: false,
            completed: Arc::clone(&self.completed),
        }))
    }
}

impl plakous_http::ExchangeObservation for TerminalCapture {
    fn failed(&self) -> bool {
        self.failed
    }

    fn response_head(&mut self, _: u16) {}

    fn response_chunk(&mut self, _: &[u8]) {}

    fn terminal(&mut self, terminal: &plakous_http::ExchangeTerminal) {
        if *terminal == plakous_http::ExchangeTerminal::Complete {
            self.completed.fetch_add(1, Ordering::SeqCst);
        }
        self.failed = self.fail_terminal;
    }
}

/// Required capture failure at refusal-body EOF must not become a retryable
/// 429. Healthy required capture and failed passive capture keep status retry.
#[tokio::test]
async fn refusal_terminal_capture_failure_is_transport_not_status_retry() -> TestResult {
    for (required, fail_terminal, truncated) in [
        (true, true, false),
        (true, true, true),
        (true, false, false),
        (true, false, true),
        (false, true, false),
        (false, true, true),
    ] {
        let capture_fails = required && fail_terminal;
        let mut refusal = rate_limited_response();
        if truncated {
            refusal.pop();
        }
        let mut responses = vec![refusal];
        if !capture_fails {
            responses.push(accepted_response());
        }
        let (client, origin, peer) = tls_exchange_sequence(responses).await?;
        let completed = Arc::new(AtomicUsize::new(0));
        let client = client.with_observer(Some(Arc::new(TerminalCapture {
            required,
            fail_terminal,
            failed: false,
            completed: Arc::clone(&completed),
        })));
        let resolved = resolved_fixture()?;
        let retries = Arc::new(AtomicUsize::new(0));
        let factory = AnthropicLocalHttpServiceFactory::new(
            client,
            origin,
            resolved.route().clone(),
            prepared(&resolved)?,
        )?
        .with_retry(Arc::new({
            let retries = Arc::clone(&retries);
            move |route| {
                retries.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move { Ok(AnthropicRetryGrant::for_api_key(route)) })
            }
        }));
        let (result, events) = timeout(Duration::from_secs(5), run(&factory)).await??;
        let notices = events
            .iter()
            .filter_map(|event| match event {
                Ok(ModelEvent::Retry(retry)) => Some(format!(
                    "{} {}/{} in {:?}",
                    retry.cause(),
                    retry.next_physical(),
                    retry.max_physical(),
                    retry.delay()
                )),
                _ => None,
            })
            .collect::<Vec<_>>();
        if capture_fails {
            let error = result.expect_err("required terminal capture failed");
            assert!(matches!(error, MessagesAttemptError::Protocol { .. }));
            assert_eq!(
                error.to_string(),
                "anthropic/Transport status=None request_id=None: response stream interrupted"
            );
            assert_eq!(error.http_status(), None);
            assert_eq!(error.provider_refusal(), None);
            assert_eq!(error.facts().ok_or("facts")?.physical_attempts(), 1);
            assert_eq!(retries.load(Ordering::SeqCst), 0);
            assert!(notices.is_empty(), "{notices:?}");
            assert_eq!(completed.load(Ordering::SeqCst), usize::from(!truncated));
            assert_eq!(timeout(Duration::from_secs(5), peer).await???, 1);
        } else {
            let response = result.expect("capture must preserve the 429 retry");
            assert_eq!(response.facts().physical_attempts(), 2);
            assert_eq!(retries.load(Ordering::SeqCst), 1);
            assert_eq!(notices, ["provider rate limited (HTTP 429) 2/4 in 0ns"]);
            assert!(events.iter().all(Result::is_ok));
            assert_eq!(
                completed.load(Ordering::SeqCst),
                1 + usize::from(!truncated)
            );
            assert_eq!(timeout(Duration::from_secs(5), peer).await???, 2);
        }
    }
    Ok(())
}

#[tokio::test]
async fn cancellation_during_retry_hook_finishes_without_late_dispatch() -> TestResult {
    let (client, origin, peer) = tls_exchange_sequence(vec![rate_limited_response()]).await?;
    let resolved = resolved_oauth_fixture()?;
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let dropped = Arc::new(AtomicUsize::new(0));
    let factory = AnthropicLocalHttpServiceFactory::new(
        client,
        origin,
        resolved.route().clone(),
        prepared(&resolved)?,
    )?
    .with_retry(Arc::new({
        let entered = Arc::clone(&entered);
        let release = Arc::clone(&release);
        let dropped = Arc::clone(&dropped);
        move |route| {
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            let dropped = Arc::clone(&dropped);
            Box::pin(async move {
                let _drop = HookDropSignal(dropped);
                entered.notify_one();
                release.notified().await;
                Ok(AnthropicRetryGrant::for_api_key(route))
            })
        }
    }));
    let cancellation = CancellationToken::new();
    let (attempt, mut events) = attempt(&factory, cancellation.clone())?;
    let mut service = factory.make_service();
    let operation =
        tokio::spawn(async move { call_local_anthropic_operation(&mut service, attempt).await });
    timeout(Duration::from_secs(5), entered.notified()).await?;
    cancellation.cancel();
    let result = timeout(Duration::from_millis(250), operation).await??;
    assert!(result.is_err());
    release.notify_one();
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    while events.next().await.is_some() {}
    assert_eq!(timeout(Duration::from_secs(5), peer).await???, 1);
    Ok(())
}

#[tokio::test]
async fn exchange_deadline_during_retry_hook_prevents_late_dispatch() -> TestResult {
    let (client, origin, peer) = tls_exchange_sequence(vec![rate_limited_response()]).await?;
    let resolved = resolved_oauth_fixture()?;
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let dropped = Arc::new(AtomicUsize::new(0));
    let factory = AnthropicLocalHttpServiceFactory::with_runtime(
        client,
        origin,
        resolved.route().clone(),
        prepared(&resolved)?,
        Arc::new(SystemClock),
        Arc::new(SystemIdGenerator),
        AnthropicHttpDeadline::new(Duration::from_millis(350))?,
    )?
    .with_retry(Arc::new({
        let entered = Arc::clone(&entered);
        let release = Arc::clone(&release);
        let dropped = Arc::clone(&dropped);
        move |route| {
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            let dropped = Arc::clone(&dropped);
            Box::pin(async move {
                let _drop = HookDropSignal(dropped);
                entered.notify_one();
                release.notified().await;
                Ok(AnthropicRetryGrant::for_api_key(route))
            })
        }
    }));
    let (attempt, mut events) = attempt(&factory, CancellationToken::new())?;
    let mut service = factory.make_service();
    let operation =
        tokio::spawn(async move { call_local_anthropic_operation(&mut service, attempt).await });
    timeout(Duration::from_secs(5), entered.notified()).await?;
    let result = timeout(Duration::from_secs(1), operation).await??;
    assert!(result.is_err());
    release.notify_one();
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    while events.next().await.is_some() {}
    assert_eq!(timeout(Duration::from_secs(5), peer).await???, 1);
    Ok(())
}

#[tokio::test]
async fn a_rejected_grant_still_ends_not_ready() -> TestResult {
    // Fail-closed posture: a 401 whose forced refresh is itself refused ends
    // the attempt on the provider's own 401, with no second dispatch.
    let (client, origin, peer) = tls_exchange_sequence(vec![unauthorized_response()]).await?;
    let resolved = resolved_oauth_fixture()?;
    let prepared = prepared(&resolved)?;
    let calls = Arc::new(AtomicUsize::new(0));
    let factory =
        AnthropicLocalHttpServiceFactory::new(client, origin, resolved.route().clone(), prepared)?
            .with_credential_renewal(Arc::new(CountingRenewal {
                calls: Arc::clone(&calls),
                granted: false,
            }));
    let (result, events) = timeout(Duration::from_secs(5), run(&factory)).await??;
    let error = result.expect_err("a refused renewal must not mask the 401");
    assert!(error.to_string().contains("401"), "{error}");
    assert!(
        !error
            .facts()
            .is_some_and(plakous_messages::TerminalFacts::provider_accepted)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(events.iter().any(Result::is_err));
    assert_eq!(timeout(Duration::from_secs(5), peer).await???, 1);
    Ok(())
}

#[tokio::test]
async fn provider_rejection_and_ambiguous_disconnect_keep_exact_physical_facts() -> TestResult {
    let error_body = br#"{"type":"error","error":{"type":"rate_limit_error","message":"slow"}}"#;
    let response = format!(
        "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\nrequest-id: req_rate_01\r\nretry-after: 2\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        error_body.len(), String::from_utf8_lossy(error_body),
    ).into_bytes();
    let (client, origin, peer) = tls_exchange(Some(response), false).await?;
    let resolved = resolved_fixture()?;
    let factory = AnthropicLocalHttpServiceFactory::new(
        client,
        origin,
        resolved.route().clone(),
        prepared(&resolved)?,
    )?;
    let (result, events) = run(&factory).await?;
    let error = result.expect_err("429 succeeded");
    assert_eq!(
        error.http_status().map(plakous_model::ModelHttpStatus::get),
        Some(429)
    );
    assert_eq!(
        error
            .retry_after()
            .map(plakous_model::ModelRetryDelay::as_millis),
        Some(2_000)
    );
    let facts = error.facts().ok_or_else(|| io::Error::other("facts"))?;
    assert_eq!(
        facts.provider_acceptance(),
        plakous_messages::ProviderAcceptance::NotAccepted
    );
    assert!(events.iter().any(|event| {
        event.as_ref().is_err_and(|error| {
            error.message().contains("RateLimited") && error.message().contains("req_rate_01")
        })
    }));
    timeout(Duration::from_secs(5), peer).await???;

    let (client, origin, peer) = tls_exchange(None, false).await?;
    let resolved = resolved_fixture()?;
    let factory = AnthropicLocalHttpServiceFactory::new(
        client,
        origin,
        resolved.route().clone(),
        prepared(&resolved)?,
    )?;
    let (result, _events) = run(&factory).await?;
    let facts = result
        .expect_err("disconnect succeeded")
        .facts()
        .ok_or_else(|| io::Error::other("facts"))?;
    assert_eq!(facts.physical_attempts(), 1);
    assert_eq!(
        facts.provider_acceptance(),
        plakous_messages::ProviderAcceptance::PossiblyAccepted
    );
    timeout(Duration::from_secs(5), peer).await???;
    Ok(())
}

#[tokio::test]
async fn accepted_sse_error_uses_anthropic_classification_and_terminal_delivery() -> TestResult {
    let body = include_bytes!("fixtures/stream_error.sse");
    let mut response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nrequest-id: req_stream_01\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len(),
    )
    .into_bytes();
    response.extend_from_slice(body);
    let (client, origin, peer) = tls_exchange(Some(response), false).await?;
    let resolved = resolved_fixture()?;
    let factory = AnthropicLocalHttpServiceFactory::new(
        client,
        origin,
        resolved.route().clone(),
        prepared(&resolved)?,
    )?;
    let (result, events) = run(&factory).await?;
    let facts = result
        .expect_err("provider SSE error succeeded")
        .facts()
        .ok_or_else(|| io::Error::other("facts"))?;
    assert_eq!(
        facts.provider_acceptance(),
        plakous_messages::ProviderAcceptance::Accepted
    );
    assert!(
        events
            .iter()
            .any(|event| event.as_ref().is_err_and(|error| {
                error.message().contains("Overloaded")
                    && !error.message().contains("temporarily overloaded")
            }))
    );
    timeout(Duration::from_secs(5), peer).await???;
    Ok(())
}

/// An injected `529` never reaches Anthropic and reads exactly as Anthropic's
/// own overloaded refusal: same status, acceptance, attempts and error class.
#[tokio::test]
async fn an_injected_529_reads_as_anthropics_own_overload() -> TestResult {
    let body = r#"{"type":"error","error":{"type":"overloaded_error","code":529,"message":"injected by PLAK_TEST_FAULTS"}}"#;
    let response = format!(
        "HTTP/1.1 529 Overloaded\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len(),
    )
    .into_bytes();
    let mut outcomes = Vec::new();
    for injected in [false, true] {
        let (client, origin, peer) = tls_exchange(Some(response.clone()), false).await?;
        let client = client.with_faults(if injected {
            Some(FaultPlan::parse("fault=529")?)
        } else {
            None
        });
        let resolved = resolved_fixture()?;
        let factory = AnthropicLocalHttpServiceFactory::new(
            client,
            origin,
            resolved.route().clone(),
            prepared(&resolved)?,
        )?;
        let (result, events) = run(&factory).await?;
        let error = result.expect_err("529 succeeded");
        let facts = error.facts().ok_or_else(|| io::Error::other("facts"))?;
        outcomes.push((
            error.http_status().map(plakous_model::ModelHttpStatus::get),
            facts.provider_acceptance(),
            facts.physical_attempts(),
            events.iter().any(|event| {
                event
                    .as_ref()
                    .is_err_and(|error| error.message().contains("Overloaded"))
            }),
            facts.summary().to_string(),
        ));
        if injected {
            assert!(!peer.is_finished(), "the injected 529 reached the peer");
            peer.abort();
        } else {
            timeout(Duration::from_secs(5), peer).await???;
        }
    }
    assert_eq!(outcomes[0], outcomes[1]);
    assert_eq!(outcomes[0].0, Some(529));
    assert!(outcomes[0].3, "classified as Anthropic overload");
    // The attempt summary names the class the terminal is typed as.
    assert!(
        outcomes[0].4.contains("last: unavailable"),
        "{}",
        outcomes[0].4
    );
    Ok(())
}

#[tokio::test]
async fn cancellation_after_accepted_head_closes_without_safe_retry() -> TestResult {
    let response = concat!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n",
        "request-id: req_cancel_01\r\nconnection: close\r\n\r\n",
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_cancel_01\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-fixture\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
    )
    .as_bytes()
    .to_vec();
    let (client, origin, peer) = tls_exchange(Some(response), true).await?;
    let resolved = resolved_fixture()?;
    let factory = AnthropicLocalHttpServiceFactory::new(
        client,
        origin,
        resolved.route().clone(),
        prepared(&resolved)?,
    )?;
    let cancellation = CancellationToken::new();
    let (attempt, mut events) = attempt(&factory, cancellation.clone())?;
    let mut service = factory.make_service();
    let operation = call_local_anthropic_operation(&mut service, attempt);
    let drain = async move {
        while let Some(event) = events.next().await {
            if matches!(event, Ok(ModelEvent::ResponseStarted(_))) {
                cancellation.cancel();
            }
        }
    };
    let (result, ()) = timeout(Duration::from_secs(5), async move {
        tokio::join!(operation, drain)
    })
    .await?;
    let error = result.expect_err("cancelled stream succeeded");
    let facts = error.facts().ok_or_else(|| io::Error::other("facts"))?;
    assert_eq!(facts.physical_attempts(), 1);
    assert_eq!(
        facts.provider_acceptance(),
        plakous_messages::ProviderAcceptance::Accepted
    );
    assert_ne!(
        facts.retry_class(),
        plakous_messages::RetryClass::BeforeAcceptance
    );
    timeout(Duration::from_secs(5), peer).await???;
    Ok(())
}

#[test]
fn production_and_non_loopback_origins_are_categorically_rejected() -> TestResult {
    let resolved = resolved_fixture()?;
    let client = Client::new(Limits::default())?;
    assert!(
        AnthropicLocalHttpServiceFactory::new(
            client.clone(),
            Origin::parse("https://api.anthropic.com")?,
            resolved.route().clone(),
            prepared(&resolved)?,
        )
        .is_err()
    );
    assert!(
        AnthropicLocalHttpServiceFactory::new(
            client,
            Origin::parse("https://127.0.0.1")?,
            resolved.route().clone(),
            prepared(&resolved)?,
        )
        .is_err()
    );
    Ok(())
}

fn fixture_oauth_identity() -> plakous_anthropic::ClaudeOAuthRequestIdentity {
    plakous_anthropic::ClaudeOAuthRequestIdentity::new(
        plakous_anthropic::ClaudeAccountUuid::new("12345678-1234-4234-8234-123456789abc")
            .expect("fixture account UUID"),
        plakous_anthropic::ClaudeClientDeviceId::new("ab".repeat(32)).expect("fixture device ID"),
    )
}

#[tokio::test]
async fn terminal_retry_advice_and_billing_bodies_never_replay() -> TestResult {
    for (kind, header) in [
        ("rate_limit_error", "x-should-retry: false\r\n"),
        ("billing_error", ""),
        ("usage_not_included", ""),
    ] {
        let body = format!(
            r#"{{"type":"error","error":{{"type":"{kind}","message":"fixture refusal"}}}}"#
        );
        let response = format!("HTTP/1.1 429 Fixture\r\ncontent-type: application/json\r\nrequest-id: req_terminal\r\nretry-after: 0\r\n{header}content-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).into_bytes();
        let (client, origin, peer) = tls_exchange_sequence(vec![response]).await?;
        let resolved = resolved_fixture()?;
        let factory = AnthropicLocalHttpServiceFactory::new(
            client,
            origin,
            resolved.route().clone(),
            prepared(&resolved)?,
        )?
        .with_retry(Arc::new(|route| {
            Box::pin(async move { Ok(AnthropicRetryGrant::for_api_key(route)) })
        }));
        let (result, _) = timeout(Duration::from_secs(5), run(&factory)).await??;
        assert_eq!(
            result
                .expect_err("terminal refusal")
                .facts()
                .ok_or("facts")?
                .physical_attempts(),
            1
        );
        assert_eq!(timeout(Duration::from_secs(5), peer).await???, 1);
    }
    Ok(())
}

#[tokio::test]
async fn a_session_workflow_retries_only_a_provably_unsent_request() -> TestResult {
    for fault in ["connect", "write-reset", "record-rejected"] {
        let (client, origin, peer) = tls_exchange_sequence(vec![accepted_response()]).await?;
        let client = client.with_faults(Some(FaultPlan::parse(&format!("fault={fault},nth=1"))?));
        let resolved = resolved_fixture()?;
        let factory = AnthropicLocalHttpServiceFactory::new(
            client,
            origin,
            resolved.route().clone(),
            prepared_with_metadata(
                &resolved,
                99,
                "fixture",
                RequestMetadata::for_session_workflow(
                    RunId::try_from(Uuid::from_u128(2))?,
                    plakous_model::SessionWorkflowRootId::new([7; 32])?,
                    AttemptId::try_from(Uuid::from_u128(4))?,
                    Timestamp::new(2, 0)?,
                ),
            )?,
        )?
        .with_retry(Arc::new(|route| {
            Box::pin(async move { Ok(AnthropicRetryGrant::for_api_key(route)) })
        }));
        let (result, events) = timeout(Duration::from_secs(5), run(&factory)).await??;
        assert_eq!(result?.facts().physical_attempts(), 2, "{fault}");
        if fault == "record-rejected" {
            let retry = events
                .iter()
                .find_map(|event| match event {
                    Ok(ModelEvent::Retry(retry)) => Some(retry),
                    _ => None,
                })
                .ok_or("retry marker")?;
            assert_eq!(retry.cause().kind(), "record_rejected_in_transit");
            assert_eq!(
                retry.cause().to_string(),
                "request corrupted in transit (TLS record rejected)"
            );
            assert_eq!(retry.delay(), Duration::ZERO);
        }
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Ok(ModelEvent::Retry(retry)) if retry.loss().declined_reason.is_none()))
        );
        assert_eq!(timeout(Duration::from_secs(5), peer).await???, 1);
    }
    Ok(())
}

#[tokio::test]
async fn a_session_workflow_retries_peer_rejected_records_only_before_response() -> TestResult {
    for (alert, retries) in [
        (TlsReply::BadMacMidBody, true),
        (TlsReply::BadMacBeforeHead, true),
        (TlsReply::BadMacAfterHead, false),
        (TlsReply::DecodeErrorBeforeHead, false),
    ] {
        let mut replies = vec![alert];
        if retries {
            replies.push(TlsReply::Http(accepted_response()));
        }
        let (client, origin, peer) = tls_reply_sequence(replies).await?;
        let resolved = resolved_fixture()?;
        let prepared = prepared_with_metadata(
            &resolved,
            99,
            &"x".repeat(128 * 1_024),
            RequestMetadata::for_session_workflow(
                RunId::try_from(Uuid::from_u128(2))?,
                plakous_model::SessionWorkflowRootId::new([7; 32])?,
                AttemptId::try_from(Uuid::from_u128(4))?,
                Timestamp::new(2, 0)?,
            ),
        )?;
        let factory = AnthropicLocalHttpServiceFactory::new(
            client,
            origin,
            resolved.route().clone(),
            prepared,
        )?
        .with_retry(Arc::new(|route| {
            Box::pin(async move { Ok(AnthropicRetryGrant::for_api_key(route)) })
        }));
        let (result, events) = timeout(Duration::from_secs(5), run(&factory)).await??;
        let expected = if retries { 2 } else { 1 };
        if retries {
            assert_eq!(result?.facts().physical_attempts(), expected);
        } else {
            assert_eq!(
                result
                    .expect_err("accepted or different alert cannot replay")
                    .facts()
                    .ok_or("facts")?
                    .physical_attempts(),
                expected
            );
        }
        let markers: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                Ok(ModelEvent::Retry(retry)) if retry.loss().declined_reason.is_none() => {
                    Some(retry)
                }
                _ => None,
            })
            .collect();
        assert_eq!(markers.len(), usize::from(retries));
        for marker in markers {
            assert_eq!(marker.cause().kind(), "record_rejected_in_transit");
            assert_eq!(marker.delay(), Duration::ZERO);
            assert!(marker.detail().as_str().contains("peer_rejected_record"));
            assert!(marker.detail().as_str().contains("RequestIncomplete"));
            assert_eq!(marker.max_physical(), 4);
        }
        assert_eq!(
            timeout(Duration::from_secs(5), peer).await???,
            expected as usize
        );
    }
    Ok(())
}

#[tokio::test]
async fn terminal_retry_advice_and_billing_on_success_heads_suppress_stream_replay() -> TestResult {
    let cut = SUCCESS_SSE
        .split("event: message_stop")
        .next()
        .ok_or("fixture terminal")?;
    let quota = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"billing_error\",\"message\":\"fixture billing\"}}\n\n";
    for (stream, header) in [(cut, "x-should-retry: false\r\n"), (quota, "")] {
        let response = String::from_utf8(sse_response(stream))?
            .replacen("request-id:", &format!("{header}request-id:"), 1)
            .into_bytes();
        let (client, origin, peer) = tls_exchange_sequence(vec![response]).await?;
        let resolved = resolved_fixture()?;
        let factory = AnthropicLocalHttpServiceFactory::new(
            client,
            origin,
            resolved.route().clone(),
            prepared(&resolved)?,
        )?
        .with_retry(Arc::new(|route| {
            Box::pin(async move { Ok(AnthropicRetryGrant::for_api_key(route)) })
        }));
        let (result, events) = timeout(Duration::from_secs(5), run(&factory)).await??;
        assert_eq!(
            result
                .expect_err("terminal stream")
                .facts()
                .ok_or("facts")?
                .physical_attempts(),
            1
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Ok(ModelEvent::Retry(retry)) if retry.loss().declined_reason.is_none()))
        );
        assert_eq!(timeout(Duration::from_secs(5), peer).await???, 1);
    }
    Ok(())
}

#[tokio::test]
async fn an_unsent_request_without_retry_revalidation_is_not_resent() -> TestResult {
    let (client, origin, mut peer) = tls_exchange_sequence(vec![accepted_response()]).await?;
    let client = client.with_faults(Some(FaultPlan::parse("fault=connect,nth=1")?));
    let resolved = resolved_fixture()?;
    let factory = AnthropicLocalHttpServiceFactory::new(
        client,
        origin,
        resolved.route().clone(),
        prepared(&resolved)?,
    )?;
    let (result, events) = timeout(Duration::from_secs(5), run(&factory)).await??;
    assert_eq!(
        result
            .expect_err("resend requires revalidation")
            .facts()
            .ok_or("facts")?
            .physical_attempts(),
        1
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Ok(ModelEvent::Retry(retry)) if retry.loss().declined_reason.is_none()))
    );
    assert!(
        timeout(Duration::from_millis(100), &mut peer)
            .await
            .is_err()
    );
    peer.abort();
    assert!(peer.await.expect_err("cancelled peer").is_cancelled());
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn messages_unreachable_ride_out_excludes_incomplete_requests() -> TestResult {
    for workflow in [false, true] {
        for (fault, expected) in [("connect", 12), ("write-reset", 4), ("record-rejected", 4)] {
            let client = Client::new(Limits::default())?
                .with_faults(Some(FaultPlan::parse(&format!("fault={fault}"))?));
            let resolved = resolved_fixture()?;
            let factory = AnthropicLocalHttpServiceFactory::new(
                client,
                Origin::parse("https://127.0.0.1:9")?,
                resolved.route().clone(),
                if workflow {
                    prepared_with_metadata(
                        &resolved,
                        11,
                        "workflow",
                        RequestMetadata::for_session_workflow(
                            RunId::try_from(Uuid::from_u128(2))?,
                            plakous_model::SessionWorkflowRootId::new([7; 32])?,
                            AttemptId::try_from(Uuid::from_u128(4))?,
                            Timestamp::new(2, 0)?,
                        ),
                    )?
                } else {
                    prepared(&resolved)?
                },
            )?
            .with_retry(Arc::new(|route| {
                Box::pin(async move { Ok(AnthropicRetryGrant::for_api_key(route)) })
            }));
            let started = tokio::time::Instant::now();
            let (result, _) = run(&factory).await?;
            let error = result.expect_err("injected outage");
            let facts = error.facts().ok_or("facts")?;
            assert_eq!(facts.physical_attempts(), expected, "{fault}");
            assert_eq!(
                facts.summary().ride_out_physical,
                if fault == "connect" { expected } else { 0 }
            );
            assert!(started.elapsed() <= Duration::from_secs(240));
        }
    }
    Ok(())
}
