//! End-to-end evidence for the sealed `OpenAI` Platform local TLS authority.

#![cfg(feature = "native-auth")]

use plakous_http::{Client, Limits, TlsConfigIdentity};
use plakous_model::{
    Actor, AttemptId, CachePolicy, Clock, ContentBlock, ContextAdmissionRequirement,
    ConversationItem, ConversationView, CredentialSelectionId, Digest, DigestDomain,
    EffectiveProviderConfigurationId, EntryId, GenerationLimits, LanguageModel,
    LocalDurabilityDisposition, ModelAdapterProfileId, ModelPreparationContext,
    ModelPreparationError, ModelRequest, NamespaceId, NativeOptions, OutputBoundDisposition,
    OutputPolicy, ParallelToolDisposition, ProtocolId, Provenance, ProviderAuthorityClassId,
    ProviderBehaviorConfiguration, ProviderProfileRevisionId, ProviderRouteSelection,
    ProviderStorageDisposition, ReasoningEffortDisposition, ReasoningIncludeDisposition,
    ReasoningSummaryDisposition, ReplayDisposition, RequestMetadata, ResponseStateDisposition,
    RetentionDisposition, RouteId, RunId, SelectedProviderConfiguration,
    SelectedProviderConfigurationId, SystemClock, TextBlock, ToolChoice, TurnId, ValidationMode,
};
use plakous_openai::{
    AuthError, CredentialAuthorityId, CredentialReference, EndpointOrigin,
    OPENAI_PLATFORM_AUTH_PROFILE_ID, OPENAI_PLATFORM_AUTH_REVISION,
    OPENAI_PLATFORM_AUTHORITY_CLASS, OPENAI_PLATFORM_CREDENTIAL_SELECTION,
    OPENAI_RESPONSES_ADAPTER_REVISION, OpenAiApiKeyGenerationGrant, OpenAiPlatformLocalModel,
    OpenAiPlatformModelBuilder, OpenAiPlatformModelFacts, OpenAiProjectId,
    PlatformCredentialSource, PlatformProjectPolicy, PlatformStoragePolicy, SecretString,
};
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use std::collections::BTreeMap;
use std::error::Error;
use std::io;
use std::num::NonZeroU32;
use std::str::FromStr as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

const MODEL: &str = "gpt-5.6-sol";

struct CountingSource {
    authority: CredentialAuthorityId,
    secret_resolutions: AtomicUsize,
    generation_grants: AtomicUsize,
}

impl CountingSource {
    fn new() -> TestResult<Self> {
        Ok(Self {
            authority: CredentialAuthorityId::new("openai-platform-local-authority")?,
            secret_resolutions: AtomicUsize::new(0),
            generation_grants: AtomicUsize::new(0),
        })
    }
}

impl PlatformCredentialSource for CountingSource {
    fn credential_authority(&self) -> &CredentialAuthorityId {
        &self.authority
    }

    fn api_key(
        &self,
        _reference: &CredentialReference,
        _origin: &EndpointOrigin,
    ) -> Result<SecretString, AuthError> {
        self.secret_resolutions.fetch_add(1, Ordering::SeqCst);
        SecretString::new("real-key-resolution-must-not-run").map_err(|_| AuthError::SourceFailure)
    }

    fn generation_grant(
        &self,
        _reference: &CredentialReference,
        _origin: &EndpointOrigin,
    ) -> Result<OpenAiApiKeyGenerationGrant, AuthError> {
        self.generation_grants.fetch_add(1, Ordering::SeqCst);
        OpenAiApiKeyGenerationGrant::from_generation_binding(b"openai-local-generation-v1")
            .map_err(|_| AuthError::SourceFailure)
    }
}

fn selected_configuration(
    authority: &plakous_openai::OpenAiPlatformConfigurationAuthority,
) -> TestResult<SelectedProviderConfiguration> {
    let descriptor = authority.descriptor();
    let revision = ProviderProfileRevisionId::new(OPENAI_RESPONSES_ADAPTER_REVISION)?;
    selected_with_behavior(
        authority,
        ProviderBehaviorConfiguration {
            processing_tier: plakous_model::ProcessingTierDisposition::Standard,
            tool_mode: plakous_model::ToolPresentationMode::Direct,
            reasoning_effort: ReasoningEffortDisposition::Disabled,
            reasoning_summary: ReasoningSummaryDisposition::Omit,
            reasoning_include: ReasoningIncludeDisposition::Omit,
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
            local_durability: LocalDurabilityDisposition::Ephemeral,
        },
    )
}

fn selected_with_behavior(
    authority: &plakous_openai::OpenAiPlatformConfigurationAuthority,
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
) -> TestResult<ModelRequest> {
    request_with_bound(selected, effective, NonZeroU32::new(128))
}

fn request_with_bound(
    selected: SelectedProviderConfigurationId,
    effective: EffectiveProviderConfigurationId,
    output_bound: Option<NonZeroU32>,
) -> TestResult<ModelRequest> {
    request_with_options(
        selected,
        effective,
        output_bound,
        Vec::new(),
        NativeOptions::default(),
    )
}

fn request_with_options(
    selected: SelectedProviderConfigurationId,
    effective: EffectiveProviderConfigurationId,
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
        ConversationView::new(vec![input]),
        RequestMetadata::for_turn(
            RunId::from_str("00000000-0000-4000-8000-000000000101")?,
            TurnId::from_str("00000000-0000-4000-8000-000000000102")?,
            AttemptId::from_str("00000000-0000-4000-8000-000000000103")?,
            SystemClock.now(),
        ),
    )
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

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one e2e keeps local TLS, exact wire, and terminal authority contiguous"
)]
async fn missing_context_contract_rejects_before_openai_tls_or_key_resolution() -> TestResult {
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
    let origin = format!("https://{}/", listener.local_addr()?);
    let acceptor = TlsAcceptor::from(Arc::new(server_config));
    let response_body = format!(
        concat!(
            "event: response.created\n",
            "data: {{\"type\":\"response.created\",\"sequence_number\":0,\"response\":{{\"id\":\"resp_openai_local\",\"object\":\"response\",\"status\":\"in_progress\",\"model\":\"{}\"}}}}\n\n",
            "event: response.completed\n",
            "data: {{\"type\":\"response.completed\",\"sequence_number\":1,\"response\":{{\"id\":\"resp_openai_local\",\"object\":\"response\",\"status\":\"completed\",\"model\":\"{}\",\"output\":[],\"usage\":{{\"input_tokens\":8,\"output_tokens\":0,\"total_tokens\":8}}}}}}\n\n"
        ),
        MODEL, MODEL
    );
    assert!(!response_body.contains("[DONE]"));
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nx-request-id: req_openai_local\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        response_body.len(),
        response_body
    )
    .into_bytes();
    let accepted_connections = Arc::new(AtomicUsize::new(0));
    let peer_connections = Arc::clone(&accepted_connections);
    let peer = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        peer_connections.fetch_add(1, Ordering::SeqCst);
        let mut stream = acceptor.accept(stream).await?;
        let request = read_wire_request(&mut stream).await?;
        stream.write_all(&response).await?;
        stream.shutdown().await?;
        Ok::<_, io::Error>(request)
    });

    let source = Arc::new(CountingSource::new()?);
    let reference = CredentialReference::application("openai-platform-local-fixture")?;
    let authority = OpenAiPlatformModelBuilder::new(
        reference,
        source.clone(),
        OpenAiPlatformModelFacts::Gpt56Sol,
        PlatformStoragePolicy::Stateless,
        PlatformProjectPolicy::Project(OpenAiProjectId::new("proj_local_fixture")?),
    )
    .seal_local_fixture(EndpointOrigin::parse(&origin)?)?;
    assert!(authority.descriptor().context_contract().is_none());
    let descriptor = authority.descriptor().clone();
    let selected = selected_configuration(&authority)?;
    let baseline_behavior = selected.behavior().clone();
    let variants = [
        {
            let mut value = baseline_behavior.clone();
            value.reasoning_summary = ReasoningSummaryDisposition::Auto;
            value
        },
        {
            let mut value = baseline_behavior.clone();
            value.reasoning_include = ReasoningIncludeDisposition::EncryptedContent;
            value
        },
        {
            let mut value = baseline_behavior.clone();
            value.replay = ReplayDisposition::Disabled;
            value
        },
        {
            let mut value = baseline_behavior.clone();
            value.cache = CachePolicy::Enabled {
                requirement: plakous_model::CapabilityRequirement::Required,
            };
            value
        },
        {
            let mut value = baseline_behavior.clone();
            value.tool_choice = ToolChoice::None;
            value
        },
        {
            let mut value = baseline_behavior.clone();
            value.output_bound = OutputBoundDisposition::NoRequestBound;
            value
        },
        {
            let mut value = baseline_behavior.clone();
            value.storage = ProviderStorageDisposition::ProviderStored;
            value
        },
        {
            let mut value = baseline_behavior.clone();
            value.response_state = ResponseStateDisposition::ProviderStored;
            value
        },
        {
            let mut value = baseline_behavior.clone();
            value.context_evidence_requirement =
                ContextAdmissionRequirement::ReleaseVerifiedRequired;
            value
        },
        {
            let mut value = baseline_behavior.clone();
            value.local_durability = LocalDurabilityDisposition::BestEffort;
            value
        },
    ];
    let variant_selections = variants
        .into_iter()
        .map(|behavior| selected_with_behavior(&authority, behavior))
        .collect::<TestResult<Vec<_>>>()?;
    let model = Arc::new(OpenAiPlatformLocalModel::new(authority, selected, client)?);
    let plan = descriptor.validate_request(
        request(
            model.configuration().selected_identity(),
            model.configuration().effective_identity(),
        )?,
        ValidationMode::Strict,
    )?;
    let runtime = model
        .clone()
        .revalidate_runtime_authority(SystemClock.now())
        .await?;
    let generation_before_rejections = source.generation_grants.load(Ordering::SeqCst);
    for variant in variant_selections {
        let foreign = request(
            variant.identity(),
            model.configuration().effective_identity(),
        )?;
        let plan = descriptor.validate_request(foreign, ValidationMode::Strict)?;
        assert!(
            model
                .clone()
                .prepare_call(
                    plan,
                    ModelPreparationContext::new(runtime.clone(), SystemClock.now()),
                )
                .is_err()
        );
    }
    let behavior_drift = request_with_bound(
        model.configuration().selected_identity(),
        model.configuration().effective_identity(),
        None,
    )?;
    let behavior_drift = descriptor.validate_request(behavior_drift, ValidationMode::Strict)?;
    assert!(
        model
            .clone()
            .prepare_call(
                behavior_drift,
                ModelPreparationContext::new(runtime.clone(), SystemClock.now()),
            )
            .is_err()
    );
    let same_ids_stop_drift = request_with_options(
        model.configuration().selected_identity(),
        model.configuration().effective_identity(),
        NonZeroU32::new(128),
        vec!["unsupported stop".to_owned()],
        NativeOptions::default(),
    )?;
    let same_ids_stop_drift =
        descriptor.validate_request(same_ids_stop_drift, ValidationMode::AllowDowngrade)?;
    assert!(
        model
            .clone()
            .prepare_call(
                same_ids_stop_drift,
                ModelPreparationContext::new(runtime.clone(), SystemClock.now()),
            )
            .is_err()
    );
    let same_ids_native_drift = request_with_options(
        model.configuration().selected_identity(),
        model.configuration().effective_identity(),
        NonZeroU32::new(128),
        Vec::new(),
        NativeOptions::new(BTreeMap::from([(
            NamespaceId::new("openai")?,
            serde_json::json!({"unsupported": true}),
        )])),
    )?;
    assert!(
        descriptor
            .validate_request(same_ids_native_drift, ValidationMode::AllowDowngrade)
            .is_err()
    );
    assert_eq!(
        source.generation_grants.load(Ordering::SeqCst),
        generation_before_rejections
    );
    assert_eq!(accepted_connections.load(Ordering::SeqCst), 0);
    let result = model.clone().prepare_call(
        plan,
        ModelPreparationContext::new(runtime, SystemClock.now()),
    );
    assert!(matches!(
        result,
        Err(ModelPreparationError::ContextAdmissionDenied { .. })
    ));
    peer.abort();
    assert_eq!(accepted_connections.load(Ordering::SeqCst), 0);
    assert_eq!(source.secret_resolutions.load(Ordering::SeqCst), 0);
    Ok(())
}

#[test]
fn local_authority_rejects_production_and_unported_origins() -> TestResult {
    let production_source = Arc::new(CountingSource::new()?);
    let production = OpenAiPlatformModelBuilder::new(
        CredentialReference::application("openai-platform-production-descriptor")?,
        production_source.clone(),
        OpenAiPlatformModelFacts::Gpt56Sol,
        PlatformStoragePolicy::Stateless,
        PlatformProjectPolicy::DefaultProject,
    )
    .seal()?;
    assert!(production.descriptor().context_contract().is_none());
    assert_eq!(
        production_source.secret_resolutions.load(Ordering::SeqCst),
        0
    );
    for origin in ["https://api.openai.com/", "https://127.0.0.1/"] {
        let source = Arc::new(CountingSource::new()?);
        let result = OpenAiPlatformModelBuilder::new(
            CredentialReference::application("openai-platform-local-negative")?,
            source.clone(),
            OpenAiPlatformModelFacts::Gpt56Sol,
            PlatformStoragePolicy::Stateless,
            PlatformProjectPolicy::DefaultProject,
        )
        .seal_local_fixture(EndpointOrigin::parse(origin)?);
        assert!(result.is_err());
        assert_eq!(source.secret_resolutions.load(Ordering::SeqCst), 0);
    }
    Ok(())
}
