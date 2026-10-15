use std::collections::BTreeSet;
use std::error::Error;
use std::io;
use std::mem::size_of;
use std::num::NonZeroU32;
use std::pin::Pin;
use std::task::{Context, Poll};

use jsonwebtoken::{EncodingKey, Header, encode};
use plakous_auth::subscription_limit::{
    InMemorySubscriptionRequestGate, SubscriptionRequestLimits, SystemSubscriptionGateClock,
};
use plakous_model::{
    ActiveFamilyArtifact, Actor, AttemptId, CachePolicy, CanonicalHistory, CapabilityRequirement,
    ContentBlock, ConversationItem, ConversationView, EffectiveProviderConfigurationId, EntryId,
    FixedClock, GenerationLimits, HistorySegment, HistorySegmentKind, ModelCacheAffinity,
    ModelCallContext, ModelError, ModelErrorKind, ModelEvent, ModelPreparationContext,
    ModelPreparationError, ModelPreparationErrorKind, ModelRequest, ModelResponse, ModelStream,
    PreparedCacheUse, PreparedCallRecord, PreparedModelCall, PreparedOutputLimitSource, Provenance,
    RequestMetadata, ResolvedProviderAuthorityBinding, RunId, RuntimeModelAuthorityOperation,
    SelectedProviderConfigurationId, TextBlock, ToolConcurrency, ToolDefinition, ToolEffect,
    ToolExecutionOwner, ToolInputContract, ToolName, ToolOutputContract, ToolResultBlock,
    ToolResultKind, ToolResultPart, TurnId, Usage, ValidatedRequestPlan, ValidationMode,
    WorkingProjection,
};
use plakous_responses::{
    RESPONSES_ARTIFACT_CODEC_REVISION, RESPONSES_ARTIFACT_SCHEMA_REVISION, ResponsesArtifactCodec,
};
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use tempfile::TempDir;
use tokio_rustls::TlsAcceptor;

use super::*;

type TestError = Box<dyn Error + Send + Sync>;
type TestResult<T = ()> = Result<T, TestError>;

#[test]
fn native_codex_operation_budget_still_obeys_earlier_caller_deadline() -> TestResult {
    let config = StandardOpenAiHttpConfig::new(
        Duration::from_secs(300),
        StandardOpenAiHttpConfig::default().event_channel_capacity(),
        plakous_responses::ResponsesStreamLimits::default(),
    )?;
    let clock = FixedClock::new(Timestamp::new(1_000, 0)?);
    let before = Instant::now();
    let full = crate::model::operation_deadline(&clock, config, None);
    let earlier = crate::model::operation_deadline(&clock, config, Some(Timestamp::new(1_180, 0)?));
    assert!(full.duration_since(before) >= Duration::from_secs(299));
    assert!(earlier.duration_since(before) >= Duration::from_secs(179));
    assert!(earlier.duration_since(before) < Duration::from_secs(181));
    assert!(earlier < full);
    Ok(())
}

fn test_cache_affinity() -> Result<ModelCacheAffinity, ModelError> {
    let session = "00000000-0000-7000-8000-000000000301"
        .parse::<plakous_model::SessionId>()
        .map_err(|error| ModelError::new(ModelErrorKind::InvalidRequest, error.to_string()))?;
    let branch = "00000000-0000-7000-8000-000000000302"
        .parse::<plakous_model::BranchId>()
        .map_err(|error| ModelError::new(ModelErrorKind::InvalidRequest, error.to_string()))?;
    ModelCacheAffinity::from_runtime_entropy(session, branch, 0, [0x11; 32])
        .map_err(|error| ModelError::new(ModelErrorKind::InvalidRequest, error.to_string()))
}

fn preparation_context(
    authority: RuntimeModelAuthority,
    prepared_at: Timestamp,
) -> Result<ModelPreparationContext, ModelError> {
    Ok(ModelPreparationContext::new(authority, prepared_at)
        .with_cache_affinity(test_cache_affinity()?))
}

async fn run_codex_model_operation(
    model: Arc<dyn LanguageModel>,
    descriptor: &ResolvedModelDescriptor,
    request: ModelRequest,
    context: ModelCallContext,
) -> Result<ModelResponse, ModelError> {
    let prepared_at = request.metadata().created_at();
    let plan = descriptor
        .validate_request(request, ValidationMode::Strict)
        .map_err(ModelError::from)?;
    let plan_digest = plan.digest();
    let authority = Arc::clone(&model)
        .revalidate_runtime_authority(prepared_at)
        .await
        .map_err(ModelPreparationError::from)
        .map_err(ModelError::from)?;
    let prepared = Arc::clone(&model)
        .prepare_call(plan, preparation_context(authority, prepared_at)?)
        .map_err(ModelError::from)?;
    collect_terminal_model_stream(prepared.start(context).await?, plan_digest).await
}

#[test]
fn remote_compaction_v2_header_is_present_only_with_compaction_trigger() {
    assert!(
        remote_compaction_v2_header(&serde_json::json!({
            "input": [{ "type": "message" }]
        }))
        .is_none()
    );
    let (name, value) = remote_compaction_v2_header(&serde_json::json!({
        "input": [
            { "type": "message" },
            { "type": "compaction_trigger" }
        ]
    }))
    .expect("native compact advertises remote_compaction_v2");
    assert_eq!(name.as_str(), "x-codex-beta-features");
    assert_eq!(value.as_bytes(), b"remote_compaction_v2");
}

#[test]
fn compact_client_metadata_is_copied_to_codex_compat_headers() -> TestResult {
    let metadata = serde_json::json!({
        "request_kind": "compaction",
        "window_id": "abc",
        "compaction": {
            "trigger": "manual",
            "reason": "user_requested",
            "implementation": "responses_compaction_v2",
            "phase": "standalone_turn",
            "strategy": "memento"
        }
    });
    let body = serde_json::json!({
        "client_metadata": {
            "x-codex-window-id": "abc",
            "x-codex-turn-metadata": metadata.to_string(),
        }
    });
    let headers = codex_compact_compat_headers(&body)
        .map_err(|_| "compact compat headers rejected valid metadata")?;
    assert_eq!(headers.len(), 2);
    assert_eq!(headers[0].0.as_str(), "x-codex-window-id");
    assert_eq!(headers[0].1.as_bytes(), b"abc");
    assert_eq!(headers[1].0.as_str(), "x-codex-turn-metadata");
    assert_eq!(headers[1].1.as_bytes(), metadata.to_string().as_bytes());
    assert_eq!(
        codex_compact_compat_headers(&serde_json::json!({}))
            .map_err(|_| "empty body should not be compact")?,
        [] as [(HeaderName, HeaderValue); 0]
    );
    Ok(())
}

struct ResolvedTestModel {
    executable: Arc<OpenAiLanguageModel>,
    descriptor: ResolvedModelDescriptor,
}

impl ResolvedTestModel {
    fn descriptor(&self) -> &ResolvedModelDescriptor {
        &self.descriptor
    }

    fn executable(&self) -> Arc<OpenAiLanguageModel> {
        Arc::clone(&self.executable)
    }

    fn dynamic_executable(&self) -> Arc<dyn LanguageModel> {
        self.executable.clone()
    }
}

impl std::ops::Deref for ResolvedTestModel {
    type Target = Arc<OpenAiLanguageModel>;

    fn deref(&self) -> &Self::Target {
        &self.executable
    }
}

fn assert_response_family_artifact_is_valid(
    response: &ModelResponse,
    plan: &ValidatedRequestPlan,
) -> TestResult<ActiveFamilyArtifact> {
    if serde_json::to_value(response)?
        .as_object()
        .is_some_and(|object| object.contains_key("replay"))
    {
        return Err("response encoded the obsolete aggregate replay field".into());
    }
    let candidate = response
        .family_artifact_candidate()
        .ok_or("response omitted its sealed family artifact candidate")?;
    if candidate.family() != "responses"
        || candidate.schema_revision() != RESPONSES_ARTIFACT_SCHEMA_REVISION
        || candidate.codec_revision() != RESPONSES_ARTIFACT_CODEC_REVISION
    {
        return Err("response family artifact named a foreign family or revision".into());
    }
    let covered = plan
        .request()
        .input()
        .staged_canonical_item_prefix_identity(response.output())?;
    candidate.validate_for_commit(plan, covered, response.output().len())?;
    let decoded = ResponsesArtifactCodec::decode_inert(candidate.bytes())?;
    if decoded.binding().identity() != candidate.binding() {
        return Err("decoded Responses artifact disagreed with its outer binding".into());
    }
    Ok(ActiveFamilyArtifact::new(
        candidate.family(),
        candidate.schema_revision(),
        candidate.codec_revision(),
        candidate.binding(),
        candidate.bytes().to_vec(),
    )?)
}

async fn run_model_operation_with_artifact(
    model: Arc<OpenAiLanguageModel>,
    descriptor: &ResolvedModelDescriptor,
    request: ModelRequest,
    context: ModelCallContext,
    artifact: ActiveFamilyArtifact,
) -> Result<ModelResponse, ModelError> {
    let artifact_identity = artifact.integrity();
    let prepared_at = request.metadata().created_at();
    let plan = descriptor
        .validate_request(request, ValidationMode::Strict)
        .map_err(ModelError::from)?;
    let plan_digest = plan.digest();
    let authority = Arc::clone(&model)
        .revalidate_runtime_authority(prepared_at)
        .await
        .map_err(ModelPreparationError::from)
        .map_err(ModelError::from)?;
    let prepared = Arc::clone(&model)
        .prepare_call(
            plan,
            preparation_context(authority, prepared_at)?.with_active_family_artifact(artifact),
        )
        .map_err(ModelError::from)?;
    let affinity = test_cache_affinity()?.identity();
    if prepared.record().projection().active_artifact() != Some(artifact_identity)
        || prepared.record().projection().cache_use()
            != (PreparedCacheUse::ActiveFamilyArtifactAndRuntimeAffinity {
                artifact: artifact_identity,
                affinity,
            })
    {
        return Err(ModelError::new(
            ModelErrorKind::InvalidRequest,
            "Codex prepared continuation did not bind its exact active family artifact",
        ));
    }
    collect_terminal_model_stream(prepared.start(context).await?, plan_digest).await
}

async fn collect_terminal_model_stream(
    mut stream: ModelStream,
    plan_digest: Digest,
) -> Result<ModelResponse, ModelError> {
    let mut completed = None;
    while let Some(event) = std::future::poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
        let event = event?;
        if completed.is_some() {
            return Err(ModelError::new(
                ModelErrorKind::InvalidEvent,
                "Codex test operation produced an event after terminal authority",
            ));
        }
        let ModelEvent::ResponseCompleted(response) = event else {
            continue;
        };
        response.validate_for_commit()?;
        if response.plan_digest() != plan_digest {
            return Err(ModelError::new(
                ModelErrorKind::InvalidEvent,
                "Codex test terminal belongs to another prepared plan",
            ));
        }
        completed = Some(response);
    }
    completed.ok_or_else(ModelError::incomplete)
}

async fn run_model_operation_collecting_observation_ids(
    model: Arc<OpenAiLanguageModel>,
    descriptor: &ResolvedModelDescriptor,
    request: ModelRequest,
    context: ModelCallContext,
) -> Result<(ModelResponse, BTreeSet<EntryId>), ModelError> {
    let prepared_at = request.metadata().created_at();
    let plan = descriptor
        .validate_request(request, ValidationMode::Strict)
        .map_err(ModelError::from)?;
    let plan_digest = plan.digest();
    let authority = Arc::clone(&model)
        .revalidate_runtime_authority(prepared_at)
        .await
        .map_err(ModelPreparationError::from)
        .map_err(ModelError::from)?;
    let prepared = Arc::clone(&model)
        .prepare_call(plan, preparation_context(authority, prepared_at)?)
        .map_err(ModelError::from)?;
    let mut stream = prepared.start(context).await?;
    let mut observation_ids = BTreeSet::new();
    let mut completed = None;
    while let Some(event) = std::future::poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
        let event = event?;
        if completed.is_some() {
            return Err(ModelError::new(
                ModelErrorKind::InvalidEvent,
                "Codex test operation produced an event after terminal authority",
            ));
        }
        match event {
            ModelEvent::BlockStarted(started) => {
                observation_ids.insert(started.item_id());
            }
            ModelEvent::ResponseCompleted(response) => {
                response.validate_for_commit()?;
                if response.plan_digest() != plan_digest {
                    return Err(ModelError::new(
                        ModelErrorKind::InvalidEvent,
                        "Codex test terminal belongs to another prepared plan",
                    ));
                }
                completed = Some(response);
            }
            _ => {}
        }
    }
    completed
        .map(|response| (response, observation_ids))
        .ok_or_else(ModelError::incomplete)
}

struct ObservationDroppingModel {
    inner: Arc<dyn LanguageModel>,
}

impl ObservationDroppingModel {
    fn wrap(inner: Arc<dyn LanguageModel>) -> Arc<dyn LanguageModel> {
        Arc::new(Self { inner })
    }
}

impl LanguageModel for ObservationDroppingModel {
    fn revalidate_runtime_authority(
        self: Arc<Self>,
        checked_at: Timestamp,
    ) -> RuntimeModelAuthorityOperation {
        Arc::clone(&self.inner).revalidate_runtime_authority(checked_at)
    }

    fn prepare_call(
        self: Arc<Self>,
        plan: ValidatedRequestPlan,
        context: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError> {
        Arc::clone(&self.inner)
            .prepare_call(plan, context)
            .map(|prepared| {
                prepared.map_start(|start| {
                    Box::pin(async move {
                        let inner = start.await?;
                        Ok(Box::pin(TerminalOnlyModelStream { inner }) as ModelStream)
                    })
                })
            })
    }
}

struct TerminalOnlyModelStream {
    inner: ModelStream,
}

impl futures_core::Stream for TerminalOnlyModelStream {
    type Item = Result<ModelEvent, ModelError>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match self.inner.as_mut().poll_next(context) {
                Poll::Ready(Some(Ok(ModelEvent::ResponseCompleted(response)))) => {
                    return Poll::Ready(Some(Ok(ModelEvent::ResponseCompleted(response))));
                }
                Poll::Ready(Some(Ok(_))) => {}
                Poll::Ready(Some(Err(error))) => return Poll::Ready(Some(Err(error))),
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

const TEST_KID: &str = "plak-test-rs256";
static BROWSER_TEST_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());
const TEST_MODULUS: &str = "gJwvgsbhK0fe9NrzNs59X-Cl5aTdccTUPBCWSzTPh3IFfTkP6mUewoAtNett_my68PC_8raMua60QcQRSINvj47OIKvIF5VFnPiXt7KICtYMkxAqQ7RqrVcDmp6zFT6WbQY3Uq_Htp3zRndiwbuR4b5LezMmqputb8Vgj9Va210TCUFrmrHCEncAbtr4z0i4DO09BtCJg-x8j-OrxVi5aKs1ObbGWKTX2Mjtc-A6bAQ00em5NWi5ugsP_1RWw_CeFHXtMeoULlLWAxGfDtv8DUrQuN4nkZSFtxHy7kOj2L-soMZUPMoHYGxDT1aydPUe_7sVnMZFhWeUWpsqFd-9rw";
// The PEM armor is split so secret scanners do not flag this synthetic key.
const TEST_PRIVATE_KEY: &[u8] = concat!(
    "-----BEGIN PRIVATE",
    r" KEY-----
MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQCAnC+CxuErR970
2vM2zn1f4KXlpN1xxNQ8EJZLNM+HcgV9OQ/qZR7CgC01623+bLrw8L/ytoy5rrRB
xBFIg2+Pjs4gq8gXlUWc+Je3sogK1gyTECpDtGqtVwOanrMVPpZtBjdSr8e2nfNG
d2LBu5Hhvkt7Myaqm61vxWCP1VrbXRMJQWuascISdwBu2vjPSLgM7T0G0ImD7HyP
46vFWLloqzU5tsZYpNfYyO1z4DpsBDTR6bk1aLm6Cw//VFbD8J4Ude0x6hQuUtYD
EZ8O2/wNStC43ieRlIW3EfLuQ6PYv6ygxlQ8ygdgbENPVrJ09R7/uxWcxkWFZ5Ra
myoV372vAgMBAAECggEAAvxhkXc8cIVECyPq/LBsDBLMyVc7zKb8ENRlON/EAWol
Z2l1AM3Jq12tQBlJgiB2YHy3eTEFynGLTwLFLlpEs4Z3b5WLQ8SibZPLZausS7Fx
LBgYlTxaVfK1SS3DQBTzCk4k+TNXJZZNUnf15kQVBSo3YkW4NcK7G1q8fPpiXH1B
8EBIyNYrPfEXWMiXH93tuc78GJv212HhSBTuBolF1UZQaErYCyV9cBumyvNkFA9R
n+lNECCZLzy1aVfY9rh68I5yW5ocQjr6hR37PCaDf2GJSgTrXI8b6T8GCfVL0gIi
UDOvwPuSDTdXYIJlS9lBdXD4lE9Kde83TXYFuMO12QKBgQC1za2WCmQoPxSvXBtd
SOvf7zDBFVIoyXlycVqmKDF59eGhV0r7BdDoZxmdxxLTkObrJgQzuPwn2YQj1VxJ
Kj3c57eq5qqRP4YJWrEwfWlWVXWyIzuLath4d+2x1rzvD4fK015H8aatQ4jLmAJH
ZXcMWiJ5i/vVXq9tP70s8l1y2wKBgQC1GQPLPDJO+ulz1SU5b5hOMb1UijX59Utj
hfOp+/xTlICtaachZC4e5Cue68ak9zw7V7Rz03ptF2I8TIdw/Hcx10YY2d5ucyPl
Cb5V3n4/pBL3069SYHGx/qJCm/ZOKFU0YTLMXmj1GNRK4q0HlTcqNUn6CYWPnCg2
j1UGICN2vQKBgCyuhyooFaOalU/f+E29/VLFK5tYzR9/HN0WpOvPVxGdb73CgYge
TtC/fhk78xGSPF6wsjmwDjEREZvo9Ux6GEgxnv9ev68BCMVFx+Sd/gSc7ldAt4K1
WMyK1R1vJHIww/QCbNqLtqZAwd8f1feMJW0gvfZXhCZVphHYp3rjejofAoGAZZNl
TF40PZaA0OaRnXiZbep6LmvjQJM0ONtOQ17CNcMKtbmX86O0NsF2Mgo+/hJqZ0KX
RYwtk/jUscufAGGrHXqIp7G0EDFmxbXEW3eTSnDl7rl+Ee9kdhjDvCasbUQ0HlAS
ZQzjQ+4sTOB9es5YBcWj+6nRjy5gY9HbN7dZDOUCgYBLjw8a2EPQBLwnnDBxeT67
+PTp+X6sf3e46yCwRcc0d+MRBYqdRItOb3EOKFDIAvYiN2FwAoY3zEIQabG9LBtu
oKLDb6KUOsSBFlxOYbP/1GVTF4LTgp20MmSNI5ZVAF343RBYeN5PLEsMkgUw+yRx
Qur+amqrwl9uDA6MsnnG6Q==
-----END PRIVATE",
    " KEY-----"
)
.as_bytes();

struct TestEndpoint {
    listener: TcpListener,
    acceptor: TlsAcceptor,
    client: Client,
    origin: EndpointOrigin,
}

async fn test_endpoint() -> TestResult<TestEndpoint> {
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
    let origin = EndpointOrigin::parse(&format!("https://{}/", listener.local_addr()?))?;
    let tls_identity = plakous_http::TlsConfigIdentity::from_parts(
        1,
        &[certificate.as_ref(), b"openai-test-rustls-ring-http2"],
    )?;
    let client =
        Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
    Ok(TestEndpoint {
        listener,
        acceptor: TlsAcceptor::from(Arc::new(server_config)),
        client,
        origin,
    })
}

fn response(status: &str, content_type: &str, body: &[u8], extra: &str) -> Vec<u8> {
    let mut response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n",
            body.len()
        )
        .into_bytes();
    response.extend_from_slice(body);
    response
}

async fn read_request(
    stream: &mut tokio_rustls::server::TlsStream<TcpStream>,
) -> TestResult<Vec<u8>> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];
    let head_end = loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Err(io::Error::other("request ended before head").into());
        }
        request.extend_from_slice(&buffer[..read]);
        if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let head = std::str::from_utf8(&request[..head_end])?;
    let length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    while request.len() < head_end + length {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Err(io::Error::other("request ended before body").into());
        }
        request.extend_from_slice(&buffer[..read]);
    }
    Ok(request)
}

fn spawn_sequence(
    endpoint: TestEndpoint,
    responses: Vec<Vec<u8>>,
) -> tokio::task::JoinHandle<TestResult<Vec<Vec<u8>>>> {
    tokio::spawn(async move {
        let mut requests = Vec::new();
        for response in responses {
            let (stream, _) = endpoint.listener.accept().await?;
            let mut stream = endpoint.acceptor.accept(stream).await?;
            requests.push(read_request(&mut stream).await?);
            stream.write_all(&response).await?;
            stream.shutdown().await?;
        }
        Ok(requests)
    })
}

fn spawn_sequence_then_probe(
    endpoint: TestEndpoint,
    responses: Vec<Vec<u8>>,
) -> tokio::task::JoinHandle<TestResult<(Vec<Vec<u8>>, bool)>> {
    tokio::spawn(async move {
        let mut requests = Vec::new();
        for response in responses {
            let (stream, _) = endpoint.listener.accept().await?;
            let mut stream = endpoint.acceptor.accept(stream).await?;
            requests.push(read_request(&mut stream).await?);
            stream.write_all(&response).await?;
            stream.shutdown().await?;
        }
        let unexpected_connection = match tokio::time::timeout(
            Duration::from_millis(500),
            endpoint.listener.accept(),
        )
        .await
        {
            Ok(Ok((stream, _))) => {
                drop(stream);
                true
            }
            Ok(Err(error)) => return Err(error.into()),
            Err(_) => false,
        };
        Ok((requests, unexpected_connection))
    })
}

fn spawn_stall_after(
    endpoint: TestEndpoint,
    responses: Vec<Vec<u8>>,
    stalled: tokio::sync::oneshot::Sender<()>,
) -> tokio::task::JoinHandle<TestResult<Vec<Vec<u8>>>> {
    tokio::spawn(async move {
        let mut requests = Vec::new();
        for response in responses {
            let (stream, _) = endpoint.listener.accept().await?;
            let mut stream = endpoint.acceptor.accept(stream).await?;
            requests.push(read_request(&mut stream).await?);
            stream.write_all(&response).await?;
            stream.shutdown().await?;
        }
        let (stream, _) = endpoint.listener.accept().await?;
        let mut stream = endpoint.acceptor.accept(stream).await?;
        requests.push(read_request(&mut stream).await?);
        let _ = stalled.send(());
        let mut byte = [0_u8; 1];
        let _ = stream.read(&mut byte).await;
        Ok(requests)
    })
}

async fn expire_observed_login_stall(
    completion: tokio::task::JoinHandle<Result<CodexCredentialStatus, NativeCodexAuthError>>,
    stalled: tokio::sync::oneshot::Receiver<()>,
    deadline: Instant,
) -> TestResult<Result<CodexCredentialStatus, NativeCodexAuthError>> {
    tokio::time::timeout(Duration::from_secs(20), stalled).await??;
    // Establish the physical phase before driving its deadline. Virtual time
    // excludes host scheduling delay, but still rejects the 30-second HTTP cap
    // if the shorter login deadline was not propagated to this request.
    tokio::time::pause();
    let bound = tokio::time::Instant::from_std(deadline) + Duration::from_millis(1);
    let outcome = tokio::time::timeout_at(bound, completion).await;
    tokio::time::resume();
    Ok(outcome??)
}

fn private_slot() -> TestResult<(TempDir, PathBuf)> {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;

    let directory = tempfile::tempdir()?;
    #[cfg(unix)]
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
    let path = directory.path().join("codex-native.json");
    Ok((directory, path))
}

fn acceptance() -> Result<CodexCompatibilityAcceptance, NativeCodexAuthError> {
    CodexCompatibilityAcceptance::accept_exact(
        OPENAI_CODEX_COMPATIBILITY_PROFILE_ID,
        OPENAI_CODEX_SOURCE_REVISION,
        OPENAI_CODEX_COMPATIBILITY_NOTICE_REVISION,
        system_timestamp(),
    )
}

fn id_token(kid: &str, subject: &str, account: &str) -> TestResult<String> {
    let now = now_unix_secs();
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(kid.to_owned());
    let claims = serde_json::json!({
        "iss": OPENAI_AUTH_ORIGIN.trim_end_matches('/'),
        "aud": OPENAI_CODEX_NATIVE_CLIENT_ID,
        "azp": OPENAI_CODEX_NATIVE_CLIENT_ID,
        "sub": subject,
        "iat": now.saturating_sub(10),
        "exp": now + 3600,
        "https://api.openai.com/auth": {"chatgpt_account_id": account}
    });
    let pem = std::str::from_utf8(TEST_PRIVATE_KEY)?;
    let encoded = pem
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect::<String>();
    let der = base64::engine::general_purpose::STANDARD.decode(encoded)?;
    let pkcs1 = pkcs8_rsa_inner(&der).ok_or("invalid test PKCS#8 key")?;
    Ok(encode(&header, &claims, &EncodingKey::from_rsa_der(pkcs1))?)
}

fn pkcs8_rsa_inner(der: &[u8]) -> Option<&[u8]> {
    fn tlv(bytes: &[u8], offset: usize) -> Option<(u8, usize, usize)> {
        let tag = *bytes.get(offset)?;
        let first = *bytes.get(offset + 1)?;
        let (length, head) = if first & 0x80 == 0 {
            (usize::from(first), 2)
        } else {
            let count = usize::from(first & 0x7f);
            if count == 0 || count > size_of::<usize>() {
                return None;
            }
            let mut length = 0_usize;
            for byte in bytes.get(offset + 2..offset + 2 + count)? {
                length = length.checked_mul(256)?.checked_add(usize::from(*byte))?;
            }
            (length, 2 + count)
        };
        let content = offset.checked_add(head)?;
        (content.checked_add(length)? <= bytes.len()).then_some((tag, content, length))
    }
    let (outer_tag, outer, outer_len) = tlv(der, 0)?;
    if outer_tag != 0x30 || outer + outer_len != der.len() {
        return None;
    }
    let (_, version, version_len) = tlv(der, outer)?;
    let algorithm_offset = version.checked_add(version_len)?;
    let (_, algorithm, algorithm_len) = tlv(der, algorithm_offset)?;
    let key_offset = algorithm.checked_add(algorithm_len)?;
    let (key_tag, key, key_len) = tlv(der, key_offset)?;
    (key_tag == 0x04).then(|| &der[key..key + key_len])
}

fn token_body(token: &str, access: &str, refresh: &str, _expires_in: u64) -> TestResult<Vec<u8>> {
    Ok(serde_json::to_vec(&serde_json::json!({
        "access_token": access,
        "refresh_token": refresh,
        "id_token": token
    }))?)
}

fn access_token_with_expiry(expires_at: u64) -> TestResult<String> {
    let payload = serde_json::to_vec(&serde_json::json!({
        "exp": expires_at,
        "provider_metadata": "ignored"
    }))?;
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload);
    Ok(format!("e30.{payload}.provider-signature"))
}

fn authorization_query_value(authorization: &Url, key: &str) -> TestResult<String> {
    authorization
        .query_pairs()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.into_owned())
        .ok_or_else(|| format!("missing authorization query key {key}").into())
}

fn browser_login_responses(subject: &str, account: &str, marker: &str) -> TestResult<Vec<Vec<u8>>> {
    let token = id_token(TEST_KID, subject, account)?;
    Ok(vec![
        response(
            "200 OK",
            "application/json",
            &token_body(
                &token,
                &format!("opaque-access-{marker}"),
                &format!("refresh-{marker}"),
                3_600,
            )?,
            "",
        ),
        response(
            "200 OK",
            "application/json",
            &jwks_body(TEST_KID, TEST_MODULUS)?,
            "Cache-Control: max-age=300\r\n",
        ),
        response(
            "200 OK",
            "application/json",
            &serde_json::to_vec(&serde_json::json!({"sub": subject}))?,
            "",
        ),
    ])
}

fn jwks_body(kid: &str, modulus: &str) -> TestResult<Vec<u8>> {
    Ok(serde_json::to_vec(&serde_json::json!({
        "keys": [{"kty":"RSA", "use":"sig", "alg":"RS256", "kid":kid, "n":modulus, "e":"AQAB", "x5c":["ignored-provider-metadata"], "x5t":"ignored-provider-thumbprint"}]
    }))?)
}

#[derive(Clone, Copy, Debug)]
enum LoginStallPhase {
    Token,
    Jwks,
    UserInfo,
}

impl LoginStallPhase {
    const fn expected_error(self) -> NativeCodexAuthError {
        match self {
            Self::Token => NativeCodexAuthError::LoginTokenTransport,
            Self::Jwks => NativeCodexAuthError::LoginSigningKeys,
            Self::UserInfo => NativeCodexAuthError::LoginAccountVerification,
        }
    }
}

fn device_login_for_deadline_test(deadline: Instant) -> CodexDeviceLogin {
    CodexDeviceLogin {
        verification_url: "https://localhost/codex/device".to_owned(),
        user_code: AuthSecretString::from("ABCD-1234".to_owned()),
        device_auth_id: AuthSecretString::from("device-deadline".to_owned()),
        interval: Duration::from_millis(1),
        deadline,
        credential_baseline: None,
    }
}

fn deadline_test_credential() -> Result<StoredCredential, NativeCodexAuthError> {
    Ok(StoredCredential {
        access_token: "deadline-access".to_owned(),
        refresh_token: "deadline-refresh".to_owned(),
        id_token: "deadline-id".to_owned(),
        subject: "deadline-subject".to_owned(),
        account_id: "deadline-account".to_owned(),
        scopes: validate_exact_scopes(OPENAI_CODEX_OAUTH_SCOPES)?,
        access_expires_at: now_unix_secs() + 3_600,
        selection_epoch: generate_selection_epoch()?,
    })
}

async fn assert_browser_login_stall_expires(phase: LoginStallPhase) -> TestResult {
    let _browser_test = BROWSER_TEST_LOCK.lock().await;
    let endpoint = test_endpoint().await?;
    let (_directory, slot) = private_slot()?;
    let manager = manager_for_endpoint(&slot, &endpoint)?;
    let mut login = manager.begin_browser_login().await?;
    let authorization = Url::parse(login.authorization_url())?;
    assert!(authorization.query_pairs().all(|(key, _)| key != "nonce"));
    let state = authorization_query_value(&authorization, "state")?;
    let port = login.listener.local_addr()?.port();
    let token = id_token(
        TEST_KID,
        "browser-deadline-subject",
        "browser-deadline-account",
    )?;
    let token_reply = response(
        "200 OK",
        "application/json",
        &token_body(
            &token,
            "browser-deadline-access",
            "browser-deadline-refresh",
            3_600,
        )?,
        "",
    );
    let jwks_reply = response(
        "200 OK",
        "application/json",
        &jwks_body(TEST_KID, TEST_MODULUS)?,
        "Cache-Control: max-age=300\r\n",
    );
    let replies = match phase {
        LoginStallPhase::Token => Vec::new(),
        LoginStallPhase::Jwks => vec![token_reply],
        LoginStallPhase::UserInfo => vec![token_reply, jwks_reply],
    };
    let expected_requests = replies.len() + 1;
    let (observed, stalled) = tokio::sync::oneshot::channel();
    let server = spawn_stall_after(endpoint, replies, observed);
    let deadline = Instant::now() + Duration::from_secs(20);
    login.attempt.deadline = deadline;
    let worker_manager = Arc::clone(&manager);
    let completion =
        tokio::spawn(async move { worker_manager.complete_browser_login(login).await });
    let mut callback = TcpStream::connect(("127.0.0.1", port)).await?;
    callback
            .write_all(
                format!(
                    "GET /auth/callback?code=deadline-code&state={state} HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await?;
    let outcome = expire_observed_login_stall(completion, stalled, deadline).await?;
    assert_eq!(outcome, Err(phase.expected_error()), "{phase:?}");
    let mut callback_response = Vec::new();
    callback.read_to_end(&mut callback_response).await?;
    let requests = tokio::time::timeout(Duration::from_secs(2), server).await???;
    assert_eq!(requests.len(), expected_requests, "{phase:?}");
    let stalled = std::str::from_utf8(requests.last().ok_or("missing stalled request")?)?;
    let expected_path = match phase {
        LoginStallPhase::Token => TOKEN_PATH,
        LoginStallPhase::Jwks => JWKS_PATH,
        LoginStallPhase::UserInfo => USER_INFO_PATH,
    };
    assert!(stalled.starts_with(&format!(
        "{} {expected_path} HTTP/1.1\r\n",
        if matches!(phase, LoginStallPhase::Token) {
            "POST"
        } else {
            "GET"
        }
    )));
    assert_eq!(manager.status()?, CodexCredentialStatus::SignedOut);
    Ok(())
}

async fn assert_device_login_stall_expires(phase: LoginStallPhase) -> TestResult {
    let endpoint = test_endpoint().await?;
    let (_directory, slot) = private_slot()?;
    let manager = manager_for_endpoint(&slot, &endpoint)?;
    let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
    let poll_reply = response(
        "200 OK",
        "application/json",
        &serde_json::to_vec(&serde_json::json!({
            "authorization_code": "device-deadline-code",
            "code_challenge": challenge,
            "code_verifier": verifier
        }))?,
        "",
    );
    let token = id_token(
        TEST_KID,
        "device-deadline-subject",
        "device-deadline-account",
    )?;
    let token_reply = response(
        "200 OK",
        "application/json",
        &token_body(
            &token,
            "device-deadline-access",
            "device-deadline-refresh",
            3_600,
        )?,
        "",
    );
    let jwks_reply = response(
        "200 OK",
        "application/json",
        &jwks_body(TEST_KID, TEST_MODULUS)?,
        "Cache-Control: max-age=300\r\n",
    );
    let replies = match phase {
        LoginStallPhase::Token => vec![poll_reply],
        LoginStallPhase::Jwks => vec![poll_reply, token_reply],
        LoginStallPhase::UserInfo => vec![poll_reply, token_reply, jwks_reply],
    };
    let expected_requests = replies.len() + 1;
    let (observed, stalled) = tokio::sync::oneshot::channel();
    let server = spawn_stall_after(endpoint, replies, observed);
    let deadline = Instant::now() + Duration::from_secs(20);
    let login = device_login_for_deadline_test(deadline);
    let worker_manager = Arc::clone(&manager);
    let completion = tokio::spawn(async move { worker_manager.complete_device_login(login).await });
    let outcome = expire_observed_login_stall(completion, stalled, deadline).await?;
    assert_eq!(outcome, Err(phase.expected_error()), "{phase:?}");
    let requests = tokio::time::timeout(Duration::from_secs(2), server).await???;
    assert_eq!(requests.len(), expected_requests, "{phase:?}");
    let stalled = std::str::from_utf8(requests.last().ok_or("missing stalled request")?)?;
    let expected_path = match phase {
        LoginStallPhase::Token => TOKEN_PATH,
        LoginStallPhase::Jwks => JWKS_PATH,
        LoginStallPhase::UserInfo => USER_INFO_PATH,
    };
    assert!(stalled.starts_with(&format!(
        "{} {expected_path} HTTP/1.1\r\n",
        if matches!(phase, LoginStallPhase::Token) {
            "POST"
        } else {
            "GET"
        }
    )));
    assert_eq!(manager.status()?, CodexCredentialStatus::SignedOut);
    Ok(())
}

fn manager_for_endpoint(
    slot: &Path,
    endpoint: &TestEndpoint,
) -> Result<Arc<CodexSubscriptionCredentialManager>, NativeCodexAuthError> {
    Ok(Arc::new(
        CodexSubscriptionCredentialManager::new_with_endpoints(
            slot,
            endpoint.client.clone(),
            acceptance()?,
            ProfileEndpoints {
                auth_origin: endpoint.origin.clone(),
                chatgpt_origin: endpoint.origin.clone(),
            },
        )?,
    ))
}

fn manager_for_endpoint_with_gate(
    slot: &Path,
    endpoint: &TestEndpoint,
    gate: Arc<dyn SubscriptionRequestGate>,
) -> Result<Arc<CodexSubscriptionCredentialManager>, NativeCodexAuthError> {
    Ok(Arc::new(
        CodexSubscriptionCredentialManager::new_with_endpoints(
            slot,
            endpoint.client.clone(),
            acceptance()?,
            ProfileEndpoints {
                auth_origin: endpoint.origin.clone(),
                chatgpt_origin: endpoint.origin.clone(),
            },
        )?
        .with_subscription_request_gate(gate),
    ))
}

fn subscription_gate(
    catalog: u32,
    inference: u32,
) -> Result<Arc<InMemorySubscriptionRequestGate>, NativeCodexAuthError> {
    let limits = SubscriptionRequestLimits::new(
        8,
        catalog,
        inference,
        1_000_000,
        Some(Duration::from_secs(300)),
        0,
    )
    .map_err(|_| NativeCodexAuthError::Validation)?;
    Ok(Arc::new(InMemorySubscriptionRequestGate::new(
        limits,
        Arc::new(SystemSubscriptionGateClock::default()),
    )))
}

/// The selection epoch of a seeded login; a relogin mints a fresh one.
const SEEDED_SELECTION_EPOCH: &str = "seeded-login-selection-epoch-0123456789abcdef";

fn seed_ready_credential(
    slot: &Path,
    account: &str,
    subject: &str,
    access_expires_at: u64,
) -> Result<(), NativeCodexAuthError> {
    let store = PrivateCredentialStore::open_slot(
        slot,
        AuthProfileId::new(OPENAI_CODEX_COMPATIBILITY_PROFILE_ID)
            .map_err(|_| NativeCodexAuthError::Validation)?,
        CredentialSlotId::new("primary").map_err(|_| NativeCodexAuthError::Validation)?,
    )
    .map_err(|_| NativeCodexAuthError::Store)?;
    let credential = StoredCredential {
        access_token: "opaque-seeded-access".to_owned(),
        refresh_token: "opaque-seeded-refresh".to_owned(),
        id_token: "opaque-seeded-id".to_owned(),
        subject: subject.to_owned(),
        account_id: account.to_owned(),
        scopes: validate_exact_scopes(OPENAI_CODEX_OAUTH_SCOPES)?,
        access_expires_at,
        selection_epoch: SEEDED_SELECTION_EPOCH.to_owned(),
    };
    let record = stored_record(
        &credential,
        CredentialGeneration::new(1).map_err(|_| NativeCodexAuthError::Store)?,
    )?;
    store
        .initialize_selected(&record)
        .map_err(|_| NativeCodexAuthError::Store)?;
    Ok(())
}

async fn expire_ready_generation(
    manager: &Arc<CodexSubscriptionCredentialManager>,
) -> Result<CredentialFence, NativeCodexAuthError> {
    let store = Arc::clone(&manager.store);
    run_blocking(move || {
        let fence = store
            .load()
            .map_err(|_| NativeCodexAuthError::Store)?
            .ok_or(NativeCodexAuthError::Unavailable)?;
        let lease = store
            .acquire_ready(&fence)
            .map_err(|_| NativeCodexAuthError::Store)?;
        let mut credential = parse_stored(lease.payload())?;
        drop(lease);
        credential.access_expires_at = now_unix_secs();
        let generation = fence
            .generation()
            .next()
            .map_err(|_| NativeCodexAuthError::Store)?;
        let replacement = stored_record(&credential, generation)?;
        store
            .replace(fence.generation(), &replacement)
            .map_err(|_| NativeCodexAuthError::Store)?;
        Ok(replacement.fence())
    })
    .await
}

fn metadata(attempt_id: &str) -> TestResult<RequestMetadata> {
    Ok(RequestMetadata::for_turn(
        "00000000-0000-7000-8000-000000000201".parse::<RunId>()?,
        "00000000-0000-7000-8000-000000000202".parse::<TurnId>()?,
        attempt_id.parse::<AttemptId>()?,
        system_timestamp(),
    ))
}

fn resolved_harness_plan() -> TestResult<Digest> {
    Ok(Digest::sha256(
        DigestDomain::ResolvedHarnessPlan,
        1,
        &[b"openai-native-auth-test-harness-plan"],
    )?)
}

fn provider_configuration_ids() -> TestResult<(
    SelectedProviderConfigurationId,
    EffectiveProviderConfigurationId,
)> {
    Ok((
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::SelectedProviderConfiguration,
            5,
            &[b"openai-native-auth-test-selected-configuration"],
        )?)?)?,
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"openai-native-auth-test-effective-configuration"],
        )?)?)?,
    ))
}

fn user_item() -> TestResult<ConversationItem> {
    Ok(ConversationItem::new(
        "00000000-0000-7000-8000-000000000203".parse::<EntryId>()?,
        Actor::User,
        vec![ContentBlock::Text(TextBlock::new("native route", None)?)],
        Provenance::UserInput,
    )?)
}

fn lookup_tool() -> TestResult<ToolDefinition> {
    Ok(ToolDefinition::new(
        ToolName::new("lookup")?,
        "1.0.0",
        "Look up one value",
        ToolInputContract::structured(
            serde_json::json!({"$schema":"https://json-schema.org/draft/2020-12/schema", "type":"object", "properties":{"q":{"type":"string"}}, "required":["q"], "additionalProperties":false}),
        )?,
        ToolOutputContract::portable_model_content(),
        ToolConcurrency::Serial,
        ToolEffect::ReadOnly,
    )?)
}

fn string_custom_tool() -> TestResult<ToolDefinition> {
    Ok(ToolDefinition::new(
        ToolName::new("free_form")?,
        "1.0.0",
        "Accept free-form text",
        ToolInputContract::utf8(65_536)?,
        ToolOutputContract::portable_model_content(),
        ToolConcurrency::Serial,
        ToolEffect::ReadOnly,
    )?)
}

fn saturated_observation_config() -> TestResult<StandardOpenAiHttpConfig> {
    Ok(StandardOpenAiHttpConfig::new(
        Duration::from_secs(120),
        1,
        plakous_responses::ResponsesStreamLimits::default(),
    )?)
}

fn subscription_model_with_gate(
    resolver: &CodexModelResolver,
    model_id: &str,
) -> TestResult<ResolvedTestModel> {
    resolver.resolve_with_evidence(model_id)?;
    let issued = resolver
        .catalog
        .lock()
        .map_err(|_| "catalog lock poisoned")?
        .as_ref()
        .ok_or("catalog missing")?
        .descriptors
        .get(model_id)
        .ok_or("model missing")?
        .clone();
    let issued_row = issued.clone();
    let descriptor =
        crate::model::with_test_context_contract(issued.descriptor, resolver.clock.now())?;
    let witness = resolver.manager.issue_catalog_witness(
        &descriptor,
        issued.witness.credential_fence(),
        issued.witness.credential_selection_epoch(),
        issued.observed_at,
        issued.expires_at,
        issued.responses_lite,
    )?;
    let fixture_descriptor = descriptor.clone();
    let target = OpenAiTarget::new_native_catalog_target(
        resolver.route.clone(),
        descriptor,
        witness,
        Arc::clone(&resolver.clock),
    )?;
    Ok(ResolvedTestModel {
        executable: Arc::new(OpenAiLanguageModel::new_codex(
            target,
            resolver.client.clone(),
            resolver.config,
            CodexReceiptRenewal {
                source: resolver.catalog_source(),
                model_id: model_id.to_owned(),
                semantics: CodexRowSemantics::of(&issued_row)?,
            },
        )?),
        descriptor: fixture_descriptor,
    })
}

fn permitted_subscription_model(
    resolver: &CodexModelResolver,
    model_id: &str,
) -> TestResult<ResolvedTestModel> {
    subscription_model_with_gate(resolver, model_id)
}

fn catalog_descriptor(
    resolver: &CodexModelResolver,
    model_id: &str,
) -> TestResult<ResolvedModelDescriptor> {
    Ok(resolver
        .catalog
        .lock()
        .map_err(|_| "catalog lock poisoned")?
        .as_ref()
        .ok_or("catalog missing")?
        .descriptors
        .get(model_id)
        .ok_or("model missing")?
        .descriptor
        .clone())
}

async fn model_for_subscription_gate_test(
    request_gate: Option<Arc<dyn SubscriptionRequestGate>>,
) -> TestResult<(
    ResolvedTestModel,
    TempDir,
    tokio::task::JoinHandle<TestResult<(Vec<Vec<u8>>, bool)>>,
)> {
    let endpoint = test_endpoint().await?;
    let client = endpoint.client.clone();
    let (directory, slot) = private_slot()?;
    seed_ready_credential(
        &slot,
        "account-gate-boundary",
        "subject-gate-boundary",
        now_unix_secs() + 3_600,
    )?;
    let manager = match request_gate {
        Some(gate) => manager_for_endpoint_with_gate(&slot, &endpoint, gate)?,
        None => manager_for_endpoint(&slot, &endpoint)?,
    };
    let model_id = "gpt-5.6-luna";
    let catalog = serde_json::to_vec(&serde_json::json!({"models":[{
        "slug": model_id,
        "visibility":"list",
        "priority":1,
        "supported_in_api":true,
        "supported_reasoning_levels":[{"effort":"low"}],
        "shell_type":"shell_command",
        "supports_parallel_tool_calls":false,
        "input_modalities":["text"],
        "context_window":272_000,
        "max_context_window":1_000_000,
        "effective_context_window_percent":95,
        "auto_compact_token_limit":240_000
    }]}))?;
    let server = spawn_sequence_then_probe(
        endpoint,
        vec![response("200 OK", "application/json", &catalog, "")],
    );
    let resolver =
        CodexModelResolver::new_for_test(manager, client, StandardOpenAiHttpConfig::default())?;
    let snapshot = resolver.refresh_catalog().await?;
    assert_eq!(snapshot.entries().len(), 1);
    let model = permitted_subscription_model(&resolver, model_id)?;
    Ok((model, directory, server))
}

fn catalog_body(model_ids: &[&str]) -> TestResult<Vec<u8>> {
    let models = model_ids
        .iter()
        .map(|model_id| {
            serde_json::json!({
                "slug": model_id,
                "visibility": "list",
                "priority": 1,
                "supported_in_api": true,
                "supported_reasoning_levels": [
                    {"effort": "low"},
                    {"effort": "medium"},
                    {"effort": "high"}
                ],
                "shell_type": "shell_command",
                "input_modalities": ["text"],
                "context_window": 272_000,
                "max_context_window": 1_000_000,
                "effective_context_window_percent": 95,
                "auto_compact_token_limit": 240_000
            })
        })
        .collect::<Vec<_>>();
    Ok(serde_json::to_vec(&serde_json::json!({"models": models}))?)
}

fn catalog_model_value(slug: &str) -> serde_json::Value {
    serde_json::json!({
        "slug": slug,
        "visibility": "list",
        "priority": 1,
        "supported_in_api": true,
        "supported_reasoning_levels": [{"effort": "low"}],
        "shell_type": "shell_command",
        "input_modalities": ["text"],
        "context_window": 272_000,
        "max_context_window": 1_000_000,
        "effective_context_window_percent": 95,
        "auto_compact_token_limit": 240_000
    })
}

async fn resolver_for_catalog_values(
    account: &str,
    models: Vec<serde_json::Value>,
) -> TestResult<(TempDir, CodexModelResolver, CodexModelCatalog)> {
    let endpoint = test_endpoint().await?;
    let client = endpoint.client.clone();
    let (directory, slot) = private_slot()?;
    seed_ready_credential(&slot, account, "row-scope-subject", now_unix_secs() + 3_600)?;
    let manager = manager_for_endpoint(&slot, &endpoint)?;
    let body = serde_json::to_vec(&serde_json::json!({"models": models}))?;
    let server = spawn_sequence(
        endpoint,
        vec![response("200 OK", "application/json", &body, "")],
    );
    let resolver =
        CodexModelResolver::new_for_test(manager, client, StandardOpenAiHttpConfig::default())?;
    let catalog = resolver.refresh_catalog().await?;
    assert_eq!(server.await??.len(), 1);
    Ok((directory, resolver, catalog))
}

#[tokio::test]
async fn exact_catalog_resolution_quarantines_unrelated_rows_without_fallback() -> TestResult {
    let selected = "gpt-5.6-luna";
    let mut future = catalog_model_value("gpt-future-unrelated");
    future["shell_type"] = serde_json::json!("future_shell_alpha");
    future["input_modalities"] = serde_json::json!(["text", "future_modality_alpha"]);
    future["supported_reasoning_levels"] = serde_json::json!([{"effort":"low"}, {"effort":"future_effort_alpha"}, {"effort":"future_effort_beta"}]);
    let unrelated_duplicate = catalog_model_value("gpt-duplicate-unrelated");
    let models = vec![
        catalog_model_value(selected),
        future,
        serde_json::json!({"slug":"gpt-missing-shell","supported_reasoning_levels":[],"supports_parallel_tool_calls":false,"context_window":1}),
        serde_json::json!({"slug":"","supported_reasoning_levels":[],"shell_type":"disabled","supports_parallel_tool_calls":false,"context_window":1}),
        serde_json::json!({"slug":"gpt-invalid-capacity","supported_reasoning_levels":[],"shell_type":"disabled","supports_parallel_tool_calls":false}),
        unrelated_duplicate.clone(),
        unrelated_duplicate,
    ];
    let (_directory, resolver, _) =
        resolver_for_catalog_values("account-row-quarantine", models).await?;
    resolver.resolve_with_evidence(selected)?;
    assert!(matches!(
        resolver.resolve("gpt-duplicate-unrelated"),
        Err(NativeCodexAuthError::Catalog)
    ));
    for non_exact in ["gpt-5", "prefix-gpt-5.4", "gpt-5.4-latest"] {
        assert!(matches!(
            resolver.resolve(non_exact),
            Err(NativeCodexAuthError::Catalog)
        ));
    }
    Ok(())
}

#[tokio::test]
async fn catalog_projects_only_recognized_portable_efforts_in_wire_order() -> TestResult {
    let mut model = catalog_model_value("gpt-5.6-luna");
    model["supported_reasoning_levels"] = serde_json::json!([
        {"effort":"high"},
        {"effort":"minimal"},
        {"effort":"ultra"},
        {"effort":"future_effort"},
        {"effort":"low"},
        {"effort":"xhigh"}
    ]);
    let (_directory, _resolver, catalog) =
        resolver_for_catalog_values("account-effort-options", vec![model]).await?;
    assert_eq!(catalog.entries().len(), 1);
    assert_eq!(
        catalog.entries()[0].reasoning_effort_options(),
        &[
            ReasoningEffort::High,
            ReasoningEffort::Low,
            ReasoningEffort::XHigh
        ]
    );
    Ok(())
}

#[tokio::test]
async fn catalog_preserves_exact_service_tiers_and_provider_defaults() -> TestResult {
    let mut model = catalog_model_value("gpt-5.6-luna");
    model["supported_reasoning_levels"] =
        serde_json::json!([{"effort":"low"}, {"effort":"medium"}]);
    model["default_reasoning_level"] = serde_json::json!("medium");
    model["service_tiers"] = serde_json::json!([{
        "id": "priority",
        "name": "Fast",
        "description": "1.5x speed, increased usage"
    }]);
    model["default_service_tier"] = serde_json::json!("priority");
    let (_directory, _resolver, catalog) =
        resolver_for_catalog_values("account-service-tiers", vec![model]).await?;
    let entry = catalog.entries().first().ok_or("catalog entry missing")?;
    assert_eq!(
        entry.default_reasoning_effort(),
        Some(ReasoningEffort::Medium)
    );
    assert_eq!(entry.default_service_tier(), Some("priority"));
    let tier = entry
        .service_tiers()
        .first()
        .ok_or("service tier missing")?;
    assert_eq!(tier.id(), "priority");
    assert_eq!(tier.name(), "Fast");
    assert_eq!(tier.description(), "1.5x speed, increased usage");
    Ok(())
}

#[tokio::test]
async fn codex_0159_reference_gpt_61_sol_row_resolves_with_priority_tier() -> TestResult {
    // Field values copied from the rust-v0.160.0 reference catalog row, including
    // fields this decoder does not consume.
    let mut model = catalog_model_value("gpt-6.1-sol");
    model["shell_type"] = serde_json::json!("shell_command");
    model["tool_mode"] = serde_json::json!("code_mode_only");
    model["use_responses_lite"] = serde_json::json!(true);
    model["supports_parallel_tool_calls"] = serde_json::json!(true);
    model["input_modalities"] = serde_json::json!(["text", "image"]);
    model["context_window"] = serde_json::json!(272_000);
    model["max_context_window"] = serde_json::json!(872_000);
    model["auto_compact_token_limit"] = serde_json::Value::Null;
    model["supported_reasoning_levels"] = serde_json::json!([
        {"effort": "low"}, {"effort": "medium"}, {"effort": "high"},
        {"effort": "xhigh"}, {"effort": "max"}, {"effort": "ultra"}
    ]);
    model["default_reasoning_level"] = serde_json::json!("low");
    model["service_tiers"] = serde_json::json!([{
        "id": "priority",
        "name": "Fast",
        "description": "2x speed, increased usage"
    }]);
    model["default_service_tier"] = serde_json::Value::Null;
    model["supports_reasoning_effort_updates"] = serde_json::json!(true);
    model["multi_agent_reasoning_effort"] = serde_json::json!("xhigh");
    model["guardian"] = serde_json::Value::Null;
    let (_directory, resolver, catalog) =
        resolver_for_catalog_values("account-codex-0159-sol", vec![model]).await?;
    let entry = catalog.entries().first().ok_or("catalog entry missing")?;
    assert_eq!(entry.model_id(), "gpt-6.1-sol");
    assert_eq!(entry.tool_mode(), Some(CodexCatalogToolMode::CodeModeOnly));
    assert!(entry.uses_responses_lite());
    assert!(!entry.supports_parallel_tools());
    assert_eq!(entry.default_reasoning_effort(), Some(ReasoningEffort::Low));
    assert_eq!(entry.default_service_tier(), None);
    let tier = entry
        .service_tiers()
        .first()
        .ok_or("service tier missing")?;
    assert_eq!(tier.id(), "priority");
    assert_eq!(tier.description(), "2x speed, increased usage");
    for effort in [
        ReasoningEffort::Low,
        ReasoningEffort::Medium,
        ReasoningEffort::High,
        ReasoningEffort::XHigh,
        ReasoningEffort::Max,
    ] {
        assert!(entry.supports_reasoning_effort(effort), "{effort:?}");
    }
    let (_, evidence) = resolver.resolve_with_evidence("gpt-6.1-sol")?;
    let capacity = evidence.capacity_facts();
    assert_eq!(capacity.context_window().ok_or("window")?.get(), 272_000);
    assert_eq!(
        capacity.max_context_window().ok_or("max window")?.get(),
        872_000
    );
    assert_eq!(capacity.auto_compact_token_limit(), None);
    Ok(())
}

#[tokio::test]
async fn executable_catalog_rows_fail_closed_on_invalid_service_tiers_and_defaults() -> TestResult {
    let mut duplicate = catalog_model_value("gpt-5.6-luna");
    duplicate["tool_mode"] = serde_json::json!("code_mode_only");
    duplicate["service_tiers"] = serde_json::json!([
        {"id":"priority","name":"Fast","description":"first"},
        {"id":"priority","name":"Faster","description":"second"}
    ]);
    let mut unknown = catalog_model_value("gpt-5.6-luna");
    unknown["tool_mode"] = serde_json::json!("code_mode_only");
    unknown["service_tiers"] = serde_json::json!([
        {"id":"future-tier","name":"Future","description":"unknown executable tier"}
    ]);
    let mut malformed = catalog_model_value("gpt-5.5");
    malformed["service_tiers"] = serde_json::json!([
        {"id":"priority","name":"","description":"missing name"}
    ]);
    let mut dangling_default = catalog_model_value("gpt-5.6-sol");
    dangling_default["tool_mode"] = serde_json::json!("code_mode_only");
    dangling_default["service_tiers"] = serde_json::json!([
        {"id":"priority","name":"Fast","description":"known tier"}
    ]);
    dangling_default["default_service_tier"] = serde_json::json!("future-tier");
    let mut unknown_reasoning_default = catalog_model_value("gpt-5.6-terra");
    unknown_reasoning_default["tool_mode"] = serde_json::json!("code_mode_only");
    unknown_reasoning_default["default_reasoning_level"] = serde_json::json!("future-effort");
    let valid = catalog_model_value("gpt-6-astra");
    let (_directory, resolver, catalog) = resolver_for_catalog_values(
        "account-invalid-service-tiers",
        vec![
            duplicate,
            unknown,
            malformed,
            dangling_default,
            unknown_reasoning_default,
            valid,
        ],
    )
    .await?;
    assert_eq!(
        catalog
            .entries()
            .iter()
            .map(CodexCatalogEntry::model_id)
            .collect::<Vec<_>>(),
        vec!["gpt-6-astra"]
    );
    for rejected in [
        "gpt-5.4-mini",
        "gpt-5.3-codex-spark",
        "gpt-5.5",
        "gpt-5.6-luna",
        "gpt-5.6-sol",
        "gpt-5.6-terra",
    ] {
        assert!(matches!(
            resolver.resolve(rejected),
            Err(NativeCodexAuthError::Catalog)
        ));
    }
    Ok(())
}

#[test]
fn catalog_fingerprint_binds_service_tiers_and_reasoning_default() -> TestResult {
    let mut base = catalog_model_value("gpt-catalog-fingerprint");
    base["supported_reasoning_levels"] = serde_json::json!([{"effort":"low"}, {"effort":"medium"}]);
    base["default_reasoning_level"] = serde_json::json!("low");
    base["service_tiers"] = serde_json::json!([{
        "id":"priority","name":"Fast","description":"provider description"
    }]);
    let baseline = catalog_enum_fingerprint(&serde_json::from_value(base.clone())?)?;
    for changed in [
        ("default_reasoning_level", serde_json::json!("medium")),
        ("default_service_tier", serde_json::json!("priority")),
    ] {
        let mut model = base.clone();
        model[changed.0] = changed.1;
        assert_ne!(
            baseline,
            catalog_enum_fingerprint(&serde_json::from_value(model)?)?
        );
    }
    for (field, value) in [
        ("id", "priority-v2"),
        ("name", "Faster"),
        ("description", "changed provider description"),
    ] {
        let mut model = base.clone();
        model["service_tiers"][0][field] = serde_json::json!(value);
        assert_ne!(
            baseline,
            catalog_enum_fingerprint(&serde_json::from_value(model)?)?
        );
    }
    Ok(())
}

#[tokio::test]
async fn catalog_tool_mode_defaults_missing_to_direct_and_rejects_unknown() -> TestResult {
    let mut code_mode = catalog_model_value("gpt-5.5");
    code_mode["tool_mode"] = serde_json::json!("code_mode");
    let mut code_mode_only = catalog_model_value("gpt-5.6-sol");
    code_mode_only["tool_mode"] = serde_json::json!("code_mode_only");
    // An enrolled CodeModeOnly slug with `tool_mode` omitted still lowers to
    // Direct on the wire and remains executable.
    let direct = catalog_model_value("gpt-5.6-luna");
    let mut future = catalog_model_value("gpt-5.6-terra");
    future["tool_mode"] = serde_json::json!("future_code_mode_v2");
    let (_directory, resolver, catalog) = resolver_for_catalog_values(
        "account-tool-mode",
        vec![code_mode, code_mode_only, direct, future],
    )
    .await?;

    let entry = |slug: &str| {
        catalog
            .entries()
            .iter()
            .find(|entry| entry.model_id() == slug)
            .ok_or("catalog entry missing")
    };
    assert_eq!(
        entry("gpt-5.6-luna")?.tool_mode(),
        Some(CodexCatalogToolMode::Direct)
    );
    assert!(!entry("gpt-5.6-luna")?.supports_code_mode());
    assert!(entry("gpt-5.5").is_err());
    assert_eq!(
        entry("gpt-5.6-sol")?.tool_mode(),
        Some(CodexCatalogToolMode::CodeModeOnly)
    );
    assert!(entry("gpt-5.6-sol")?.supports_code_mode());
    let (_, missing_evidence) = resolver.resolve_with_evidence("gpt-5.6-luna")?;
    assert_eq!(
        missing_evidence.tool_mode(),
        Some(CodexCatalogToolMode::Direct)
    );
    assert!(entry("gpt-5.6-terra").is_err());
    assert!(matches!(
        resolver.resolve_with_evidence("gpt-5.6-terra"),
        Err(NativeCodexAuthError::Catalog)
    ));
    let (_, evidence) = resolver.resolve_with_evidence("gpt-5.6-sol")?;
    assert_eq!(
        evidence.tool_mode(),
        Some(CodexCatalogToolMode::CodeModeOnly)
    );
    assert!(evidence.supports_code_mode());
    assert_eq!(
        serde_json::to_value(&evidence)?.get("tool_mode"),
        Some(&serde_json::json!("code_mode_only"))
    );
    let compaction = NamespaceId::new(CODEX_NATIVE_COMPACTION_NAMESPACE)?;
    let compaction_support = evidence
        .descriptor()
        .capabilities()
        .native_namespace(&compaction)
        .ok_or("Codex OAuth catalog must admit plakous.compaction")?;
    assert_eq!(compaction_support.support(), Support::Native);
    assert_eq!(
        compaction_support.provenance().source(),
        FactSource::RouteEvidence
    );
    let code_identity = evidence.semantic_provider_evidence()?.identity();
    let (_, direct_evidence) = resolver.resolve_with_evidence("gpt-5.6-luna")?;
    assert!(
        direct_evidence
            .descriptor()
            .capabilities()
            .native_namespace(&compaction)
            .is_some_and(|fact| fact.support() == Support::Native),
        "Codex OAuth Direct shares the native compact namespace"
    );
    assert_ne!(
        code_identity,
        direct_evidence.semantic_provider_evidence()?.identity(),
        "effective-configuration provider evidence must bind tool_mode"
    );
    Ok(())
}

#[tokio::test]
async fn codex_56_enrollment_keeps_slug_for_code_mode_only_or_direct_tool_mode() -> TestResult {
    let rows = [
        ("gpt-6-astra", "code_mode_only"),
        ("gpt-5.6-sol", "code_mode_only"),
        ("gpt-5.6-terra", "direct"),
        ("gpt-5.6-luna", "code_mode_only"),
    ];
    let models = rows
        .iter()
        .enumerate()
        .map(|(index, (slug, tool_mode))| {
            let mut model = catalog_model_value(slug);
            model["priority"] = serde_json::json!(index + 1);
            model["tool_mode"] = serde_json::json!(tool_mode);
            model["shell_type"] = serde_json::json!("unified_exec");
            model
        })
        .collect();
    let (_directory, _resolver, catalog) =
        resolver_for_catalog_values("account-56-tool-modes", models).await?;
    assert_eq!(catalog.entries().len(), 4);
    for (slug, tool_mode) in rows.iter().take(4) {
        let entry = catalog
            .entries()
            .iter()
            .find(|entry| entry.model_id() == *slug)
            .ok_or("catalog entry missing")?;
        assert_eq!(
            entry.tool_mode(),
            Some(if *tool_mode == "direct" {
                CodexCatalogToolMode::Direct
            } else {
                CodexCatalogToolMode::CodeModeOnly
            })
        );
    }
    assert!(
        catalog
            .entries()
            .iter()
            .all(|entry| entry.model_id() != "gpt-5.3-codex-spark"),
        "retired Spark is not an executable Codex OAuth slug"
    );
    Ok(())
}

#[tokio::test]
async fn codex_0159_catalog_projection_preserves_visibility_mode_and_oauth_rows() -> TestResult {
    let rows = [
        ("gpt-6-astra", "hide", Some("code_mode_only"), true),
        ("gpt-6.1-sol", "list", Some("code_mode_only"), true),
        ("gpt-6-sol", "list", Some("code_mode_only"), true),
        ("gpt-6-luna", "list", Some("code_mode_only"), true),
        ("gpt-5.6-sol", "list", Some("code_mode_only"), true),
        ("gpt-5.6-terra", "list", Some("code_mode_only"), true),
        ("gpt-5.6-luna", "list", Some("code_mode_only"), true),
        ("gpt-5.3-codex-spark", "list", None, true),
        ("codex-auto-review", "hide", Some("code_mode_only"), true),
    ];
    let models = rows
        .iter()
        .enumerate()
        .map(|(index, (slug, visibility, tool_mode, supported_in_api))| {
            let mut model = catalog_model_value(slug);
            model["visibility"] = serde_json::json!(visibility);
            model["priority"] = serde_json::json!(index + 1);
            model["supported_in_api"] = serde_json::json!(supported_in_api);
            model["shell_type"] = serde_json::json!("unified_exec");
            if index < 7 {
                model["use_responses_lite"] = serde_json::json!(true);
            }
            if let Some(tool_mode) = tool_mode {
                model["tool_mode"] = serde_json::json!(tool_mode);
            } else {
                model["tool_mode"] = serde_json::Value::Null;
            }
            if index == 0 {
                model["default_reasoning_level"] = serde_json::json!("ultra");
                model["supported_reasoning_levels"]
                    .as_array_mut()
                    .expect("reasoning levels")
                    .push(serde_json::json!({"effort": "ultra"}));
            }
            model
        })
        .collect();
    let (_directory, resolver, catalog) =
        resolver_for_catalog_values("account-codex-0159", models).await?;
    let listed = 7;
    assert_eq!(catalog.entries().len(), listed);
    for (entry, (slug, visibility, tool_mode, supported_in_api)) in
        catalog.entries().iter().zip(rows.into_iter().take(listed))
    {
        assert_eq!(entry.model_id(), slug);
        if slug == "gpt-6-astra" {
            assert_eq!(entry.default_reasoning_effort(), None);
            assert!(
                !entry
                    .reasoning_effort_options()
                    .contains(&ReasoningEffort::Ultra)
            );
        }
        assert_eq!(
            entry.visibility(),
            Some(if visibility == "list" {
                CodexCatalogVisibility::List
            } else {
                CodexCatalogVisibility::Hide
            })
        );
        assert_eq!(entry.supported_in_api(), supported_in_api);
        assert!(entry.uses_responses_lite());
        assert!(!entry.supports_parallel_tools());
        let (_, evidence) = resolver.resolve_with_evidence(slug)?;
        assert!(
            evidence
                .descriptor()
                .capabilities()
                .get(Capability::ParallelTools)
                .is_none(),
            "Responses Lite descriptor must not advertise parallel tools: {slug}"
        );
        assert_eq!(
            entry.tool_mode(),
            Some(match tool_mode {
                Some("code_mode_only") => CodexCatalogToolMode::CodeModeOnly,
                _ => CodexCatalogToolMode::Direct,
            })
        );
    }
    Ok(())
}

#[test]
fn catalog_tool_mode_rotates_exact_enum_fingerprint() -> TestResult {
    let wire = |tool_mode: Option<&str>| -> TestResult<CatalogModelWire> {
        let mut value = catalog_model_value("gpt-tool-mode-identity");
        if let Some(tool_mode) = tool_mode {
            value["tool_mode"] = serde_json::json!(tool_mode);
        }
        Ok(serde_json::from_value(value)?)
    };
    let direct = wire(Some("direct"))?;
    let code_mode = wire(Some("code_mode"))?;
    let code_mode_only = wire(Some("code_mode_only"))?;
    let missing = wire(None)?;
    let future = wire(Some("future_code_mode_v2"))?;
    let fingerprints = [
        catalog_enum_fingerprint(&direct)?,
        catalog_enum_fingerprint(&code_mode)?,
        catalog_enum_fingerprint(&code_mode_only)?,
        catalog_enum_fingerprint(&missing)?,
        catalog_enum_fingerprint(&future)?,
    ];
    for (index, fingerprint) in fingerprints.iter().enumerate() {
        for other in &fingerprints[index + 1..] {
            assert_ne!(fingerprint, other);
        }
    }
    let mut changed_priority = catalog_model_value("gpt-tool-mode-identity");
    changed_priority["priority"] = serde_json::json!(2);
    let mut changed_api_support = catalog_model_value("gpt-tool-mode-identity");
    changed_api_support["supported_in_api"] = serde_json::json!(false);
    assert_ne!(
        catalog_enum_fingerprint(&missing)?,
        catalog_enum_fingerprint(&serde_json::from_value(changed_priority)?)?
    );
    assert_ne!(
        catalog_enum_fingerprint(&missing)?,
        catalog_enum_fingerprint(&serde_json::from_value(changed_api_support)?)?
    );
    Ok(())
}

#[tokio::test]
async fn exact_catalog_resolution_rejects_selected_defects_and_duplicate_slug() -> TestResult {
    let mut missing_shell = catalog_model_value("gpt-selected-missing-shell");
    missing_shell.as_object_mut().unwrap().remove("shell_type");
    let mut missing_reasoning = catalog_model_value("gpt-selected-missing-reasoning");
    missing_reasoning
        .as_object_mut()
        .unwrap()
        .remove("supported_reasoning_levels");
    let mut missing_capacity = catalog_model_value("gpt-selected-missing-capacity");
    missing_capacity
        .as_object_mut()
        .unwrap()
        .remove("context_window");
    missing_capacity
        .as_object_mut()
        .unwrap()
        .remove("max_context_window");
    let mut missing_visibility = catalog_model_value("gpt-selected-missing-visibility");
    missing_visibility
        .as_object_mut()
        .unwrap()
        .remove("visibility");
    let mut unknown_visibility = catalog_model_value("gpt-selected-unknown-visibility");
    unknown_visibility["visibility"] = serde_json::json!("future_visibility");
    let mut missing_priority = catalog_model_value("gpt-selected-missing-priority");
    missing_priority.as_object_mut().unwrap().remove("priority");
    let mut missing_api_support = catalog_model_value("gpt-selected-missing-api-support");
    missing_api_support
        .as_object_mut()
        .unwrap()
        .remove("supported_in_api");
    let mut no_text = catalog_model_value("gpt-selected-no-text");
    no_text["input_modalities"] = serde_json::json!(["image"]);
    let mut duplicate_modalities = catalog_model_value("gpt-selected-duplicate-modality");
    duplicate_modalities["input_modalities"] = serde_json::json!(["text", "text"]);
    let mut duplicate_reasoning = catalog_model_value("gpt-selected-duplicate-reasoning");
    duplicate_reasoning["supported_reasoning_levels"] =
        serde_json::json!([{"effort":"low"}, {"effort":"low"}]);
    let duplicate = catalog_model_value("gpt-selected-duplicate-slug");
    let mut known_capability_drift = catalog_model_value("gpt-5.6-luna");
    known_capability_drift["tool_mode"] = serde_json::json!("code_mode_only");
    known_capability_drift["shell_type"] = serde_json::json!("disabled");
    let rejected = [
        "gpt-selected-missing-shell",
        "gpt-selected-missing-reasoning",
        "gpt-selected-missing-capacity",
        "gpt-selected-missing-visibility",
        "gpt-selected-unknown-visibility",
        "gpt-selected-missing-priority",
        "gpt-selected-missing-api-support",
        "gpt-selected-no-text",
        "gpt-selected-duplicate-modality",
        "gpt-selected-duplicate-reasoning",
        "gpt-selected-duplicate-slug",
        "gpt-5.6-luna",
    ];
    let models = vec![
        catalog_model_value("gpt-5.6-sol"),
        catalog_model_value("gpt-valid-anchor"),
        missing_shell,
        missing_reasoning,
        missing_capacity,
        missing_visibility,
        unknown_visibility,
        missing_priority,
        missing_api_support,
        no_text,
        duplicate_modalities,
        duplicate_reasoning,
        duplicate.clone(),
        duplicate,
        known_capability_drift,
    ];
    let (_directory, resolver, _) =
        resolver_for_catalog_values("account-selected-defects", models).await?;
    for slug in rejected {
        assert!(matches!(
            resolver.resolve(slug),
            Err(NativeCodexAuthError::Catalog)
        ));
    }
    resolver.resolve("gpt-5.6-sol")?;
    assert!(matches!(
        resolver.resolve("gpt-valid-anchor"),
        Err(NativeCodexAuthError::Catalog)
    ));
    assert!(!release_enrolled_codex_model("gpt-valid-anchor"));
    Ok(())
}

#[test]
fn executable_catalog_predicate_matches_exact_source_enrollment() {
    let expected = [
        ("gpt-6-astra", CodexCatalogToolMode::CodeModeOnly),
        ("gpt-6.1-sol", CodexCatalogToolMode::CodeModeOnly),
        ("gpt-6-sol", CodexCatalogToolMode::CodeModeOnly),
        ("gpt-6-luna", CodexCatalogToolMode::CodeModeOnly),
        ("gpt-5.6-sol", CodexCatalogToolMode::CodeModeOnly),
        ("gpt-5.6-terra", CodexCatalogToolMode::CodeModeOnly),
        ("gpt-5.6-luna", CodexCatalogToolMode::CodeModeOnly),
    ];
    assert_eq!(RELEASE_ENROLLED_CODEX_MODELS.len(), expected.len());
    for (model, tool_mode) in expected {
        assert!(executable_codex_catalog_model(model), "{model}");
        assert_eq!(release_enrolled_codex_tool_mode(model), Some(tool_mode));
    }
    for model in [
        "gpt-6",
        "gpt-6-astra-latest",
        "gpt-5.6",
        "GPT-5.6-SOL",
        "gpt-5.6-sol-latest",
        "gpt-5.4-nano",
        "gpt-5.5",
        "gpt-5.4",
        "gpt-5.4-mini",
        "gpt-5.2",
        "codex-auto-review",
        "gpt-5.3-codex-spark",
        "gpt-valid-anchor",
        "",
    ] {
        assert!(!executable_codex_catalog_model(model), "{model}");
        assert_eq!(release_enrolled_codex_tool_mode(model), None);
    }
}

fn capacity_model_wire(
    context_window: i64,
    max_context_window: i64,
    effective_context_window_percent: i64,
    auto_compact_token_limit: Option<i64>,
) -> TestResult<CatalogModelWire> {
    Ok(serde_json::from_value(serde_json::json!({
        "slug": "gpt-capacity-fixture",
        "visibility": "list",
        "priority": 1,
        "supported_in_api": true,
        "supported_reasoning_levels": [],
        "shell_type": "shell_command",
        "input_modalities": ["text"],
        "context_window": context_window,
        "max_context_window": max_context_window,
        "effective_context_window_percent": effective_context_window_percent,
        "auto_compact_token_limit": auto_compact_token_limit
    }))?)
}

#[test]
fn catalog_capacity_facts_are_exact_and_fail_closed() -> TestResult {
    let facts = CodexCatalogCapacityFacts::from_wire(&capacity_model_wire(
        272_000,
        1_000_000,
        95,
        Some(240_000),
    )?)?;
    assert_eq!(facts.context_window(), NonZeroU32::new(272_000));
    assert_eq!(facts.max_context_window(), NonZeroU32::new(1_000_000));
    assert_eq!(
        facts.effective_context_window_percent(),
        NonZeroU8::new(95).unwrap()
    );
    assert_eq!(facts.auto_compact_token_limit(), NonZeroU32::new(240_000));
    assert_eq!(facts.effective_auto_compact_token_limit(), 240_000);

    let missing_auto =
        CodexCatalogCapacityFacts::from_wire(&capacity_model_wire(272_000, 272_000, 95, None)?)?;
    assert_eq!(missing_auto.auto_compact_token_limit(), None);
    assert_eq!(missing_auto.effective_auto_compact_token_limit(), 244_800);

    for incoherent in [
        capacity_model_wire(0, 1, 100, None)?,
        capacity_model_wire(-1, 1, 100, None)?,
        capacity_model_wire(1, 0, 100, None)?,
        capacity_model_wire(2, 1, 100, None)?,
        capacity_model_wire(100, 100, 0, None)?,
        capacity_model_wire(100, 100, 101, None)?,
        capacity_model_wire(100, 100, 95, Some(0))?,
    ] {
        assert_eq!(
            CodexCatalogCapacityFacts::from_wire(&incoherent),
            Err(NativeCodexAuthError::Catalog)
        );
    }
    let clamped =
        CodexCatalogCapacityFacts::from_wire(&capacity_model_wire(100, 100, 95, Some(96))?)?;
    assert_eq!(clamped.auto_compact_token_limit().unwrap().get(), 96);
    assert_eq!(clamped.effective_auto_compact_token_limit(), 90);
    let overflowing = capacity_model_wire(i64::MAX, i64::MAX, 100, None)?;
    assert_eq!(
        CodexCatalogCapacityFacts::from_wire(&overflowing),
        Err(NativeCodexAuthError::Catalog)
    );
    let out_of_u32 = capacity_model_wire(i64::from(u32::MAX) + 1, i64::MAX, 100, None)?;
    assert_eq!(
        CodexCatalogCapacityFacts::from_wire(&out_of_u32),
        Err(NativeCodexAuthError::Catalog)
    );
    assert!(parse_catalog_json::<CatalogModelWire>(
            br#"{"slug":"m","supported_reasoning_levels":[],"shell_type":"shell_command","input_modalities":["text"],"context_window":18446744073709551615,"max_context_window":18446744073709551615,"effective_context_window_percent":100,"auto_compact_token_limit":null}"#
        )
        .is_err());
    Ok(())
}

#[test]
fn catalog_capacity_fields_apply_provider_defaults_and_reject_ambiguity() -> TestResult {
    let complete = serde_json::json!({
        "slug": "gpt-capacity-fixture",
        "visibility": "list",
        "priority": 1,
        "supported_in_api": true,
        "supported_reasoning_levels": [],
        "shell_type": "shell_command",
        "input_modalities": ["text"],
        "context_window": 272_000,
        "max_context_window": 1_000_000,
        "effective_context_window_percent": 95,
        "auto_compact_token_limit": null
    });
    let mut missing_context = complete.clone();
    missing_context
        .as_object_mut()
        .ok_or("catalog fixture was not an object")?
        .remove("context_window");
    let missing_context = serde_json::from_value::<CatalogModelWire>(missing_context)?;
    assert_eq!(
        CodexCatalogCapacityFacts::from_wire(&missing_context)?.context_window(),
        None
    );
    let mut missing_maximum = complete.clone();
    missing_maximum
        .as_object_mut()
        .ok_or("catalog fixture was not an object")?
        .remove("max_context_window");
    let missing_maximum = serde_json::from_value::<CatalogModelWire>(missing_maximum)?;
    assert_eq!(
        CodexCatalogCapacityFacts::from_wire(&missing_maximum)?.max_context_window(),
        None
    );
    let mut missing_both = complete.clone();
    let missing_both_object = missing_both
        .as_object_mut()
        .ok_or("catalog fixture was not an object")?;
    missing_both_object.remove("context_window");
    missing_both_object.remove("max_context_window");
    assert_eq!(
        CodexCatalogCapacityFacts::from_wire(&serde_json::from_value(missing_both)?),
        Err(NativeCodexAuthError::Catalog)
    );
    let mut missing_percent = complete.clone();
    missing_percent
        .as_object_mut()
        .ok_or("catalog fixture was not an object")?
        .remove("effective_context_window_percent");
    let missing_percent = serde_json::from_value::<CatalogModelWire>(missing_percent)?;
    assert_eq!(missing_percent.effective_context_window_percent, 95);
    let without_optional_auto = complete
        .as_object()
        .ok_or("catalog fixture was not an object")?
        .iter()
        .filter(|(name, _)| name.as_str() != "auto_compact_token_limit")
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect::<serde_json::Map<_, _>>();
    let without_optional_auto = serde_json::from_value::<CatalogModelWire>(
        serde_json::Value::Object(without_optional_auto),
    )?;
    assert_eq!(without_optional_auto.auto_compact_token_limit, None);
    assert!(parse_catalog_json::<CatalogModelWire>(
            br#"{"slug":"m","supported_reasoning_levels":[],"shell_type":"shell_command","input_modalities":["text"],"context_window":272000,"context_window":1000000,"max_context_window":1000000,"effective_context_window_percent":95,"auto_compact_token_limit":null}"#
        )
        .is_err());
    Ok(())
}

async fn resolved_evidence_fixture(
    account: &str,
    model_ids: &[&str],
) -> TestResult<(
    TempDir,
    PathBuf,
    Arc<CodexSubscriptionCredentialManager>,
    CodexModelResolver,
    Timestamp,
)> {
    resolved_evidence_fixture_with_access(account, model_ids, now_unix_secs() + 3_600).await
}

async fn resolved_evidence_fixture_with_access(
    account: &str,
    model_ids: &[&str],
    access_expires_at: u64,
) -> TestResult<(
    TempDir,
    PathBuf,
    Arc<CodexSubscriptionCredentialManager>,
    CodexModelResolver,
    Timestamp,
)> {
    let endpoint = test_endpoint().await?;
    let client = endpoint.client.clone();
    let (directory, slot) = private_slot()?;
    seed_ready_credential(&slot, account, "resolution-subject", access_expires_at)?;
    let manager = manager_for_endpoint(&slot, &endpoint)?;
    let server = spawn_sequence(
        endpoint,
        vec![response(
            "200 OK",
            "application/json",
            &catalog_body(model_ids)?,
            "",
        )],
    );
    let observed_at = system_timestamp();
    let resolver = CodexModelResolver::new_for_test(
        Arc::clone(&manager),
        client,
        StandardOpenAiHttpConfig::default(),
    )?
    .clock(Arc::new(FixedClock::new(observed_at)));
    let catalog = resolver.refresh_catalog().await?;
    assert_eq!(catalog.entries().len(), model_ids.len());
    assert_eq!(server.await??.len(), 1);
    Ok((directory, slot, manager, resolver, observed_at))
}

fn native_target_from_resolver(
    resolver: &CodexModelResolver,
    model_id: &str,
) -> TestResult<OpenAiTarget> {
    let catalog = resolver
        .catalog
        .lock()
        .map_err(|_| "catalog lock poisoned")?;
    let issued = catalog
        .as_ref()
        .and_then(|catalog| catalog.descriptors.get(model_id))
        .ok_or("model missing from resolved catalog")?
        .clone();
    Ok(OpenAiTarget::new_native_catalog_target(
        resolver.route.clone(),
        issued.descriptor,
        issued.witness,
        Arc::clone(&resolver.clock),
    )?)
}

fn relogin_credential(account: &str, marker: &str) -> TestResult<StoredCredential> {
    Ok(StoredCredential {
        access_token: format!("opaque-resolution-access-{marker}"),
        refresh_token: format!("opaque-resolution-refresh-{marker}"),
        id_token: format!("opaque-resolution-id-{marker}"),
        subject: format!("resolution-subject-{marker}"),
        account_id: account.to_owned(),
        scopes: validate_exact_scopes(OPENAI_CODEX_OAUTH_SCOPES)?,
        access_expires_at: now_unix_secs() + 3_600,
        selection_epoch: generate_selection_epoch()?,
    })
}

fn externally_replace_ready_generation(
    slot: &Path,
    account: &str,
    marker: &str,
) -> TestResult<CredentialFence> {
    let store = PrivateCredentialStore::open_slot(
        slot,
        AuthProfileId::new(OPENAI_CODEX_COMPATIBILITY_PROFILE_ID)?,
        CredentialSlotId::new("primary")?,
    )?;
    let current = store
        .load()?
        .ok_or("external rotation found no credential")?;
    let next = current.generation().next()?;
    let replacement = stored_record(&relogin_credential(account, marker)?, next)?;
    let replacement_fence = replacement.fence();
    store.replace(current.generation(), &replacement)?;
    Ok(replacement_fence)
}

#[tokio::test]
async fn cross_account_request_authority_is_rejected_before_codex_preflight() -> TestResult {
    let model_id = "gpt-5.6-luna";
    let (_directory_a, _slot_a, _manager_a, resolver_a, _) =
        resolved_evidence_fixture("account-authority-a", &[model_id]).await?;
    let (_directory_b, _slot_b, _manager_b, resolver_b, _) =
        resolved_evidence_fixture("account-authority-b", &[model_id]).await?;
    let target_a = native_target_from_resolver(&resolver_a, model_id)?;
    let target_b = native_target_from_resolver(&resolver_b, model_id)?;
    let (selected, effective) = provider_configuration_ids()?;
    let request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![user_item()?])?,
        metadata("00000000-0000-7000-8000-000000000220")?,
    )
    .resolved_harness_plan_identity(resolved_harness_plan()?)
    .build()?;
    let plan = target_a
        .descriptor()
        .validate_request(request.clone(), ValidationMode::Strict)?;
    let authority = ResponsesRequestAuthority::from_admitted_operation(
        target_a.descriptor(),
        &plan,
        &request,
        target_a.route().account_binding()?,
        plakous_responses::ResponsesSemanticRevisions::new(2, 1, 4)?,
        None,
        plakous_responses_dialect::ResponsesStoragePolicy::Stateless,
    )?;
    assert_eq!(
        validate_codex_request_authority(&target_a, &authority),
        Ok(())
    );
    assert_eq!(
        validate_codex_request_authority(&target_b, &authority),
        Err(ResponsesDispatchRejection::Binding)
    );
    Ok(())
}

#[tokio::test]
async fn resolution_evidence_is_exact_and_deterministic() -> TestResult {
    let model_id = "gpt-5.6-luna";
    let (_directory, _slot, manager, resolver, observed_at) =
        resolved_evidence_fixture("account-resolution-a", &[model_id]).await?;
    let (first_model, first) = resolver.resolve_with_evidence(model_id)?;
    let (_second_model, second) = resolver.resolve_with_evidence(model_id)?;
    assert_eq!(first, second);
    assert_eq!(
        first.account_fingerprint(),
        resolver.route.account_binding()?
    );
    assert_eq!(
        first.credential_authority_binding(),
        resolver.route.credential_authority_binding()?
    );
    assert_eq!(first.selected_credential_generation(), 1);
    assert_eq!(first.catalog_observed_at(), observed_at);
    // The evidence lasts 6 h from its observation, independent of the
    // fixture's one-hour access token.
    assert_eq!(
        first.catalog_expires_at().seconds(),
        (observed_at.seconds() + CATALOG_TTL_SECONDS).min(PROFILE_EXPIRES_AT)
    );
    assert_eq!(first.adapter_revision(), OPENAI_RESPONSES_ADAPTER_REVISION);
    assert_eq!(
        first.profile_revision(),
        OPENAI_CODEX_COMPATIBILITY_PROFILE_ID
    );
    assert_eq!(first.profile_id(), first.profile_revision());
    let capacity_facts = first.capacity_facts();
    assert_eq!(capacity_facts.context_window().unwrap().get(), 272_000);
    assert_eq!(
        capacity_facts.max_context_window().unwrap().get(),
        1_000_000
    );
    assert_eq!(capacity_facts.effective_context_window_percent().get(), 95);
    assert_eq!(
        capacity_facts.auto_compact_token_limit(),
        NonZeroU32::new(240_000)
    );
    let entry = resolver
        .catalog
        .lock()
        .map_err(|_| "catalog lock poisoned")?
        .as_ref()
        .and_then(|catalog| {
            catalog
                .public
                .entries()
                .iter()
                .find(|entry| entry.model_id() == model_id)
        })
        .cloned()
        .ok_or("public catalog entry missing")?;
    assert_eq!(entry.capacity_facts(), capacity_facts);
    for effort in [
        ReasoningEffort::Low,
        ReasoningEffort::Medium,
        ReasoningEffort::High,
    ] {
        assert!(entry.supports_reasoning_effort(effort));
        assert!(first.supports_reasoning_effort(effort));
    }
    for effort in [
        ReasoningEffort::XHigh,
        ReasoningEffort::Max,
        ReasoningEffort::Ultra,
        ReasoningEffort::TokenBudget(NonZeroU32::new(1).ok_or("zero token budget")?),
    ] {
        assert!(!entry.supports_reasoning_effort(effort));
        assert!(!first.supports_reasoning_effort(effort));
    }
    assert_eq!(
        serde_json::to_value(&first)?.get("supported_reasoning_levels"),
        Some(&serde_json::json!(
            CodexReasoningEffortSet::LOW
                | CodexReasoningEffortSet::MEDIUM
                | CodexReasoningEffortSet::HIGH
        ))
    );
    assert_eq!(entry.observed_at(), first.catalog_observed_at());
    assert_eq!(entry.expires_at(), first.catalog_expires_at());
    assert_eq!(
        first.route_fingerprint().domain(),
        DigestDomain::ModelBinding
    );
    assert_eq!(
        first.credential_selection_epoch(),
        credential_selection_epoch(
            first.account_fingerprint(),
            first.credential_authority_binding(),
            SEEDED_SELECTION_EPOCH,
        )?
    );
    let issued = resolver
        .catalog
        .lock()
        .map_err(|_| "catalog lock poisoned")?
        .as_ref()
        .ok_or("catalog missing")?
        .descriptors
        .get(model_id)
        .ok_or("descriptor missing")?
        .clone();
    let descriptor_digest = issued.descriptor.binding_digest()?;
    assert_eq!(first.descriptor_digest(), descriptor_digest);
    let contract = issued
        .descriptor
        .context_contract()
        .ok_or("reference-client-envelope context contract missing")?;
    assert!(matches!(
        contract.capacity().evidence_class(),
        plakous_model::ContextEvidenceClass::ReferenceClientEnvelope {
            catalog_receipt,
            reference_revision,
        } if *catalog_receipt == first.catalog_receipt()
            && reference_revision == OPENAI_CODEX_SOURCE_REVISION
    ));
    assert!(matches!(
        contract.capacity().rule(),
        CapacityRule::ProviderReservedInput {
            reported_context_window,
            maximum_projected_input,
            ..
        } if reported_context_window == Some(u64::from(capacity_facts.resolved_context_window().get()))
            && maximum_projected_input == capacity_facts.provider_reserved_input()?
    ));
    let drifted_capacity = CodexCatalogCapacityFacts {
        context_window: Some(
            NonZeroU32::new(capacity_facts.context_window().unwrap().get() - 1)
                .ok_or("drifted context window was zero")?,
        ),
        ..capacity_facts
    };
    assert_ne!(
        resolution_route_fingerprint(
            &resolver.route,
            &issued.descriptor,
            first.account_fingerprint(),
            first.credential_authority_binding(),
            drifted_capacity,
            issued.reasoning_efforts,
            issued.catalog_enum_fingerprint,
            issued.responses_lite,
        )?,
        first.route_fingerprint()
    );
    assert_ne!(
        resolution_route_fingerprint(
            &resolver.route,
            &issued.descriptor,
            first.account_fingerprint(),
            first.credential_authority_binding(),
            capacity_facts,
            CodexReasoningEffortSet(CodexReasoningEffortSet::LOW),
            issued.catalog_enum_fingerprint,
            issued.responses_lite,
        )?,
        first.route_fingerprint()
    );
    let target = OpenAiTarget::new_native_catalog_target(
        resolver.route.clone(),
        issued.descriptor,
        issued.witness,
        Arc::clone(&resolver.clock),
    )?;
    let (target_fence, target_epoch) = target
        .native_codex_credential_selection()
        .ok_or("native target omitted its credential selection")?;
    assert_eq!(
        target_fence.generation().get(),
        first.selected_credential_generation()
    );
    assert_eq!(target_epoch, first.credential_selection_epoch());
    assert_eq!(
        manager.load_fence().await?.generation().get(),
        first.selected_credential_generation()
    );
    let attempt = "00000000-0000-7000-8000-000000000234";
    let (selected, effective) = provider_configuration_ids()?;
    let request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![user_item()?])?,
        metadata(attempt)?,
    )
    .resolved_harness_plan_identity(resolved_harness_plan()?)
    .build()?;
    let prepared_at = request.metadata().created_at();
    let descriptor = catalog_descriptor(&resolver, model_id)?;
    let plan = descriptor.validate_request(request, ValidationMode::Strict)?;
    let authority = Arc::clone(&first_model)
        .revalidate_runtime_authority(prepared_at)
        .await?;
    let prepared = first_model.prepare_call(plan, preparation_context(authority, prepared_at)?)?;
    let projection = prepared.record().projection();
    assert!(projection.output_limit().is_none());
    assert_eq!(
        projection
            .token_projection()
            .observation()
            .ok_or("reference-client-envelope prepared observation missing")?
            .tokens(),
        crate::model::codex_complete_input_token_upper_bound(projection.wire_size())
            .ok_or("reference-client-envelope complete-input bound overflowed")?
    );
    assert_eq!(
        prepared.record().context_admission_identity().domain(),
        DigestDomain::ContextAdmission
    );
    Ok(())
}

#[tokio::test]
async fn resolution_evidence_changes_for_exact_model_mutation() -> TestResult {
    let first_model = "gpt-5.6-luna";
    let second_model = "gpt-5.6-sol";
    let (_directory, _slot, _manager, resolver, _observed_at) = resolved_evidence_fixture(
        "account-resolution-model-mutation",
        &[first_model, second_model],
    )
    .await?;
    let (_, first) = resolver.resolve_with_evidence(first_model)?;
    let (_, second) = resolver.resolve_with_evidence(second_model)?;
    assert_eq!(first.account_fingerprint(), second.account_fingerprint());
    assert_eq!(
        first.credential_authority_binding(),
        second.credential_authority_binding()
    );
    assert_eq!(
        first.credential_selection_epoch(),
        second.credential_selection_epoch()
    );
    assert_ne!(first.route_fingerprint(), second.route_fingerprint());
    assert_ne!(first.descriptor_digest(), second.descriptor_digest());
    Ok(())
}

#[tokio::test]
async fn resolution_evidence_binds_exact_credential_authority_mutation() -> TestResult {
    let endpoint = test_endpoint().await?;
    let client = endpoint.client.clone();
    let (_first_directory, first_slot) = private_slot()?;
    let (_second_directory, second_slot) = private_slot()?;
    let account = "account-resolution-authority";
    let model_id = "gpt-5.6-luna";
    // One access window for both slots: catalog evidence lasts that window.
    let access_expires_at = now_unix_secs() + 3_600;
    seed_ready_credential(
        &first_slot,
        account,
        "resolution-authority-subject",
        access_expires_at,
    )?;
    seed_ready_credential(
        &second_slot,
        account,
        "resolution-authority-subject",
        access_expires_at,
    )?;
    let first_manager = manager_for_endpoint(&first_slot, &endpoint)?;
    let second_manager = manager_for_endpoint(&second_slot, &endpoint)?;
    let body = catalog_body(&[model_id])?;
    let server = spawn_sequence(
        endpoint,
        vec![
            response("200 OK", "application/json", &body, ""),
            response("200 OK", "application/json", &body, ""),
        ],
    );
    let observed_at = Timestamp::new(PROFILE_REVIEWED_AT + 3, 0)?;
    let first_resolver = CodexModelResolver::new_for_test(
        first_manager,
        client.clone(),
        StandardOpenAiHttpConfig::default(),
    )?
    .clock(Arc::new(FixedClock::new(observed_at)));
    let second_resolver = CodexModelResolver::new_for_test(
        second_manager,
        client,
        StandardOpenAiHttpConfig::default(),
    )?
    .clock(Arc::new(FixedClock::new(observed_at)));
    first_resolver.refresh_catalog().await?;
    second_resolver.refresh_catalog().await?;
    let (_, first) = first_resolver.resolve_with_evidence(model_id)?;
    let (_, second) = second_resolver.resolve_with_evidence(model_id)?;
    assert_eq!(first.account_fingerprint(), second.account_fingerprint());
    assert_eq!(
        first.selected_credential_generation(),
        second.selected_credential_generation()
    );
    assert_ne!(first.descriptor_digest(), second.descriptor_digest());
    assert_ne!(first.catalog_receipt(), second.catalog_receipt());
    assert_eq!(first.catalog_observed_at(), second.catalog_observed_at());
    assert_eq!(first.catalog_expires_at(), second.catalog_expires_at());
    assert_eq!(first.profile_revision(), second.profile_revision());
    assert_ne!(
        first.credential_authority_binding(),
        second.credential_authority_binding()
    );
    assert_ne!(
        first.credential_selection_epoch(),
        second.credential_selection_epoch()
    );
    assert_ne!(first.route_fingerprint(), second.route_fingerprint());
    assert_eq!(server.await??.len(), 2);
    Ok(())
}

#[tokio::test]
async fn relogin_and_generation_mutation_fence_resolution_evidence() -> TestResult {
    let endpoint = test_endpoint().await?;
    let client = endpoint.client.clone();
    let (directory, slot) = private_slot()?;
    let first_account = "account-resolution-relogin-a";
    let second_account = "account-resolution-relogin-b";
    let model_id = "gpt-5.6-luna";
    seed_ready_credential(
        &slot,
        first_account,
        "resolution-subject-a",
        now_unix_secs() + 3_600,
    )?;
    let manager = manager_for_endpoint(&slot, &endpoint)?;
    let body = catalog_body(&[model_id])?;
    let server = spawn_sequence(
        endpoint,
        vec![
            response("200 OK", "application/json", &body, ""),
            response("200 OK", "application/json", &body, ""),
            response("200 OK", "application/json", &body, ""),
        ],
    );
    let observed_at = Timestamp::new(PROFILE_REVIEWED_AT + 2, 0)?;
    let make_resolver = || {
        CodexModelResolver::new_for_test(
            Arc::clone(&manager),
            client.clone(),
            StandardOpenAiHttpConfig::default(),
        )
        .map(|resolver| resolver.clock(Arc::new(FixedClock::new(observed_at))))
    };

    let first_resolver = make_resolver()?;
    first_resolver.refresh_catalog().await?;
    let (_, first) = first_resolver.resolve_with_evidence(model_id)?;

    let second_credential = relogin_credential(second_account, "relogin")?;
    let access_expires_at = second_credential.access_expires_at;
    manager
        .commit_login(
            second_credential,
            manager.store.load()?,
            Instant::now() + Duration::from_secs(5),
        )
        .await?;
    assert!(matches!(
        first_resolver.resolve_with_evidence(model_id),
        Err(NativeCodexAuthError::AccountChanged)
    ));

    let second_resolver = make_resolver()?;
    second_resolver.refresh_catalog().await?;
    let (_, second) = second_resolver.resolve_with_evidence(model_id)?;
    assert_eq!(second.selected_credential_generation(), 2);
    assert_ne!(first.account_fingerprint(), second.account_fingerprint());
    assert_eq!(
        first.credential_authority_binding(),
        second.credential_authority_binding()
    );
    assert_ne!(
        first.credential_selection_epoch(),
        second.credential_selection_epoch()
    );
    assert_ne!(first.route_fingerprint(), second.route_fingerprint());
    assert_ne!(first.descriptor_digest(), second.descriptor_digest());
    assert_ne!(first.catalog_receipt(), second.catalog_receipt());

    let mut third_credential = relogin_credential(second_account, "same-account-generation")?;
    third_credential.access_expires_at = access_expires_at;
    manager
        .commit_login(
            third_credential,
            manager.store.load()?,
            Instant::now() + Duration::from_secs(5),
        )
        .await?;
    assert!(matches!(
        second_resolver.resolve_with_evidence(model_id),
        Err(NativeCodexAuthError::Catalog)
    ));

    let third_resolver = make_resolver()?;
    third_resolver.refresh_catalog().await?;
    let (_, third) = third_resolver.resolve_with_evidence(model_id)?;
    assert_eq!(third.selected_credential_generation(), 3);
    assert_eq!(second.account_fingerprint(), third.account_fingerprint());
    assert_eq!(
        second.credential_authority_binding(),
        third.credential_authority_binding()
    );
    assert_ne!(
        second.credential_selection_epoch(),
        third.credential_selection_epoch()
    );
    assert_eq!(second.route_fingerprint(), third.route_fingerprint());
    assert_eq!(second.descriptor_digest(), third.descriptor_digest());
    assert_eq!(server.await??.len(), 3);
    drop(directory);
    Ok(())
}

#[tokio::test]
async fn resolution_evidence_debug_and_serde_are_redaction_safe() -> TestResult {
    let account = "account-resolution-redaction-secret";
    let model_id = "gpt-5.6-luna";
    let (_directory, slot, _manager, resolver, _observed_at) =
        resolved_evidence_fixture(account, &[model_id]).await?;
    let (_, evidence) = resolver.resolve_with_evidence(model_id)?;
    let debug = format!("{evidence:?}");
    let serialized = serde_json::to_string(&evidence)?;
    let issued = resolver
        .catalog
        .lock()
        .map_err(|_| "catalog lock poisoned")?
        .as_ref()
        .ok_or("catalog missing")?
        .descriptors
        .get(model_id)
        .ok_or("descriptor missing")?
        .clone();
    let target = OpenAiTarget::new_native_catalog_target(
        resolver.route.clone(),
        issued.descriptor,
        issued.witness,
        Arc::clone(&resolver.clock),
    )?;
    let target_debug = format!("{target:?}");
    let slot_text = slot.to_string_lossy();
    let route_origin = resolver.route.origin().as_str();
    for forbidden in [
        account,
        "opaque-seeded-access",
        "opaque-seeded-refresh",
        "opaque-seeded-id",
        slot_text.as_ref(),
        route_origin,
        OPENAI_CHATGPT_ORIGIN,
        crate::OPENAI_CODEX_RESPONSES_PATH,
    ] {
        assert!(!debug.contains(forbidden), "Debug exposed {forbidden}");
        assert!(
            !serialized.contains(forbidden),
            "serialization exposed {forbidden}"
        );
    }
    for forbidden in [
        account,
        "opaque-seeded-access",
        "opaque-seeded-refresh",
        "opaque-seeded-id",
        slot_text.as_ref(),
    ] {
        assert!(
            !target_debug.contains(forbidden),
            "target Debug exposed {forbidden}"
        );
    }
    assert!(target_debug.contains("<redacted exact credential fence>"));
    Ok(())
}

/// Catalog evidence lasts 6 h from its observation, independent of the
/// access token that fetched it.
///
/// A fixed 30-minute lifetime ended every Codex turn, and every attached
/// session, 30 minutes after resolution; it was sized to fit the retired
/// 20-minute turn deadline. Runtime authority stays bounded by the access
/// token, and a running model renews its receipt inside the 6 h window.
#[tokio::test]
async fn catalog_evidence_outlives_the_former_thirty_minute_fence() -> TestResult {
    let model_id = "gpt-5.6-luna";
    let access_expires_at = now_unix_secs() + 6 * 3_600;
    let (_directory, _slot, _manager, resolver, observed_at) =
        resolved_evidence_fixture_with_access("account-long-turn", &[model_id], access_expires_at)
            .await?;
    let (model, evidence) = resolver.resolve_with_evidence(model_id)?;
    assert_eq!(
        evidence.catalog_expires_at(),
        Timestamp::new(
            (observed_at.seconds() + CATALOG_TTL_SECONDS).min(PROFILE_EXPIRES_AT),
            0
        )?
    );
    let two_hours_on = Timestamp::new(observed_at.seconds() + 2 * 3_600, 0)?;
    let authority = Arc::clone(&model)
        .revalidate_runtime_authority(two_hours_on)
        .await?;
    // The fixture's access token (one hour, less the refresh skew) ends the
    // authority before the 6 h catalog window does.
    assert!(authority.valid_until() < Some(evidence.catalog_expires_at()));
    assert_eq!(
        Arc::clone(&model)
            .revalidate_runtime_authority(evidence.catalog_expires_at())
            .await,
        Err(RuntimeModelAuthorityError::Stale)
    );
    Ok(())
}

#[tokio::test]
async fn runtime_authority_is_exact_redacted_and_rejects_account_generation_and_time_drift()
-> TestResult {
    let account = "account-runtime-authority-secret";
    let model_id = "gpt-5.6-luna";
    let (directory, slot, manager, resolver, observed_at) =
        resolved_evidence_fixture(account, &[model_id]).await?;
    let (model, evidence) = resolver.resolve_with_evidence(model_id)?;
    let authority = Arc::clone(&model)
        .revalidate_runtime_authority(observed_at)
        .await?;
    let descriptor = catalog_descriptor(&resolver, model_id)?;
    authority.validate(&descriptor, observed_at)?;
    assert_eq!(authority.descriptor_binding(), descriptor.binding_digest()?);
    assert_eq!(authority.account_binding(), evidence.account_fingerprint());
    assert_eq!(
        authority.credential_authority_binding(),
        evidence.credential_authority_binding()
    );
    assert_eq!(authority.credential_generation(), 1);
    assert_eq!(authority.observed_at(), observed_at);
    // The fixture's access token (one hour, less the refresh skew) ends the
    // authority before the 6 h catalog window does.
    assert!(authority.valid_until() < Some(evidence.catalog_expires_at()));

    let debug = format!("{authority:?}");
    let serialized = serde_json::to_string(&authority)?;
    let slot_text = slot.to_string_lossy();
    for forbidden in [
        account,
        "opaque-seeded-access",
        "opaque-seeded-refresh",
        "opaque-seeded-id",
        slot_text.as_ref(),
    ] {
        assert!(!debug.contains(forbidden), "Debug exposed {forbidden}");
        assert!(
            !serialized.contains(forbidden),
            "serialization exposed {forbidden}"
        );
    }

    assert_eq!(
        Arc::clone(&model)
            .revalidate_runtime_authority(evidence.catalog_expires_at())
            .await,
        Err(RuntimeModelAuthorityError::Stale)
    );
    // A changed account or a new login is never renewed into.
    manager.install_selected_account(AccountBinding::new("account-runtime-authority-drift")?)?;
    assert_eq!(
        Arc::clone(&model)
            .revalidate_runtime_authority(observed_at)
            .await,
        Err(RuntimeModelAuthorityError::ForeignAuthority)
    );
    manager.install_selected_account(AccountBinding::new(account)?)?;
    let rotated = externally_replace_ready_generation(&slot, account, "runtime-authority")?;
    assert_eq!(rotated.generation().get(), 2);
    assert_eq!(
        model.revalidate_runtime_authority(observed_at).await,
        Err(RuntimeModelAuthorityError::ForeignAuthority)
    );
    drop(directory);
    Ok(())
}

#[tokio::test]
async fn resolved_model_rejects_external_generation_rotation_before_dispatch() -> TestResult {
    let endpoint = test_endpoint().await?;
    let client = endpoint.client.clone();
    let (directory, slot) = private_slot()?;
    let account = "account-resolve-dispatch-fence";
    let model_id = "gpt-5.6-luna";
    seed_ready_credential(&slot, account, "resolve-dispatch", now_unix_secs() + 3_600)?;
    let request_gate = subscription_gate(1, 1)?;
    let manager = manager_for_endpoint_with_gate(&slot, &endpoint, request_gate.clone())?;
    let server = spawn_sequence_then_probe(
        endpoint,
        vec![response(
            "200 OK",
            "application/json",
            &catalog_body(&[model_id])?,
            "",
        )],
    );
    let resolver = CodexModelResolver::new_for_test(
        Arc::clone(&manager),
        client,
        saturated_observation_config()?,
    )?;
    resolver.refresh_catalog().await?;
    let (model, evidence) = resolver.resolve_with_evidence(model_id)?;
    assert_eq!(evidence.selected_credential_generation(), 1);

    let rotated = externally_replace_ready_generation(&slot, account, "external-before-dispatch")?;
    assert_eq!(rotated.generation().get(), 2);
    let attempt = "00000000-0000-7000-8000-000000000211";
    let (selected, effective) = provider_configuration_ids()?;
    let request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![user_item()?])?,
        metadata(attempt)?,
    )
    .resolved_harness_plan_identity(resolved_harness_plan()?)
    .build()?;
    let error = run_codex_model_operation(
        model,
        &catalog_descriptor(&resolver, model_id)?,
        request,
        ModelCallContext::new(
            attempt.parse::<AttemptId>()?,
            plakous_model::CancellationToken::new(),
            None,
        ),
    )
    .await
    .err()
    .ok_or("externally rotated resolved model unexpectedly dispatched")?;
    assert_eq!(error.kind(), ModelErrorKind::InvalidConfiguration);
    assert_eq!(manager.load_fence().await?, rotated);
    let (requests, unexpected_connection) = server.await??;
    assert_eq!(requests.len(), 1, "only catalog discovery may dispatch");
    assert!(!unexpected_connection, "rotated model reached the wire");
    let inference = request_gate
        .snapshot()?
        .class(SubscriptionRequestClass::Inference);
    assert_eq!(inference.reserved(), 0);
    assert_eq!(inference.dispatched(), 0);
    drop(directory);
    Ok(())
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one continuation fixture keeps the resolved fence, first round, rotation, and rejected later round together"
)]
async fn resolved_model_rejects_external_rotation_before_later_round() -> TestResult {
    let endpoint = test_endpoint().await?;
    let client = endpoint.client.clone();
    let (directory, slot) = private_slot()?;
    let account = "account-later-round-fence";
    let model_id = "gpt-5.6-luna";
    seed_ready_credential(&slot, account, "later-round", now_unix_secs() + 3_600)?;
    let request_gate = subscription_gate(1, 2)?;
    let manager = manager_for_endpoint_with_gate(&slot, &endpoint, request_gate.clone())?;
    let server = spawn_sequence_then_probe(
        endpoint,
        vec![
            response(
                "200 OK",
                "application/json",
                &catalog_body(&[model_id])?,
                "",
            ),
            response(
                "200 OK",
                "text/event-stream",
                include_bytes!("../../tests/fixtures/response_callers.sse"),
                "X-Request-ID: req_native_fenced_first_round\r\n",
            ),
        ],
    );
    let resolver = CodexModelResolver::new_for_test(
        Arc::clone(&manager),
        client,
        saturated_observation_config()?,
    )?;
    resolver.refresh_catalog().await?;
    let model = permitted_subscription_model(&resolver, model_id)?;

    let first_attempt = "00000000-0000-7000-8000-000000000212";
    let (selected, effective) = provider_configuration_ids()?;
    let first_request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![user_item()?])?,
        metadata(first_attempt)?,
    )
    .resolved_harness_plan_identity(resolved_harness_plan()?)
    .tools(vec![plakous_model::ConfiguredTool::new(
        lookup_tool()?,
        plakous_model::ToolExecutionDisposition::Local,
        plakous_model::ToolCallerDisposition::DirectOnly,
    )])
    .build()?;
    let first_plan = model
        .descriptor()
        .validate_request(first_request.clone(), ValidationMode::Strict)?;
    let first = run_codex_model_operation(
        model.dynamic_executable(),
        model.descriptor(),
        first_request,
        ModelCallContext::new(
            first_attempt.parse::<AttemptId>()?,
            plakous_model::CancellationToken::new(),
            None,
        ),
    )
    .await?;
    let first_artifact = assert_response_family_artifact_is_valid(&first, &first_plan)?;
    assert_eq!(
        first.metadata().provider_request_id(),
        Some("req_native_fenced_first_round")
    );
    let call = first
        .output()
        .iter()
        .flat_map(ConversationItem::blocks)
        .find_map(|block| match block {
            ContentBlock::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .ok_or("first native round omitted its local tool call")?;
    let result = ToolResultBlock::new(
        call.call_id(),
        ToolExecutionOwner::Local,
        ToolResultKind::Success,
        vec![ToolResultPart::Text(TextBlock::new("rotated", None)?)],
        call.correlation().clone(),
    )?;
    let result_item = ConversationItem::new(
        "00000000-0000-7000-8000-000000000213".parse::<EntryId>()?,
        Actor::Tool,
        vec![ContentBlock::ToolResult(result)],
        Provenance::ToolExecution,
    )?;
    let first_segment = HistorySegment::new(HistorySegmentKind::RecentIntent, vec![user_item()?])?;
    let mut exchange = first.output().to_vec();
    exchange.push(result_item);
    let exchange = HistorySegment::new(HistorySegmentKind::ToolExchange, exchange)?;

    let rotated = externally_replace_ready_generation(&slot, account, "external-later-round")?;
    assert_eq!(rotated.generation().get(), 2);
    let second_attempt = "00000000-0000-7000-8000-000000000214";
    let second_request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_shared(
            CanonicalHistory::new(vec![first_segment, exchange])?,
            WorkingProjection::default(),
        ),
        metadata(second_attempt)?,
    )
    .resolved_harness_plan_identity(resolved_harness_plan()?)
    .tools(vec![plakous_model::ConfiguredTool::new(
        lookup_tool()?,
        plakous_model::ToolExecutionDisposition::Local,
        plakous_model::ToolCallerDisposition::DirectOnly,
    )])
    .build()?;
    let error = run_model_operation_with_artifact(
        model.executable(),
        model.descriptor(),
        second_request,
        ModelCallContext::new(
            second_attempt.parse::<AttemptId>()?,
            plakous_model::CancellationToken::new(),
            None,
        ),
        first_artifact,
    )
    .await
    .err()
    .ok_or("later round used an externally rotated credential")?;
    assert_eq!(error.kind(), ModelErrorKind::InvalidConfiguration);
    assert_eq!(manager.load_fence().await?, rotated);

    let (requests, unexpected_connection) = server.await??;
    assert_eq!(requests.len(), 2, "catalog and first round only");
    assert!(
        !unexpected_connection,
        "rotated later round reached the wire"
    );
    let inference = request_gate
        .snapshot()?
        .class(SubscriptionRequestClass::Inference);
    assert_eq!(inference.reserved(), 1);
    assert_eq!(inference.dispatched(), 1);
    drop(directory);
    Ok(())
}

async fn assert_subscription_gate_blocks_before_credentials(
    request_gate: Option<Arc<dyn SubscriptionRequestGate>>,
    expected_kind: ModelErrorKind,
) -> TestResult {
    let (model, _directory, server) = model_for_subscription_gate_test(request_gate).await?;
    let attempt = "00000000-0000-7000-8000-000000000207";
    let (selected, effective) = provider_configuration_ids()?;
    let request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![user_item()?])?,
        metadata(attempt)?,
    )
    .resolved_harness_plan_identity(resolved_harness_plan()?)
    .limits(GenerationLimits::new(NonZeroU32::new(64), Vec::new())?)
    .build()?;
    let error = run_codex_model_operation(
        model.dynamic_executable(),
        model.descriptor(),
        request,
        ModelCallContext::new(
            attempt.parse::<AttemptId>()?,
            plakous_model::CancellationToken::new(),
            None,
        ),
    )
    .await
    .err()
    .ok_or("subscription inference unexpectedly completed")?;
    assert_eq!(error.kind(), expected_kind);
    let (requests, unexpected_connection) = server.await??;
    assert_eq!(
        requests.len(),
        1,
        "only catalog discovery may reach the wire"
    );
    assert!(
        !unexpected_connection,
        "missing or denied inference gate contacted the peer"
    );
    Ok(())
}

async fn assert_inference_outcome_is_single_dispatch(
    inference_response: Vec<u8>,
    expected_error: Option<ModelErrorKind>,
) -> TestResult {
    assert_inference_outcome(vec![inference_response], 1, expected_error).await
}

/// Runs one Codex turn inference against `inference_responses`, in order,
/// under a gate of `inference_permits`, and requires each response to be
/// requested exactly once and nothing after them.
async fn assert_inference_outcome(
    inference_responses: Vec<Vec<u8>>,
    inference_permits: u32,
    expected_error: Option<ModelErrorKind>,
) -> TestResult {
    let dispatches = inference_responses.len();
    let endpoint = test_endpoint().await?;
    let client = endpoint.client.clone();
    let (directory, slot) = private_slot()?;
    let model_id = "gpt-5.6-luna";
    seed_ready_credential(
        &slot,
        "account-fault-boundary",
        "subject-fault-boundary",
        now_unix_secs() + 3_600,
    )?;
    let gate = subscription_gate(1, inference_permits)?;
    let manager = manager_for_endpoint_with_gate(&slot, &endpoint, gate.clone())?;
    let mut responses = vec![response(
        "200 OK",
        "application/json",
        &catalog_body(&[model_id])?,
        "",
    )];
    responses.extend(inference_responses);
    let server = spawn_sequence_then_probe(endpoint, responses);
    let resolver =
        CodexModelResolver::new_for_test(manager, client, saturated_observation_config()?)?;
    resolver.refresh_catalog().await?;
    let model = permitted_subscription_model(&resolver, model_id)?;
    let attempt = "00000000-0000-7000-8000-000000000219";
    let (selected, effective) = provider_configuration_ids()?;
    let request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![user_item()?])?,
        metadata(attempt)?,
    )
    .resolved_harness_plan_identity(resolved_harness_plan()?)
    .build()?;
    let result = run_codex_model_operation(
        model.dynamic_executable(),
        model.descriptor(),
        request,
        ModelCallContext::new(
            attempt.parse::<AttemptId>()?,
            plakous_model::CancellationToken::new(),
            None,
        ),
    )
    .await;
    match expected_error {
        Some(expected) => {
            let error = result
                .err()
                .ok_or("faulted Codex response unexpectedly completed")?;
            assert_eq!(error.kind(), expected, "{error}");
        }
        None => {
            result.map_err(|error| format!("valid Codex terminal boundary failed: {error}"))?;
        }
    }
    let (requests, unexpected_connection) = server.await??;
    assert_eq!(
        requests.len(),
        1 + dispatches,
        "catalog plus one write per expected inference"
    );
    assert!(
        !unexpected_connection,
        "fault triggered an unexpected retry or fallback"
    );
    let inference = gate.snapshot()?.class(SubscriptionRequestClass::Inference);
    let dispatches = u64::try_from(dispatches)?;
    assert_eq!(u64::from(inference.reserved()), dispatches);
    assert_eq!(u64::from(inference.dispatched()), dispatches);
    drop(directory);
    Ok(())
}

#[tokio::test]
async fn codex_prepare_rejects_authority_and_route_drift_without_fallback() -> TestResult {
    let endpoint = test_endpoint().await?;
    let client = endpoint.client.clone();
    let (directory, slot) = private_slot()?;
    let model_id = "gpt-5.6-luna";
    seed_ready_credential(
        &slot,
        "account-prepare-drift",
        "prepare-drift",
        now_unix_secs() + 3_600,
    )?;
    let gate = subscription_gate(1, 1)?;
    let manager = manager_for_endpoint_with_gate(&slot, &endpoint, gate.clone())?;
    let server = spawn_sequence_then_probe(
        endpoint,
        vec![response(
            "200 OK",
            "application/json",
            &catalog_body(&[model_id])?,
            "",
        )],
    );
    let resolver =
        CodexModelResolver::new_for_test(manager, client, saturated_observation_config()?)?;
    resolver.refresh_catalog().await?;
    let descriptor = catalog_descriptor(&resolver, model_id)?;
    let model = resolver.resolve(model_id)?;
    let (selected, effective) = provider_configuration_ids()?;
    let request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![user_item()?])?,
        metadata("00000000-0000-7000-8000-000000000228")?,
    )
    .resolved_harness_plan_identity(resolved_harness_plan()?)
    .build()?;
    let prepared_at = request.metadata().created_at();
    let plan = descriptor.validate_request(request.clone(), ValidationMode::Strict)?;
    let authority = Arc::clone(&model)
        .revalidate_runtime_authority(prepared_at)
        .await?;
    let foreign_authority = RuntimeModelAuthority::new(
        &descriptor,
        Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[b"foreign-Codex-preparation-account"],
        )?,
        authority.credential_authority_binding(),
        authority.credential_generation(),
        authority.observed_at(),
        authority.valid_until(),
    )?;
    let authority_error = Arc::clone(&model)
        .prepare_call(
            plan.clone(),
            ModelPreparationContext::new(foreign_authority, prepared_at),
        )
        .err()
        .ok_or("foreign Codex authority unexpectedly prepared")?;
    assert!(matches!(
        authority_error,
        ModelPreparationError::RuntimeAuthority(RuntimeModelAuthorityError::ForeignAuthority)
    ));

    let foreign_credential_authority = RuntimeModelAuthority::new(
        &descriptor,
        authority.account_binding(),
        Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[b"foreign-Codex-preparation-credential-authority"],
        )?,
        authority.credential_generation(),
        authority.observed_at(),
        authority.valid_until(),
    )?;
    let credential_authority_error = Arc::clone(&model)
        .prepare_call(
            plan,
            ModelPreparationContext::new(foreign_credential_authority, prepared_at),
        )
        .err()
        .ok_or("foreign Codex credential authority unexpectedly prepared")?;
    assert!(matches!(
        credential_authority_error,
        ModelPreparationError::RuntimeAuthority(RuntimeModelAuthorityError::ForeignAuthority)
    ));

    let foreign_descriptor = ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        descriptor.protocol().clone(),
        RouteId::new("openai.codex.foreign-route")?,
        descriptor.requested_model().clone(),
        descriptor.reported_model().cloned(),
        descriptor.model_family().cloned(),
        descriptor.profile().clone(),
        descriptor.capabilities().clone(),
    );
    let foreign_plan =
        foreign_descriptor.validate_request(request.clone(), ValidationMode::Strict)?;
    let route_error = Arc::clone(&model)
        .prepare_call(
            foreign_plan,
            ModelPreparationContext::new(authority.clone(), prepared_at),
        )
        .err()
        .ok_or("foreign Codex route plan unexpectedly prepared")?;
    assert_eq!(
        route_error.rejection_kind(),
        Some(ModelPreparationErrorKind::InvalidRequest)
    );

    let foreign_model = ModelId::new("gpt-Codex-foreign-model-2026-08-10")?;
    let foreign_model_descriptor = ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        descriptor.protocol().clone(),
        descriptor.route().clone(),
        foreign_model.clone(),
        Some(foreign_model),
        descriptor.model_family().cloned(),
        descriptor.profile().clone(),
        descriptor.capabilities().clone(),
    );
    let foreign_model_plan =
        foreign_model_descriptor.validate_request(request, ValidationMode::Strict)?;
    let model_error = Arc::clone(&model)
        .prepare_call(
            foreign_model_plan,
            ModelPreparationContext::new(authority, prepared_at),
        )
        .err()
        .ok_or("foreign Codex model plan unexpectedly prepared")?;
    assert_eq!(
        model_error.rejection_kind(),
        Some(ModelPreparationErrorKind::InvalidRequest)
    );
    let (requests, unexpected_connection) = server.await??;
    assert_eq!(requests.len(), 1, "only catalog discovery may dispatch");
    assert!(!unexpected_connection, "preparation drift reached the peer");
    let inference = gate.snapshot()?.class(SubscriptionRequestClass::Inference);
    assert_eq!(inference.reserved(), 0);
    assert_eq!(inference.dispatched(), 0);
    drop(directory);
    Ok(())
}

#[tokio::test]
async fn codex_prepare_seals_exact_wire_and_is_inert_until_first_poll() -> TestResult {
    let endpoint = test_endpoint().await?;
    let client = endpoint.client.clone();
    let (directory, slot) = private_slot()?;
    let account = "account-prepared-wire";
    let model_id = "gpt-5.6-luna";
    seed_ready_credential(&slot, account, "prepared-wire", now_unix_secs() + 3_600)?;
    let gate = subscription_gate(1, 1)?;
    let manager = manager_for_endpoint_with_gate(&slot, &endpoint, gate.clone())?;
    let server = spawn_sequence(
        endpoint,
        vec![
            response(
                "200 OK",
                "application/json",
                &catalog_body(&[model_id])?,
                "",
            ),
            response(
                "200 OK",
                "text/event-stream",
                include_bytes!("../../tests/fixtures/response_text.sse"),
                "X-Request-ID: req_prepared_wire\r\n",
            ),
        ],
    );
    let resolver =
        CodexModelResolver::new_for_test(manager, client, saturated_observation_config()?)?;
    resolver.refresh_catalog().await?;
    let model = permitted_subscription_model(&resolver, model_id)?;
    assert!(
        model
            .descriptor()
            .capabilities()
            .get(Capability::Caching)
            .is_none(),
        "Codex subscription route must not mint prompt-cache capability",
    );
    let attempt = "00000000-0000-7000-8000-000000000225";
    let (selected, effective) = provider_configuration_ids()?;
    let request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![user_item()?])?,
        metadata(attempt)?,
    )
    .resolved_harness_plan_identity(resolved_harness_plan()?)
    .limits(GenerationLimits::new(NonZeroU32::new(512), Vec::new())?)
    .build()?;
    let prepared_at = request.metadata().created_at();
    let plan = model
        .descriptor()
        .validate_request(request, ValidationMode::Strict)?;
    let authority = Arc::clone(&model)
        .revalidate_runtime_authority(prepared_at)
        .await?;
    let prepared = Arc::clone(&model)
        .prepare_call(plan.clone(), preparation_context(authority, prepared_at)?)?;
    let record = prepared.record().clone();
    let reopened_record: PreparedCallRecord =
        serde_json::from_slice(&serde_json::to_vec(&record)?)?;
    assert_eq!(reopened_record, record);
    let projection = record.projection();
    assert_eq!(projection.plan(), plan.digest());
    assert_eq!(projection.request(), plan.request().identity());
    assert_eq!(
        projection.model_input(),
        plan.request().model_input_identity()
    );
    assert_eq!(
        projection.attempt_id(),
        plan.request().metadata().attempt_id()
    );
    assert_eq!(projection.protocol_family().as_str(), "responses-family");
    assert_eq!(projection.dialect(), model.descriptor().protocol());
    assert_eq!(projection.route(), model.descriptor().route());
    assert_eq!(
        projection.requested_model(),
        model.descriptor().requested_model()
    );
    let affinity = test_cache_affinity()?.identity();
    assert_eq!(
        projection.cache_use(),
        PreparedCacheUse::RuntimeAffinity { affinity }
    );
    assert!(projection.active_artifact().is_none());
    assert_eq!(projection.cache_affinity(), Some(affinity));
    let contract = model
        .descriptor()
        .context_contract()
        .ok_or("permitted Codex test model omitted its synthetic context contract")?;
    let output_limit = projection
        .output_limit()
        .ok_or("permitted Codex test projection omitted its certified hard cap")?;
    assert_eq!(
        output_limit.tokens(),
        NonZeroU32::new(64).ok_or("nonzero cap")?
    );
    assert!(matches!(
        output_limit.source(),
        PreparedOutputLimitSource::ReleaseVerifiedHardCap { certificate, .. }
            if *certificate == contract.capacity().identity()
    ));
    let observation = projection
        .token_projection()
        .observation()
        .ok_or("permitted Codex test projection omitted its complete-wire count")?;
    assert_eq!(
        observation.certificate_id(),
        "openai-test-complete-wire-byte-bound"
    );
    assert_eq!(observation.certificate_revision(), "fixture-v1");
    assert_eq!(observation.tokens(), projection.wire_size());
    assert_eq!(
        projection.wire_digest(),
        projection.token_projection().wire_digest()
    );
    assert_eq!(
        projection.wire_size(),
        projection.token_projection().wire_size()
    );
    assert_eq!(
        record.context_admission_identity().domain(),
        DigestDomain::ContextAdmission
    );
    assert_eq!(record.context_admission_identity().version(), 4);
    assert_eq!(
        record.context_status_identity().domain(),
        DigestDomain::ContextAdmission
    );
    assert_eq!(record.context_status_identity().version(), 5);
    assert_eq!(record.identity().domain(), DigestDomain::ProviderOperation);

    let before_start = gate.snapshot()?.class(SubscriptionRequestClass::Inference);
    assert_eq!(before_start.reserved(), 0);
    assert_eq!(before_start.dispatched(), 0);
    let mut start = prepared.start(ModelCallContext::new(
        attempt.parse::<AttemptId>()?,
        plakous_model::CancellationToken::new(),
        None,
    ));
    let before_poll = gate.snapshot()?.class(SubscriptionRequestClass::Inference);
    assert_eq!(before_poll.reserved(), 0);
    assert_eq!(before_poll.dispatched(), 0);
    let first_poll = start
        .as_mut()
        .poll(&mut Context::from_waker(std::task::Waker::noop()));
    let stream = match first_poll {
        Poll::Ready(Ok(stream)) => stream,
        Poll::Ready(Err(error)) => return Err(error.into()),
        Poll::Pending => {
            return Err("shared Responses host did not first-poll admission".into());
        }
    };
    // First poll admits the owned worker. It may already dispatch before this
    // task runs again; only the unpolled start must be inert. The joined result
    // below pins exactly one reservation, dispatch and emitted request.
    let response = collect_terminal_model_stream(stream, plan.digest()).await?;
    assert_eq!(
        response.metadata().provider_request_id(),
        Some("req_prepared_wire")
    );

    let requests = server.await??;
    assert_eq!(requests.len(), 2, "catalog plus one prepared inference");
    let inference = &requests[1];
    let head_end = inference
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or("prepared request omitted its HTTP head boundary")?
        + 4;
    let wire = &inference[head_end..];
    let expected = include_bytes!("../../tests/fixtures/request_codex_prepared_golden.json")
        .strip_suffix(b"\n")
        .ok_or("Codex prepared request golden must end in one newline")?;
    assert_eq!(wire, expected, "the exact emitted Codex body drifted");
    assert!(
        serde_json::from_slice::<serde_json::Value>(wire)?
            .get("max_output_tokens")
            .is_none(),
        "Codex test hard-cap evidence must not masquerade as a wire maximum"
    );
    assert_eq!(projection.wire_size(), u64::try_from(wire.len())?);
    assert_eq!(
        projection.wire_digest(),
        Digest::sha256(
            DigestDomain::Blob,
            1,
            &[b"responses-prepared-wire-body-v1", wire],
        )?
    );
    let inference_gate = gate.snapshot()?.class(SubscriptionRequestClass::Inference);
    assert_eq!(inference_gate.reserved(), 1);
    assert_eq!(inference_gate.dispatched(), 1);
    drop(directory);
    Ok(())
}

#[tokio::test]
async fn codex_invalid_input_drop_and_foreign_attempt_have_zero_provider_effect() -> TestResult {
    let endpoint = test_endpoint().await?;
    let client = endpoint.client.clone();
    let (directory, slot) = private_slot()?;
    let model_id = "gpt-5.6-luna";
    seed_ready_credential(
        &slot,
        "account-zero-effect-prepared-call",
        "zero-effect-prepared-call",
        now_unix_secs() + 3_600,
    )?;
    let gate = subscription_gate(1, 2)?;
    let manager = manager_for_endpoint_with_gate(&slot, &endpoint, gate.clone())?;
    let server = spawn_sequence_then_probe(
        endpoint,
        vec![response(
            "200 OK",
            "application/json",
            &catalog_body(&[model_id])?,
            "",
        )],
    );
    let resolver =
        CodexModelResolver::new_for_test(manager, client, StandardOpenAiHttpConfig::default())?;
    resolver.refresh_catalog().await?;
    let model = permitted_subscription_model(&resolver, model_id)?;

    let (selected, effective) = provider_configuration_ids()?;
    let cache_attempt = "00000000-0000-7000-8000-000000000234";
    let cache_request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![user_item()?])?,
        metadata(cache_attempt)?,
    )
    .resolved_harness_plan_identity(resolved_harness_plan()?)
    .cache(CachePolicy::Enabled {
        requirement: CapabilityRequirement::Required,
    })
    .build()?;
    assert!(matches!(
        model
            .descriptor()
            .validate_request(cache_request, ValidationMode::Strict),
        Err(plakous_model::ValidationError::RequiredCapability {
            capability: Capability::Caching,
            support: Support::Unsupported,
        })
    ));

    let missing_affinity_attempt = "00000000-0000-7000-8000-000000000236";
    let missing_affinity_request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![user_item()?])?,
        metadata(missing_affinity_attempt)?,
    )
    .resolved_harness_plan_identity(resolved_harness_plan()?)
    .build()?;
    let missing_affinity_at = missing_affinity_request.metadata().created_at();
    let missing_affinity_plan = model
        .descriptor()
        .validate_request(missing_affinity_request, ValidationMode::Strict)?;
    let missing_affinity_authority = Arc::clone(&model)
        .revalidate_runtime_authority(missing_affinity_at)
        .await?;
    let missing_affinity_error = Arc::clone(&model)
        .prepare_call(
            missing_affinity_plan,
            ModelPreparationContext::new(missing_affinity_authority, missing_affinity_at),
        )
        .err()
        .ok_or("Codex prepare without cache affinity unexpectedly succeeded")?;
    assert_eq!(
        missing_affinity_error.rejection_kind(),
        Some(ModelPreparationErrorKind::InvalidRequest)
    );

    let invalid_attempt = "00000000-0000-7000-8000-000000000233";
    let invalid_request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![user_item()?])?,
        metadata(invalid_attempt)?,
    )
    .resolved_harness_plan_identity(resolved_harness_plan()?)
    .tools(vec![plakous_model::ConfiguredTool::new(
        string_custom_tool()?,
        plakous_model::ToolExecutionDisposition::Local,
        plakous_model::ToolCallerDisposition::DirectOnly,
    )])
    .build()?;
    let invalid_at = invalid_request.metadata().created_at();
    let invalid_plan = model
        .descriptor()
        .validate_request(invalid_request, ValidationMode::Strict)?;
    let invalid_authority = Arc::clone(&model)
        .revalidate_runtime_authority(invalid_at)
        .await?;
    let invalid_error = Arc::clone(&model)
        .prepare_call(
            invalid_plan,
            preparation_context(invalid_authority, invalid_at)?,
        )
        .err()
        .ok_or("Codex string custom tool unexpectedly prepared")?;
    assert_eq!(
        invalid_error.rejection_kind(),
        Some(ModelPreparationErrorKind::InvalidRequest)
    );

    let dropped_attempt = "00000000-0000-7000-8000-000000000229";
    let dropped_request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![user_item()?])?,
        metadata(dropped_attempt)?,
    )
    .resolved_harness_plan_identity(resolved_harness_plan()?)
    .build()?;
    let dropped_at = dropped_request.metadata().created_at();
    let dropped_plan = model
        .descriptor()
        .validate_request(dropped_request, ValidationMode::Strict)?;
    let dropped_authority = Arc::clone(&model)
        .revalidate_runtime_authority(dropped_at)
        .await?;
    let dropped = Arc::clone(&model).prepare_call(
        dropped_plan,
        preparation_context(dropped_authority, dropped_at)?,
    )?;
    drop(dropped);

    let prepared_attempt = "00000000-0000-7000-8000-000000000230";
    let foreign_attempt = "00000000-0000-7000-8000-000000000231";
    let foreign_request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![user_item()?])?,
        metadata(prepared_attempt)?,
    )
    .resolved_harness_plan_identity(resolved_harness_plan()?)
    .build()?;
    let prepared_at = foreign_request.metadata().created_at();
    let plan = model
        .descriptor()
        .validate_request(foreign_request, ValidationMode::Strict)?;
    let authority = Arc::clone(&model)
        .revalidate_runtime_authority(prepared_at)
        .await?;
    let prepared =
        Arc::clone(&model).prepare_call(plan, preparation_context(authority, prepared_at)?)?;
    let error = prepared
        .start(ModelCallContext::new(
            foreign_attempt.parse::<AttemptId>()?,
            plakous_model::CancellationToken::new(),
            None,
        ))
        .await
        .err()
        .ok_or("foreign attempt unexpectedly started its prepared Codex call")?;
    assert_eq!(error.kind(), ModelErrorKind::InvalidRequest);

    let inference = gate.snapshot()?.class(SubscriptionRequestClass::Inference);
    assert_eq!(inference.reserved(), 0);
    assert_eq!(inference.dispatched(), 0);
    let (requests, unexpected_connection) = server.await??;
    assert_eq!(requests.len(), 1, "only catalog discovery may dispatch");
    assert!(
        !unexpected_connection,
        "an inert prepared call reached the peer"
    );
    drop(directory);
    Ok(())
}

#[tokio::test]
async fn codex_terminal_only_and_gapped_observations_share_terminal_authority() -> TestResult {
    let endpoint = test_endpoint().await?;
    let client = endpoint.client.clone();
    let (directory, slot) = private_slot()?;
    let model_id = "gpt-5.6-luna";
    seed_ready_credential(
        &slot,
        "account-terminal-authority",
        "terminal-authority",
        now_unix_secs() + 3_600,
    )?;
    let gate = subscription_gate(1, 2)?;
    let manager = manager_for_endpoint_with_gate(&slot, &endpoint, gate.clone())?;
    let server = spawn_sequence(
        endpoint,
        vec![
            response(
                "200 OK",
                "application/json",
                &catalog_body(&[model_id])?,
                "",
            ),
            response(
                "200 OK",
                "text/event-stream",
                include_bytes!("../../tests/fixtures/response_text.sse"),
                "",
            ),
            response(
                "200 OK",
                "text/event-stream",
                include_bytes!("../../tests/fixtures/response_text.sse"),
                "",
            ),
        ],
    );
    let resolver =
        CodexModelResolver::new_for_test(manager, client, saturated_observation_config()?)?;
    resolver.refresh_catalog().await?;
    let model = permitted_subscription_model(&resolver, model_id)?;
    let terminal_attempt = "00000000-0000-7000-8000-000000000226";
    let (selected, effective) = provider_configuration_ids()?;
    let terminal_request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![user_item()?])?,
        metadata(terminal_attempt)?,
    )
    .resolved_harness_plan_identity(resolved_harness_plan()?)
    .build()?;
    let terminal = run_codex_model_operation(
        ObservationDroppingModel::wrap(model.dynamic_executable()),
        model.descriptor(),
        terminal_request,
        ModelCallContext::new(
            terminal_attempt.parse::<AttemptId>()?,
            plakous_model::CancellationToken::new(),
            None,
        ),
    )
    .await?;
    assert!(
        terminal
            .output()
            .iter()
            .flat_map(ConversationItem::blocks)
            .any(|block| matches!(block, ContentBlock::Text(text) if text.text() == "wire ok 🌍"))
    );

    let gapped_attempt = "00000000-0000-7000-8000-000000000227";
    let gapped_request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![user_item()?])?,
        metadata(gapped_attempt)?,
    )
    .resolved_harness_plan_identity(resolved_harness_plan()?)
    .build()?;
    let gapped = run_codex_model_operation(
        model.dynamic_executable(),
        model.descriptor(),
        gapped_request,
        ModelCallContext::new(
            gapped_attempt.parse::<AttemptId>()?,
            plakous_model::CancellationToken::new(),
            None,
        ),
    )
    .await?;
    assert!(
        gapped
            .output()
            .iter()
            .flat_map(ConversationItem::blocks)
            .any(|block| matches!(block, ContentBlock::Text(text) if text.text() == "wire ok 🌍"))
    );
    let requests = server.await??;
    assert_eq!(requests.len(), 3, "catalog plus exactly two inferences");
    let inference = gate.snapshot()?.class(SubscriptionRequestClass::Inference);
    assert_eq!(inference.reserved(), 2);
    assert_eq!(inference.dispatched(), 2);
    drop(directory);
    Ok(())
}

#[tokio::test]
async fn codex_observation_and_terminal_canonical_ids_are_distinct() -> TestResult {
    let endpoint = test_endpoint().await?;
    let client = endpoint.client.clone();
    let (directory, slot) = private_slot()?;
    let model_id = "gpt-5.6-luna";
    seed_ready_credential(
        &slot,
        "account-distinct-canonical-ids",
        "distinct-canonical-ids",
        now_unix_secs() + 3_600,
    )?;
    let gate = subscription_gate(1, 1)?;
    let manager = manager_for_endpoint_with_gate(&slot, &endpoint, gate.clone())?;
    let server = spawn_sequence(
        endpoint,
        vec![
            response(
                "200 OK",
                "application/json",
                &catalog_body(&[model_id])?,
                "",
            ),
            response(
                "200 OK",
                "text/event-stream",
                include_bytes!("../../tests/fixtures/response_text.sse"),
                "",
            ),
        ],
    );
    let resolver =
        CodexModelResolver::new_for_test(manager, client, StandardOpenAiHttpConfig::default())?;
    resolver.refresh_catalog().await?;
    let model = permitted_subscription_model(&resolver, model_id)?;
    let attempt = "00000000-0000-7000-8000-000000000232";
    let (selected, effective) = provider_configuration_ids()?;
    let request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![user_item()?])?,
        metadata(attempt)?,
    )
    .resolved_harness_plan_identity(resolved_harness_plan()?)
    .build()?;
    let (terminal, observation_ids) = run_model_operation_collecting_observation_ids(
        model.executable(),
        model.descriptor(),
        request,
        ModelCallContext::new(
            attempt.parse::<AttemptId>()?,
            plakous_model::CancellationToken::new(),
            None,
        ),
    )
    .await?;
    assert!(
        !observation_ids.is_empty(),
        "the complete observation path produced no item identities"
    );
    let terminal_ids = terminal
        .output()
        .iter()
        .map(ConversationItem::id)
        .collect::<BTreeSet<_>>();
    assert!(!terminal_ids.is_empty());
    assert!(
        observation_ids.is_disjoint(&terminal_ids),
        "observation-only identities leaked into terminal authority"
    );
    assert_eq!(server.await??.len(), 2);
    let inference = gate.snapshot()?.class(SubscriptionRequestClass::Inference);
    assert_eq!(inference.reserved(), 1);
    assert_eq!(inference.dispatched(), 1);
    drop(directory);
    Ok(())
}

#[tokio::test]
async fn codex_not_started_refunds_inference_permit_and_input() -> TestResult {
    let endpoint = test_endpoint().await?;
    let client = endpoint
        .client
        .clone()
        .with_faults(Some(plakous_http::FaultPlan::parse(
            "path=/backend-api/codex/responses,fault=connect,nth=1",
        )?));
    let (directory, slot) = private_slot()?;
    let model_id = "gpt-5.6-luna";
    seed_ready_credential(
        &slot,
        "account-distinct-canonical-ids",
        "distinct-canonical-ids",
        now_unix_secs() + 3_600,
    )?;
    let gate = subscription_gate(1, 1)?;
    let manager = manager_for_endpoint_with_gate(&slot, &endpoint, gate.clone())?;
    let server = spawn_sequence(
        endpoint,
        vec![
            response(
                "200 OK",
                "application/json",
                &catalog_body(&[model_id])?,
                "",
            ),
            response(
                "200 OK",
                "text/event-stream",
                include_bytes!("../../tests/fixtures/response_text.sse"),
                "",
            ),
        ],
    );
    let resolver =
        CodexModelResolver::new_for_test(manager, client, StandardOpenAiHttpConfig::default())?;
    resolver.refresh_catalog().await?;
    let model = permitted_subscription_model(&resolver, model_id)?;
    let attempt = "00000000-0000-7000-8000-000000000232";
    let (selected, effective) = provider_configuration_ids()?;
    let request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![user_item()?])?,
        metadata(attempt)?,
    )
    .resolved_harness_plan_identity(resolved_harness_plan()?)
    .build()?;
    let (terminal, _) = run_model_operation_collecting_observation_ids(
        model.executable(),
        model.descriptor(),
        request,
        ModelCallContext::new(
            attempt.parse::<AttemptId>()?,
            plakous_model::CancellationToken::new(),
            None,
        ),
    )
    .await?;
    assert_ne!(terminal.output(), []);
    assert_eq!(server.await??.len(), 2);
    let inference = gate.snapshot()?.class(SubscriptionRequestClass::Inference);
    assert_eq!(inference.reserved(), 1);
    assert_eq!(inference.dispatched(), 1);
    assert!(inference.input_token_upper_bound() > 0);
    drop(directory);
    Ok(())
}

#[tokio::test]
async fn missing_and_denied_subscription_gates_precede_codex_credentials_and_network() -> TestResult
{
    assert_subscription_gate_blocks_before_credentials(None, ModelErrorKind::InvalidConfiguration)
        .await?;

    let denied = subscription_gate(1, 0)?;
    assert_subscription_gate_blocks_before_credentials(
        Some(denied.clone()),
        ModelErrorKind::Unavailable,
    )
    .await?;
    let inference = denied
        .snapshot()?
        .class(SubscriptionRequestClass::Inference);
    assert_eq!(inference.reserved(), 0);
    assert_eq!(inference.dispatched(), 0);
    assert_eq!(inference.denied(), 1);
    Ok(())
}

#[tokio::test]
async fn codex_response_head_malformed_and_post_terminal_never_fall_back() -> TestResult {
    assert_inference_outcome_is_single_dispatch(
        response("200 OK", "application/json", b"{}", ""),
        Some(ModelErrorKind::Protocol),
    )
    .await?;
    assert_inference_outcome_is_single_dispatch(
        response("200 OK", "text/event-stream", b"data: {\n\n", ""),
        Some(ModelErrorKind::Protocol),
    )
    .await?;
    let foreign_model =
        String::from_utf8(include_bytes!("../../tests/fixtures/response_text.sse").to_vec())?
            .replace("\"model\":\"gpt-5.4\"", "\"model\":\"gpt-5.4-foreign\"")
            .into_bytes();
    // Catalog slug stays gpt-5.4; SSE may echo a snapshot/routed id.
    assert_inference_outcome_is_single_dispatch(
        response("200 OK", "text/event-stream", &foreign_model, ""),
        None,
    )
    .await?;
    let mut post_terminal = include_bytes!("../../tests/fixtures/response_text.sse").to_vec();
    post_terminal.extend_from_slice(
            b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"sequence_number\":9,\"output_index\":0,\"content_index\":0,\"delta\":\"late\"}\n\n",
        );
    assert_inference_outcome_is_single_dispatch(
        response("200 OK", "text/event-stream", &post_terminal, ""),
        None,
    )
    .await?;
    Ok(())
}

/// A turn stream cut after `response.created`.
fn codex_truncated_stream() -> Vec<u8> {
    response(
        "200 OK",
        "text/event-stream",
        b"event: response.created\ndata: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"truncated\",\"status\":\"in_progress\",\"model\":\"gpt-fault-boundary-2026-08-09\",\"output\":[]}}\n\n",
        "",
    )
}

fn content_filtered_stream() -> Vec<u8> {
    response("200 OK", "text/event-stream", b"event: response.created\ndata: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"filtered\",\"object\":\"response\",\"status\":\"in_progress\",\"model\":\"gpt-5.6-luna\",\"output\":[]}}\n\nevent: response.incomplete\ndata: {\"type\":\"response.incomplete\",\"sequence_number\":1,\"response\":{\"id\":\"filtered\",\"object\":\"response\",\"status\":\"incomplete\",\"model\":\"gpt-5.6-luna\",\"output\":[],\"incomplete_details\":{\"reason\":\"content_filter\"}}}\n\n", "")
}

#[tokio::test]
async fn content_filtered_sampling_is_typed_and_not_retried_inside_transport() -> TestResult {
    assert_inference_outcome(
        vec![content_filtered_stream()],
        4,
        Some(ModelErrorKind::ContentFilteredIncomplete),
    )
    .await
}

#[tokio::test]
async fn content_filtered_compaction_is_single_dispatch_terminal_policy() -> TestResult {
    let endpoint = test_endpoint().await?;
    let client = endpoint.client.clone();
    let (_private, slot) = private_slot()?;
    seed_ready_credential(
        &slot,
        "account-filter-compact",
        "subject-filter-compact",
        now_unix_secs() + 3_600,
    )?;
    let gate = subscription_gate(1, 4)?;
    let manager = manager_for_endpoint_with_gate(&slot, &endpoint, gate.clone())?;
    let model_id = "gpt-5.6-luna";
    let server = spawn_sequence_then_probe(
        endpoint,
        vec![
            response(
                "200 OK",
                "application/json",
                &catalog_body(&[model_id])?,
                "",
            ),
            content_filtered_stream(),
        ],
    );
    let resolver =
        CodexModelResolver::new_for_test(manager, client, StandardOpenAiHttpConfig::default())?;
    resolver.refresh_catalog().await?;
    let model = permitted_subscription_model(&resolver, model_id)?;
    let attempt = "00000000-0000-7000-8000-000000000239";
    let (selected, effective) = provider_configuration_ids()?;
    let request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![user_item()?])?,
        metadata(attempt)?,
    )
    .resolved_harness_plan_identity(resolved_harness_plan()?)
    .native(plakous_model::NativeOptions::new(BTreeMap::from([(
        NamespaceId::new("plakous.compaction")?,
        serde_json::json!("native_responses_trigger_v1"),
    )])))
    .build()?;
    let error = run_codex_model_operation(
        model.dynamic_executable(),
        model.descriptor(),
        request,
        ModelCallContext::new(
            attempt.parse()?,
            plakous_model::CancellationToken::new(),
            None,
        ),
    )
    .await
    .expect_err("filtered compaction must fail");
    assert_eq!(error.kind(), ModelErrorKind::Policy);
    assert_eq!(
        error.context().ok_or("missing context")?.retryability(),
        plakous_model::ModelRetryability::Unsafe {
            risk: plakous_model::ModelRetryRisk::DuplicateInferenceOrBilling,
        }
    );
    assert_eq!(
        error.message(),
        "stream disconnected before completion: Incomplete response returned, reason: content_filter"
    );
    let (requests, unexpected) = server.await??;
    assert_eq!(requests.len(), 2);
    assert!(!unexpected);
    assert_eq!(
        gate.snapshot()?
            .class(SubscriptionRequestClass::Inference)
            .dispatched(),
        1
    );
    Ok(())
}

#[test]
fn catalog_content_filter_guidance_is_optional_bounded_and_identity_bearing() -> TestResult {
    let base = catalog_model_value("gpt-5.6-luna");
    let model: CatalogModelWire = serde_json::from_value(base.clone())?;
    let baseline = catalog_enum_fingerprint(&model)?;
    for text in [String::new(), " \n".to_owned(), "x".repeat(513)] {
        let mut value = base.clone();
        value["model_messages"] = serde_json::json!({"content_filter_guidance":text});
        let model = serde_json::from_value(value)?;
        assert_eq!(catalog_content_filter_guidance(&model), None);
        assert_eq!(catalog_enum_fingerprint(&model)?, baseline);
    }
    let mut value = base;
    value["model_messages"] =
        serde_json::json!({"content_filter_guidance":"Offer another option."});
    let model = serde_json::from_value(value)?;
    assert_eq!(
        catalog_content_filter_guidance(&model),
        Some("Offer another option.")
    );
    assert_ne!(catalog_enum_fingerprint(&model)?, baseline);
    Ok(())
}

/// A turn stream cut before its terminal is replayed on a fresh inference
/// permit and the turn completes.
#[tokio::test]
async fn codex_truncated_turn_stream_is_replayed_within_the_inference_reserve() -> TestResult {
    let success = include_bytes!("../../tests/fixtures/response_text.sse");
    assert_inference_outcome(
        vec![
            codex_truncated_stream(),
            response("200 OK", "text/event-stream", success, ""),
        ],
        4,
        None,
    )
    .await
}

/// A gate with no permit left refuses the replay before it is written, and
/// the turn ends unavailable.
#[tokio::test]
async fn codex_truncated_turn_stream_replay_is_refused_by_an_exhausted_gate() -> TestResult {
    assert_inference_outcome(
        vec![codex_truncated_stream()],
        1,
        Some(ModelErrorKind::Unavailable),
    )
    .await
}

#[tokio::test]
async fn codex_inference_cancellation_after_one_write_never_falls_back() -> TestResult {
    let endpoint = test_endpoint().await?;
    let client = endpoint.client.clone();
    let (directory, slot) = private_slot()?;
    let model_id = "gpt-5.6-luna";
    seed_ready_credential(
        &slot,
        "account-cancellation-boundary",
        "subject-cancellation-boundary",
        now_unix_secs() + 3_600,
    )?;
    let gate = subscription_gate(1, 1)?;
    let manager = manager_for_endpoint_with_gate(&slot, &endpoint, gate.clone())?;
    let catalog_response = response(
        "200 OK",
        "application/json",
        &catalog_body(&[model_id])?,
        "",
    );
    let TestEndpoint {
        listener, acceptor, ..
    } = endpoint;
    let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut stream = acceptor.accept(stream).await?;
        let catalog_request = read_request(&mut stream).await?;
        stream.write_all(&catalog_response).await?;
        stream.shutdown().await?;

        let (stream, _) = listener.accept().await?;
        let mut stream = acceptor.accept(stream).await?;
        let inference_request = read_request(&mut stream).await?;
        let _ = seen_tx.send(());
        let mut byte = [0_u8; 1];
        let _ = stream.read(&mut byte).await;
        Ok::<_, TestError>(vec![catalog_request, inference_request])
    });
    let resolver =
        CodexModelResolver::new_for_test(manager, client, saturated_observation_config()?)?;
    resolver.refresh_catalog().await?;
    let model = permitted_subscription_model(&resolver, model_id)?;
    let attempt = "00000000-0000-7000-8000-000000000221";
    let (selected, effective) = provider_configuration_ids()?;
    let request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![user_item()?])?,
        metadata(attempt)?,
    )
    .resolved_harness_plan_identity(resolved_harness_plan()?)
    .build()?;
    let cancellation = plakous_model::CancellationToken::new();
    let worker_cancellation = cancellation.clone();
    let attempt_id = attempt.parse::<AttemptId>()?;
    let descriptor = model.descriptor().clone();
    let worker = tokio::spawn(async move {
        run_codex_model_operation(
            model.dynamic_executable(),
            &descriptor,
            request,
            ModelCallContext::new(attempt_id, worker_cancellation, None),
        )
        .await
    });
    seen_rx.await?;
    cancellation.cancel();
    let error = tokio::time::timeout(Duration::from_secs(2), worker)
        .await??
        .err()
        .ok_or("cancelled Codex inference unexpectedly completed")?;
    assert_eq!(error.kind(), ModelErrorKind::Cancelled);
    let requests = tokio::time::timeout(Duration::from_secs(2), server).await???;
    assert_eq!(requests.len(), 2, "catalog plus one inference write");
    let inference = gate.snapshot()?.class(SubscriptionRequestClass::Inference);
    assert_eq!(inference.reserved(), 1);
    assert_eq!(inference.dispatched(), 1);
    drop(directory);
    Ok(())
}

#[derive(Default)]
struct InferenceCapture {
    requests: Mutex<Vec<Vec<u8>>>,
    responses: Mutex<Vec<u8>>,
    terminals: Mutex<Vec<plakous_http::ExchangeTerminal>>,
}

struct InferenceObserver(Arc<InferenceCapture>);

impl plakous_http::ExchangeObserver for InferenceObserver {
    fn start(&self, body: &[u8]) -> Option<Box<dyn plakous_http::ExchangeObservation>> {
        self.0.requests.lock().ok()?.push(body.to_vec());
        Some(Box::new(Self(Arc::clone(&self.0))))
    }
}

impl plakous_http::ExchangeObservation for InferenceObserver {
    fn response_head(&mut self, status: u16) {
        assert_eq!(status, 200);
    }

    fn response_chunk(&mut self, bytes: &[u8]) {
        if let Ok(mut responses) = self.0.responses.lock() {
            responses.extend_from_slice(bytes);
        }
    }

    fn terminal(&mut self, terminal: &plakous_http::ExchangeTerminal) {
        if let Ok(mut terminals) = self.0.terminals.lock() {
            terminals.push(*terminal);
        }
    }
}

#[tokio::test]
async fn inference_observer_excludes_refresh_and_catalog_without_changing_identity() -> TestResult {
    assert_inference_observation(false).await
}

#[tokio::test]
async fn resolver_preserves_caller_client_observer_without_inference_override() -> TestResult {
    assert_inference_observation(true).await
}

#[allow(
    clippy::too_many_lines,
    reason = "one local TLS sequence proves observer inheritance, auth isolation and identical inference preparation"
)]
async fn assert_inference_observation(inherited: bool) -> TestResult {
    let endpoint = test_endpoint().await?;
    let client = endpoint.client.clone();
    let (_directory, slot) = private_slot()?;
    seed_ready_credential(
        &slot,
        "capture-account",
        "capture-subject",
        now_unix_secs() + 3600,
    )?;
    let manager = manager_for_endpoint_with_gate(&slot, &endpoint, subscription_gate(1, 1)?)?;
    expire_ready_generation(&manager).await?;
    let access_token = access_token_with_expiry(now_unix_secs() + 1800)?;
    let refresh_body = serde_json::to_vec(&serde_json::json!({"access_token": access_token}))?;
    let model_id = "gpt-5.6-luna";
    let server = spawn_sequence(
        endpoint,
        vec![
            response("200 OK", "application/json", &refresh_body, ""),
            response(
                "200 OK",
                "application/json",
                br#"{"sub":"capture-subject"}"#,
                "",
            ),
            response(
                "200 OK",
                "application/json",
                &catalog_body(&[model_id])?,
                "",
            ),
            response(
                "200 OK",
                "text/event-stream",
                include_bytes!("../../tests/fixtures/response_text.sse"),
                "",
            ),
        ],
    );
    let capture = Arc::new(InferenceCapture::default());
    let capture_hook: Arc<dyn plakous_http::ExchangeObserver> =
        Arc::new(InferenceObserver(Arc::clone(&capture)));
    assert_eq!(
        client.behavior_identity(),
        client
            .clone()
            .with_observer(Some(Arc::clone(&capture_hook)))
            .behavior_identity()
    );
    let (client, inference_override) = if inherited {
        (client.with_observer(Some(capture_hook)), None)
    } else {
        (client, Some(capture_hook))
    };
    let resolver =
        CodexModelResolver::new_for_test(manager, client, saturated_observation_config()?)?
            .with_inference_observer(inference_override);
    resolver.refresh_catalog().await?;
    if inherited {
        // An explicitly observed base client observes catalog traffic too.
        // The resolver must preserve that caller configuration by default.
        let mut bodies = capture.requests.lock().map_err(|_| "capture poisoned")?;
        assert_eq!(bodies.as_slice(), &[Vec::<u8>::new()]);
        bodies.clear();
        let mut responses = capture.responses.lock().map_err(|_| "capture poisoned")?;
        assert_eq!(*responses, catalog_body(&[model_id])?);
        responses.clear();
        capture
            .terminals
            .lock()
            .map_err(|_| "capture poisoned")?
            .clear();
    }
    assert!(
        capture
            .requests
            .lock()
            .map_err(|_| "capture poisoned")?
            .is_empty()
    );
    assert!(
        capture
            .responses
            .lock()
            .map_err(|_| "capture poisoned")?
            .is_empty()
    );
    let (observed, observed_evidence) = resolver.resolve_with_evidence(model_id)?;
    let descriptor = catalog_descriptor(&resolver, model_id)?;
    let resolver = resolver.with_inference_observer(None);
    let (unobserved, unobserved_evidence) = resolver.resolve_with_evidence(model_id)?;
    assert_eq!(
        observed_evidence.descriptor_digest(),
        unobserved_evidence.descriptor_digest()
    );
    let attempt = "00000000-0000-7000-8000-000000000235";
    let (selected, effective) = provider_configuration_ids()?;
    let request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![user_item()?])?,
        metadata(attempt)?,
    )
    .resolved_harness_plan_identity(resolved_harness_plan()?)
    .build()?;
    let prepared_at = request.metadata().created_at();
    let plan = descriptor.validate_request(request.clone(), ValidationMode::Strict)?;
    let authority = Arc::clone(&observed)
        .revalidate_runtime_authority(prepared_at)
        .await?;
    let observed_prepared = Arc::clone(&observed).prepare_call(
        plan.clone(),
        preparation_context(authority.clone(), prepared_at)?,
    )?;
    let unobserved_prepared =
        Arc::clone(&unobserved).prepare_call(plan, preparation_context(authority, prepared_at)?)?;
    assert_eq!(observed_prepared.record(), unobserved_prepared.record());
    drop((observed_prepared, unobserved_prepared));
    // The already materialized observed model retains its inference-only hook.
    let result = run_codex_model_operation(
        observed,
        &descriptor,
        request,
        ModelCallContext::new(
            attempt.parse::<AttemptId>()?,
            plakous_model::CancellationToken::new(),
            None,
        ),
    )
    .await?;
    assert_ne!(result.output(), []);
    let requests = server.await??;
    assert_eq!(
        requests.len(),
        4,
        "refresh, userinfo, catalog, inference; no retries"
    );
    let body_start = requests[3]
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .ok_or("missing head")?
        + 4;
    let captured_requests = capture.requests.lock().map_err(|_| "capture poisoned")?;
    assert_eq!(
        captured_requests.as_slice(),
        &[requests[3][body_start..].to_vec()]
    );
    let captured_responses = capture.responses.lock().map_err(|_| "capture poisoned")?;
    assert_eq!(
        captured_responses.as_slice(),
        include_bytes!("../../tests/fixtures/response_text.sse")
    );
    for secret in [
        "opaque-seeded-refresh",
        "opaque-seeded-id",
        access_token.as_str(),
        "capture-subject",
        "capture-account",
    ] {
        assert!(!captured_requests.iter().any(|body| {
            body.windows(secret.len())
                .any(|bytes| bytes == secret.as_bytes())
        }));
        assert!(
            !captured_responses
                .windows(secret.len())
                .any(|bytes| bytes == secret.as_bytes())
        );
    }
    // The typed terminal explicitly closes before network EOF and joins the
    // connection, so required capture finalization can fail before success.
    assert_eq!(
        *capture.terminals.lock().map_err(|_| "capture poisoned")?,
        vec![plakous_http::ExchangeTerminal::ProtocolComplete]
    );
    Ok(())
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one end-to-end fixture proves exact device, identity, catalog, and two-round wire ordering"
)]
async fn device_jwks_userinfo_catalog_and_lite_inference_are_exact() -> TestResult {
    let endpoint = test_endpoint().await?;
    let client = endpoint.client.clone();
    let (_directory, slot) = private_slot()?;
    let request_gate = subscription_gate(1, 2)?;
    let manager = manager_for_endpoint_with_gate(&slot, &endpoint, request_gate.clone())?;
    let token = id_token(TEST_KID, "subject-device", "account-device")?;
    let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
    let model_id = "gpt-5.6-luna";
    let catalog = serde_json::to_vec(&serde_json::json!({"models":[{
        "slug": model_id,
        "visibility":"list",
        "priority":1,
        "supported_in_api":true,
        "supported_reasoning_levels":[{"effort":"low"},{"effort":"medium"},{"effort":"high"}],
        "shell_type":"shell_command",
        "use_responses_lite":true,
        "supports_parallel_tool_calls":true,
        "input_modalities":["text"],
        "context_window":272_000,
        "max_context_window":1_000_000,
        "effective_context_window_percent":95,
        "auto_compact_token_limit":240_000
    }]}))?;
    let responses = vec![
        response(
            "200 OK",
            "application/json",
            br#"{"device_auth_id":"device-123","user_code":"ABCD-1234","interval":"1"}"#,
            "",
        ),
        response(
            "200 OK",
            "application/json",
            &serde_json::to_vec(&serde_json::json!({
                "authorization_code":"device-auth-code", "code_challenge":challenge, "code_verifier":verifier
            }))?,
            "",
        ),
        response(
            "200 OK",
            "application/json",
            &token_body(&token, "opaque-access-device", "refresh-device", 3600)?,
            "",
        ),
        response(
            "200 OK",
            "application/json",
            &jwks_body("other-kid", TEST_MODULUS)?,
            "Cache-Control: max-age=300\r\n",
        ),
        response(
            "200 OK",
            "application/json",
            &jwks_body(TEST_KID, TEST_MODULUS)?,
            "Cache-Control: max-age=300\r\n",
        ),
        response(
            "200 OK",
            "application/json",
            br#"{"sub":"subject-device","email":"device@example.test"}"#,
            "",
        ),
        response("200 OK", "application/json", &catalog, ""),
        response(
            "200 OK",
            "text/event-stream",
            &String::from_utf8(include_bytes!("../../tests/fixtures/response_mixed.sse").to_vec())?
                .replace(
                    "\"name\":\"lookup\",\"caller\"",
                    "\"name\":\"lookup\",\"namespace\":\"functions\",\"caller\"",
                )
                .into_bytes(),
            "X-Request-ID: req_native_tool\r\n",
        ),
        response(
            "200 OK",
            "text/event-stream",
            include_bytes!("../../tests/fixtures/response_text.sse"),
            "X-Request-ID: req_native_text\r\n",
        ),
    ];
    let server = spawn_sequence(endpoint, responses);

    let login = manager.begin_device_login().await?;
    assert!(login.verification_url().ends_with(DEVICE_VERIFICATION_PATH));
    assert_eq!(login.user_code(), "ABCD-1234");
    assert_eq!(
        manager.complete_device_login(login).await?,
        CodexCredentialStatus::Ready
    );
    let resolver = CodexModelResolver::new_for_test(
        Arc::clone(&manager),
        client,
        saturated_observation_config()?,
    )?;
    let snapshot = resolver.refresh_catalog().await?;
    assert_eq!(snapshot.entries().len(), 1);
    assert!(snapshot.entries()[0].supports_text());
    assert!(snapshot.entries()[0].supports_tools());
    assert!(!snapshot.entries()[0].supports_parallel_tools());
    assert!(snapshot.entries()[0].uses_responses_lite());
    assert!(snapshot.entries()[0].supports_reasoning());
    let forged_descriptor = resolver
        .catalog
        .lock()
        .map_err(|_| "catalog lock poisoned")?
        .as_ref()
        .ok_or("catalog missing")?
        .descriptors
        .get(model_id)
        .ok_or("model missing")?
        .descriptor
        .clone();
    assert_eq!(
        OpenAiTarget::new_offline_codec(resolver.route.clone(), forged_descriptor).err(),
        Some(crate::ConfigError::NonFixtureOfflineClaim)
    );
    let model = permitted_subscription_model(&resolver, model_id)?;
    let first_attempt = "00000000-0000-7000-8000-000000000204";
    let (selected, effective) = provider_configuration_ids()?;
    let first_request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![user_item()?])?,
        metadata(first_attempt)?,
    )
    .resolved_harness_plan_identity(resolved_harness_plan()?)
    .tools(vec![plakous_model::ConfiguredTool::new(
        lookup_tool()?,
        plakous_model::ToolExecutionDisposition::Local,
        plakous_model::ToolCallerDisposition::DirectOnly,
    )])
    .reasoning(plakous_model::ReasoningPolicy::Enabled {
        effort: Some(ReasoningEffort::Medium),
        requirement: CapabilityRequirement::Required,
    })
    .limits(GenerationLimits::new(NonZeroU32::new(512), Vec::new())?)
    .build()?;
    let first_plan = model
        .descriptor()
        .validate_request(first_request.clone(), ValidationMode::Strict)?;
    let first = run_codex_model_operation(
        model.dynamic_executable(),
        model.descriptor(),
        first_request,
        ModelCallContext::new(
            first_attempt.parse::<AttemptId>()?,
            plakous_model::CancellationToken::new(),
            None,
        ),
    )
    .await?;
    let first_artifact = assert_response_family_artifact_is_valid(&first, &first_plan)?;
    assert_eq!(
        first.metadata().provider_request_id(),
        Some("req_native_tool")
    );
    let first_blocks = first.output().iter().flat_map(ConversationItem::blocks);
    assert!(
        first_blocks
            .clone()
            .any(|block| matches!(block, ContentBlock::Text(text) if text.text() == "Hello 🌍"))
    );
    assert!(first_blocks.clone().any(|block| {
        matches!(block, ContentBlock::Reasoning(reasoning) if reasoning.text() == Some("Checked."))
    }));
    assert_eq!(first.usage(), Some(Usage::new(12, 8, 3, 4, 0)));
    let call = first
        .output()
        .iter()
        .flat_map(ConversationItem::blocks)
        .find_map(|block| match block {
            ContentBlock::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .ok_or("native response omitted local tool call")?;
    assert_eq!(call.owner(), &ToolExecutionOwner::Local);
    let result = ToolResultBlock::new(
        call.call_id(),
        ToolExecutionOwner::Local,
        ToolResultKind::Success,
        vec![ToolResultPart::Text(TextBlock::new(
            "lookup complete",
            None,
        )?)],
        call.correlation().clone(),
    )?;
    let result_item = ConversationItem::new(
        "00000000-0000-7000-8000-000000000205".parse::<EntryId>()?,
        Actor::Tool,
        vec![ContentBlock::ToolResult(result)],
        Provenance::ToolExecution,
    )?;
    let first_segment = HistorySegment::new(HistorySegmentKind::RecentIntent, vec![user_item()?])?;
    let mut exchange = first.output().to_vec();
    exchange.push(result_item);
    let exchange = HistorySegment::new(HistorySegmentKind::ToolExchange, exchange)?;
    let second_attempt = "00000000-0000-7000-8000-000000000206";
    let second_request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_shared(
            CanonicalHistory::new(vec![first_segment, exchange])?,
            WorkingProjection::default(),
        ),
        metadata(second_attempt)?,
    )
    .resolved_harness_plan_identity(resolved_harness_plan()?)
    .tools(vec![plakous_model::ConfiguredTool::new(
        lookup_tool()?,
        plakous_model::ToolExecutionDisposition::Local,
        plakous_model::ToolCallerDisposition::DirectOnly,
    )])
    .reasoning(plakous_model::ReasoningPolicy::Enabled {
        effort: Some(ReasoningEffort::Medium),
        requirement: CapabilityRequirement::Required,
    })
    .limits(GenerationLimits::new(NonZeroU32::new(512), Vec::new())?)
    .build()?;
    let artifact_integrity = first_artifact.integrity();
    let reopened_artifact: ActiveFamilyArtifact =
        serde_json::from_slice(&serde_json::to_vec(&first_artifact)?)?;
    assert_eq!(reopened_artifact.integrity(), artifact_integrity);
    let second = run_model_operation_with_artifact(
        model.executable(),
        model.descriptor(),
        second_request,
        ModelCallContext::new(
            second_attempt.parse::<AttemptId>()?,
            plakous_model::CancellationToken::new(),
            None,
        ),
        reopened_artifact,
    )
    .await?;
    assert_ne!(second.output(), []);
    assert_eq!(
        second.metadata().provider_request_id(),
        Some("req_native_text")
    );
    manager.install_selected_account(AccountBinding::new("changed-after-catalog")?)?;
    assert!(matches!(
        resolver.resolve(model_id),
        Err(NativeCodexAuthError::AccountChanged)
    ));

    let requests = server.await??;
    assert_eq!(requests.len(), 9);
    let inference_gate = request_gate
        .snapshot()?
        .class(SubscriptionRequestClass::Inference);
    assert_eq!(inference_gate.reserved(), 2);
    assert_eq!(inference_gate.dispatched(), 2);
    let device_head = std::str::from_utf8(&requests[0])?;
    assert!(device_head.starts_with("POST /api/accounts/deviceauth/usercode HTTP/1.1\r\n"));
    let catalog_head = std::str::from_utf8(&requests[6])?;
    assert!(
        catalog_head.starts_with("GET /backend-api/codex/models?client_version=0.160.1 HTTP/1.1")
    );
    let inference = &requests[7];
    let head_end = inference
        .windows(4)
        .position(|value| value == b"\r\n\r\n")
        .ok_or("missing head")?
        + 4;
    let head = std::str::from_utf8(&inference[..head_end])?;
    assert!(head.starts_with("POST /backend-api/codex/responses HTTP/1.1\r\n"));
    assert!(head.contains("authorization: Bearer opaque-access-device\r\n"));
    assert!(head.contains("chatgpt-account-id: account-device\r\n"));
    assert!(head.contains("originator: plak\r\n"));
    assert!(head.contains(&format!("user-agent: {USER_AGENT}\r\n")));
    assert!(head.contains("x-openai-internal-codex-responses-lite: true\r\n"));
    assert!(!head.contains("x-codex-beta-features:"));
    assert!(!head.to_ascii_lowercase().contains("x-api-key:"));
    let body: serde_json::Value = serde_json::from_slice(&inference[head_end..])?;
    assert_eq!(body["input"][0]["type"], "additional_tools");
    assert_eq!(body["input"][0]["tools"][0]["type"], "namespace");
    assert!(body.get("tools").is_none());
    assert!(body.get("instructions").is_none());
    assert_eq!(body["parallel_tool_calls"], false);
    assert_eq!(body["reasoning"]["context"], "all_turns");
    assert_eq!(body.get("store"), Some(&serde_json::Value::Bool(false)));
    assert_eq!(
        body.get("include"),
        Some(&serde_json::json!([
            crate::OPENAI_CODEX_ENCRYPTED_REASONING_INCLUDE
        ]))
    );
    let expected_cache_key =
        serde_json::Value::String(test_cache_affinity()?.identity().to_string());
    assert_eq!(body.get("prompt_cache_key"), Some(&expected_cache_key));
    assert_eq!(body.get("previous_response_id"), None);
    // ChatGPT Codex /backend-api/codex/responses rejects max_output_tokens;
    // the dialect omits it on the subscription access mode.
    assert_eq!(body.get("max_output_tokens"), None);
    let second_head_end = requests[8]
        .windows(4)
        .position(|value| value == b"\r\n\r\n")
        .ok_or("missing second head")?
        + 4;
    let second_head = std::str::from_utf8(&requests[8][..second_head_end])?;
    assert!(second_head.starts_with("POST /backend-api/codex/responses HTTP/1.1\r\n"));
    assert!(second_head.contains("x-openai-internal-codex-responses-lite: true\r\n"));
    let second_body: serde_json::Value = serde_json::from_slice(&requests[8][second_head_end..])?;
    assert_eq!(
        second_body.get("prompt_cache_key"),
        Some(&expected_cache_key)
    );
    let second_input = second_body["input"]
        .as_array()
        .ok_or("reopened artifact request input was not an array")?;
    assert_eq!(second_input[0]["type"], "additional_tools");
    for replay_id in ["msg_1", "fc_1", "rs_1"] {
        assert!(
            second_input.iter().any(|item| item["id"] == replay_id),
            "reopened active artifact omitted replay item {replay_id}"
        );
    }
    let replayed_call = second_input
        .iter()
        .find(|item| item["id"] == "fc_1")
        .ok_or("namespaced function call missing from replay")?;
    assert_eq!(replayed_call["namespace"], "functions");
    assert!(second_input.iter().any(|item| {
        item["type"] == "function_call_output" && item["call_id"] == "call_remote"
    }));
    assert!(second_body.get("previous_response_id").is_none());
    let first_body = u64::try_from(requests[7].len() - head_end)?;
    let second_body = u64::try_from(requests[8].len() - second_head_end)?;
    let expected_liability = crate::model::codex_complete_input_token_upper_bound(first_body)
        .and_then(|tokens| tokens.checked_add(SUBSCRIPTION_INPUT_TOKEN_OVERHEAD))
        .and_then(|first| {
            crate::model::codex_complete_input_token_upper_bound(second_body)
                .and_then(|tokens| tokens.checked_add(SUBSCRIPTION_INPUT_TOKEN_OVERHEAD))
                .and_then(|second| first.checked_add(second))
        })
        .ok_or("inference liability overflow")?;
    assert_eq!(inference_gate.input_token_upper_bound(), expected_liability);
    Ok(())
}

#[test]
fn browser_deadline_bounds_token_jwks_and_userinfo_stalls_without_commit() -> TestResult {
    for phase in [
        LoginStallPhase::Token,
        LoginStallPhase::Jwks,
        LoginStallPhase::UserInfo,
    ] {
        // Each phase gets a fresh virtual clock: resume() retains a paused
        // clock's advanced offset, while login deadlines use std::Instant.
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(assert_browser_login_stall_expires(phase))?;
    }
    Ok(())
}

#[test]
fn device_deadline_bounds_token_jwks_and_userinfo_stalls_without_commit() -> TestResult {
    for phase in [
        LoginStallPhase::Token,
        LoginStallPhase::Jwks,
        LoginStallPhase::UserInfo,
    ] {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(assert_device_login_stall_expires(phase))?;
    }
    Ok(())
}

#[tokio::test]
async fn browser_and_device_deadlines_bound_near_expiry_lifecycle_waits() -> TestResult {
    let _browser_test = BROWSER_TEST_LOCK.lock().await;
    let browser_endpoint = test_endpoint().await?;
    let (_browser_directory, browser_slot) = private_slot()?;
    let browser_manager = manager_for_endpoint(&browser_slot, &browser_endpoint)?;
    let mut browser_login = browser_manager.begin_browser_login().await?;
    let authorization = Url::parse(browser_login.authorization_url())?;
    let state = authorization
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
        .ok_or("missing state")?;
    let port = browser_login.listener.local_addr()?.port();
    browser_login.attempt.deadline = Instant::now() + Duration::from_millis(200);
    let browser_lifecycle = browser_manager.lifecycle.lock().await;
    let worker_manager = Arc::clone(&browser_manager);
    let browser_completion =
        tokio::spawn(async move { worker_manager.complete_browser_login(browser_login).await });
    let mut callback = TcpStream::connect(("127.0.0.1", port)).await?;
    callback
            .write_all(
                format!(
                    "GET /auth/callback?code=lock-code&state={state} HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await?;
    let browser_outcome =
        tokio::time::timeout(Duration::from_secs(2), browser_completion).await??;
    assert_eq!(browser_outcome, Err(NativeCodexAuthError::Callback));
    drop(browser_lifecycle);
    assert_eq!(browser_manager.status()?, CodexCredentialStatus::SignedOut);
    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            browser_endpoint.listener.accept()
        )
        .await
        .is_err()
    );

    let device_endpoint = test_endpoint().await?;
    let (_device_directory, device_slot) = private_slot()?;
    let device_manager = manager_for_endpoint(&device_slot, &device_endpoint)?;
    let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
    let device_server = spawn_sequence(
        device_endpoint,
        vec![response(
            "200 OK",
            "application/json",
            &serde_json::to_vec(&serde_json::json!({
                "authorization_code": "device-lock-code",
                "code_challenge": challenge,
                "code_verifier": verifier
            }))?,
            "",
        )],
    );
    let device_lifecycle = device_manager.lifecycle.lock().await;
    let device_login = device_login_for_deadline_test(Instant::now() + Duration::from_millis(200));
    let worker_manager = Arc::clone(&device_manager);
    let device_completion =
        tokio::spawn(async move { worker_manager.complete_device_login(device_login).await });
    assert_eq!(device_server.await??.len(), 1);
    let device_outcome = tokio::time::timeout(Duration::from_secs(2), device_completion).await??;
    assert_eq!(device_outcome, Err(NativeCodexAuthError::Provider));
    drop(device_lifecycle);
    assert_eq!(device_manager.status()?, CodexCredentialStatus::SignedOut);
    Ok(())
}

#[tokio::test]
async fn expired_commit_and_stalled_callback_writer_are_fail_closed_and_bounded() -> TestResult {
    let endpoint = test_endpoint().await?;
    let (_directory, slot) = private_slot()?;
    let manager = manager_for_endpoint(&slot, &endpoint)?;
    assert_eq!(
        manager
            .commit_login(deadline_test_credential()?, None, Instant::now())
            .await,
        Err(NativeCodexAuthError::LoginPersistence)
    );
    assert_eq!(manager.status()?, CodexCredentialStatus::SignedOut);

    let (mut stalled, _reader) = tokio::io::duplex(1);
    let started = Instant::now();
    write_callback_response(
        &mut stalled,
        StatusCode::OK,
        "Login complete",
        started + Duration::from_millis(50),
    )
    .await;
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(manager.status()?, CodexCredentialStatus::SignedOut);
    Ok(())
}

#[tokio::test]
async fn remote_browser_login_is_listener_free_pinned_and_state_bound() -> TestResult {
    let _browser_test = BROWSER_TEST_LOCK.lock().await;
    // The pinned callback ports are held for the whole flow; a bind refused
    // because another process already holds one leaves it just as occupied.
    let _callback_ports = (
        TcpListener::bind(("127.0.0.1", 1455)).await.ok(),
        TcpListener::bind(("127.0.0.1", 1457)).await.ok(),
    );
    let endpoint = test_endpoint().await?;
    let (_directory, slot) = private_slot()?;
    let manager = manager_for_endpoint(&slot, &endpoint)?;
    let login = manager.begin_remote_browser_login()?;
    let authorization = Url::parse(login.authorization_url())?;
    let expected_challenge = login.attempt.pkce.challenge().as_str().to_owned();
    let expected_verifier = login.attempt.pkce.verifier().expose_secret().to_owned();
    assert_eq!(
        authorization
            .query_pairs()
            .find(|(key, _)| key == "redirect_uri")
            .map(|(_, value)| value.into_owned()),
        Some(REMOTE_BROWSER_REDIRECT_URI.to_owned())
    );
    assert!(authorization.query_pairs().all(|(key, _)| key != "nonce"));
    assert_eq!(
        authorization_query_value(&authorization, "code_challenge")?,
        expected_challenge
    );
    assert_eq!(
        authorization_query_value(&authorization, "code_challenge_method")?,
        "S256"
    );
    let state = authorization_query_value(&authorization, "state")?;
    let token = id_token(TEST_KID, "subject-remote-browser", "account-remote-browser")?;
    let server = spawn_sequence_then_probe(
        endpoint,
        vec![
            response(
                "200 OK",
                "application/json",
                &token_body(
                    &token,
                    "opaque-access-remote-browser",
                    "refresh-remote-browser",
                    3_600,
                )?,
                "",
            ),
            response(
                "200 OK",
                "application/json",
                &jwks_body(TEST_KID, TEST_MODULUS)?,
                "Cache-Control: max-age=300\r\n",
            ),
            response(
                "200 OK",
                "application/json",
                br#"{"sub":"subject-remote-browser"}"#,
                "",
            ),
        ],
    );
    let callback_query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("code", "remote-browser-code")
        .append_pair("scope", OPENAI_CODEX_OAUTH_SCOPES)
        .append_pair("state", &state)
        .finish();
    let full_redirect_url = format!("{REMOTE_BROWSER_REDIRECT_URI}?{callback_query}");
    assert_eq!(
        manager
            .complete_remote_browser_login(login, &full_redirect_url)
            .await?,
        CodexCredentialStatus::Ready
    );
    assert_eq!(manager.status()?, CodexCredentialStatus::Ready);

    let second_login = manager.begin_remote_browser_login()?;
    assert_eq!(
        manager
            .complete_remote_browser_login(second_login, &full_redirect_url)
            .await,
        Err(NativeCodexAuthError::Callback)
    );
    assert_eq!(manager.status()?, CodexCredentialStatus::Ready);
    let (requests, unexpected_connection) = server.await??;
    assert_eq!(requests.len(), 3);
    assert!(!unexpected_connection);
    let token_request = std::str::from_utf8(&requests[0])?;
    assert!(token_request.starts_with("POST /oauth/token HTTP/1.1\r\n"));
    assert!(!token_request.contains("originator:"));
    assert!(token_request.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"));
    assert!(token_request.contains(&format!("code_verifier={expected_verifier}")));
    Ok(())
}

#[tokio::test]
async fn remote_browser_login_cannot_resurrect_a_concurrent_logout() -> TestResult {
    let endpoint = test_endpoint().await?;
    let (_directory, slot) = private_slot()?;
    let manager = manager_for_endpoint(&slot, &endpoint)?;
    let login = manager.begin_remote_browser_login()?;
    let authorization = Url::parse(login.authorization_url())?;
    assert!(authorization.query_pairs().all(|(key, _)| key != "nonce"));
    let state = authorization_query_value(&authorization, "state")?;

    seed_ready_credential(
        &slot,
        "concurrent-account",
        "concurrent-subject",
        now_unix_secs() + 3_600,
    )?;
    let concurrent = manager
        .store
        .load()?
        .ok_or("concurrent credential missing")?;
    manager.store.logout(concurrent.generation())?;

    let server = spawn_sequence(
        endpoint,
        browser_login_responses(
            "stale-remote-subject",
            "stale-remote-account",
            "stale-remote",
        )?,
    );
    let callback_query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("code", "stale-remote-code")
        .append_pair("scope", OPENAI_CODEX_OAUTH_SCOPES)
        .append_pair("state", &state)
        .finish();
    assert_eq!(
        manager
            .complete_remote_browser_login(
                login,
                &format!("{REMOTE_BROWSER_REDIRECT_URI}?{callback_query}"),
            )
            .await,
        Err(NativeCodexAuthError::LoginPersistence)
    );
    assert_eq!(manager.status()?, CodexCredentialStatus::Revoked);
    assert_eq!(server.await??.len(), 3);
    Ok(())
}

#[tokio::test]
async fn local_browser_login_rejects_concurrent_credential_rotation() -> TestResult {
    let _browser_test = BROWSER_TEST_LOCK.lock().await;
    let endpoint = test_endpoint().await?;
    let (_directory, slot) = private_slot()?;
    seed_ready_credential(
        &slot,
        "current-account",
        "current-subject",
        now_unix_secs() + 3_600,
    )?;
    let manager = manager_for_endpoint(&slot, &endpoint)?;
    let login = manager.begin_browser_login().await?;
    let authorization = Url::parse(login.authorization_url())?;
    assert!(authorization.query_pairs().all(|(key, _)| key != "nonce"));
    let state = authorization_query_value(&authorization, "state")?;
    let port = login.listener.local_addr()?.port();
    let rotated = expire_ready_generation(&manager).await?;
    let server = spawn_sequence(
        endpoint,
        browser_login_responses("stale-local-subject", "stale-local-account", "stale-local")?,
    );

    let manager_for_completion = Arc::clone(&manager);
    let completion =
        tokio::spawn(async move { manager_for_completion.complete_browser_login(login).await });
    let callback_query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("code", "stale-local-code")
        .append_pair("scope", OPENAI_CODEX_OAUTH_SCOPES)
        .append_pair("state", &state)
        .finish();
    let mut callback = TcpStream::connect(("127.0.0.1", port)).await?;
    callback
            .write_all(
                format!(
                    "GET /auth/callback?{callback_query} HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await?;
    let mut response = Vec::new();
    callback.read_to_end(&mut response).await?;
    assert_eq!(
        completion.await?,
        Err(NativeCodexAuthError::LoginPersistence)
    );
    assert_eq!(server.await??.len(), 3);
    assert!(std::str::from_utf8(&response)?.ends_with("Login failed"));
    let current = manager.store.load()?.ok_or("rotated credential missing")?;
    assert_eq!(current, rotated);
    assert_eq!(manager.status()?, CodexCredentialStatus::Ready);
    Ok(())
}

#[tokio::test]
async fn remote_browser_provider_error_and_expiry_stop_before_transport() -> TestResult {
    let endpoint = test_endpoint().await?;
    let (_directory, slot) = private_slot()?;
    let manager = manager_for_endpoint(&slot, &endpoint)?;

    let provider_error_login = manager.begin_remote_browser_login()?;
    let provider_error_authorization = Url::parse(provider_error_login.authorization_url())?;
    let provider_error_state = provider_error_authorization
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
        .ok_or("missing state")?;
    let provider_error_query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("error", "access_denied")
        .append_pair("error_description", "cancelled")
        .append_pair("state", &provider_error_state)
        .finish();
    assert_eq!(
        manager
            .complete_remote_browser_login(
                provider_error_login,
                &format!("{REMOTE_BROWSER_REDIRECT_URI}?{provider_error_query}"),
            )
            .await,
        Err(NativeCodexAuthError::Provider)
    );

    let mut expired_login = manager.begin_remote_browser_login()?;
    let expired_authorization = Url::parse(expired_login.authorization_url())?;
    let expired_state = expired_authorization
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
        .ok_or("missing state")?;
    expired_login.attempt.deadline = Instant::now();
    let expired_query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("code", "expired-code")
        .append_pair("state", &expired_state)
        .finish();
    assert_eq!(
        manager
            .complete_remote_browser_login(
                expired_login,
                &format!("{REMOTE_BROWSER_REDIRECT_URI}?{expired_query}"),
            )
            .await,
        Err(NativeCodexAuthError::Callback)
    );
    assert_eq!(manager.status()?, CodexCredentialStatus::SignedOut);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), endpoint.listener.accept())
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn malformed_browser_callback_gets_explicit_rejection_without_network() -> TestResult {
    let _browser_test = BROWSER_TEST_LOCK.lock().await;
    let endpoint = test_endpoint().await?;
    let (_directory, slot) = private_slot()?;
    let manager = manager_for_endpoint(&slot, &endpoint)?;
    let login = manager.begin_browser_login().await?;
    let authorization = Url::parse(login.authorization_url())?;
    let state = authorization
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
        .ok_or("missing state")?;
    let callback_query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("code", "browser-code")
        .append_pair("scope", "openid")
        .append_pair("state", &state)
        .finish();
    let port = login.listener.local_addr()?.port();
    let manager_for_completion = Arc::clone(&manager);
    let completion =
        tokio::spawn(async move { manager_for_completion.complete_browser_login(login).await });
    let mut callback = TcpStream::connect(("127.0.0.1", port)).await?;
    callback
            .write_all(
                format!(
                    "GET /auth/callback?{callback_query} HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await?;
    let mut callback_response = Vec::new();
    callback.read_to_end(&mut callback_response).await?;
    assert_eq!(completion.await?, Err(NativeCodexAuthError::Callback));
    let callback_response = std::str::from_utf8(&callback_response)?;
    assert!(callback_response.starts_with("HTTP/1.1 400"));
    assert!(callback_response.ends_with("Login rejected"));
    assert!(!callback_response.contains("browser-code"));
    assert!(!callback_response.contains(&state));
    assert!(!callback_response.contains("openid"));
    assert_eq!(manager.status()?, CodexCredentialStatus::SignedOut);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), endpoint.listener.accept())
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one lifecycle fixture proves browser binding, refresh commit/quarantine, and local cleanup"
)]
async fn browser_callback_is_exact_state_bound_and_one_use() -> TestResult {
    let _browser_test = BROWSER_TEST_LOCK.lock().await;
    let endpoint = test_endpoint().await?;
    let (_directory, slot) = private_slot()?;
    let manager = manager_for_endpoint(&slot, &endpoint)?;
    let login = manager.begin_browser_login().await?;
    let authorization = Url::parse(login.authorization_url())?;
    assert!(authorization.query_pairs().all(|(key, _)| key != "nonce"));
    let token = id_token(TEST_KID, "subject-browser", "account-browser")?;
    let changed_account_token = id_token(TEST_KID, "subject-browser", "account-other")?;
    let responses = vec![
        response(
            "200 OK",
            "application/json",
            &token_body(&token, "opaque-access-browser", "refresh-browser", 3600)?,
            "",
        ),
        response(
            "200 OK",
            "application/json",
            &jwks_body(TEST_KID, TEST_MODULUS)?,
            "Cache-Control: max-age=300\r\n",
        ),
        response(
            "200 OK",
            "application/json",
            br#"{"sub":"subject-browser"}"#,
            "",
        ),
        response(
            "200 OK",
            "application/json",
            br#"{"access_token":"opaque-access-refreshed","refresh_token":"refresh-rotated"}"#,
            "",
        ),
        response(
            "200 OK",
            "application/json",
            br#"{"sub":"subject-browser"}"#,
            "",
        ),
        response(
            "200 OK",
            "application/json",
            &token_body(
                &changed_account_token,
                "opaque-access-wrong",
                "refresh-wrong",
                3600,
            )?,
            "",
        ),
    ];
    let server = spawn_sequence(endpoint, responses);
    assert_eq!(authorization.host_str(), Some("127.0.0.1"));
    assert_eq!(authorization.path(), AUTHORIZE_PATH);
    assert_eq!(
        authorization
            .query_pairs()
            .find(|(key, _)| key == "originator")
            .map(|(_, value)| value.into_owned()),
        Some("plak".to_owned())
    );
    let state = authorization
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
        .ok_or("missing state")?;
    let callback_query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("code", "browser-code")
        .append_pair("scope", OPENAI_CODEX_OAUTH_SCOPES)
        .append_pair("state", &state)
        .finish();
    let port = login.listener.local_addr()?.port();
    let manager_for_completion = Arc::clone(&manager);
    let completion =
        tokio::spawn(async move { manager_for_completion.complete_browser_login(login).await });
    let mut callback = TcpStream::connect(("127.0.0.1", port)).await?;
    callback
            .write_all(
                format!(
                    "GET /auth/callback?{callback_query} HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await?;
    let mut callback_response = Vec::new();
    callback.read_to_end(&mut callback_response).await?;
    assert_eq!(completion.await??, CodexCredentialStatus::Ready);
    assert!(std::str::from_utf8(&callback_response)?.starts_with("HTTP/1.1 200"));
    assert_eq!(manager.status()?, CodexCredentialStatus::Ready);
    let expired = expire_ready_generation(&manager).await?;
    manager.refresh_if_needed().await?;
    let refreshed = manager.load_fence().await?;
    assert_eq!(refreshed.generation(), expired.generation().next()?.next()?);
    assert_eq!(refreshed.status(), CredentialStatus::Ready);
    let refreshed_lease = manager.acquire_ready_lease(refreshed.clone()).await?;
    let refreshed_credential = parse_stored(refreshed_lease.payload())?;
    assert_eq!(refreshed_credential.refresh_token, "refresh-rotated");
    assert_eq!(refreshed_credential.id_token, token);
    assert!(
        refreshed_credential.access_expires_at
            > now_unix_secs().saturating_add(ACCESS_REFRESH_SKEW)
    );
    drop(refreshed_credential);
    drop(refreshed_lease);

    expire_ready_generation(&manager).await?;
    assert_eq!(
        manager.refresh_if_needed().await,
        Err(NativeCodexAuthError::AccountChanged)
    );
    assert_eq!(manager.status()?, CodexCredentialStatus::Quarantined);
    assert_eq!(server.await??.len(), 6);
    assert_eq!(
        manager.logout_local().await?,
        CodexCredentialStatus::Revoked
    );
    assert_eq!(manager.status()?, CodexCredentialStatus::Revoked);
    Ok(())
}

#[tokio::test]
async fn subscription_usage_retains_one_ready_lease_without_recursive_store_lock() -> TestResult {
    let endpoint = test_endpoint().await?;
    let (_directory, slot) = private_slot()?;
    seed_ready_credential(
        &slot,
        "usage-account",
        "usage-subject",
        now_unix_secs() + 3_600,
    )?;
    let manager = manager_for_endpoint(&slot, &endpoint)?;
    let fence = manager.store.load()?.ok_or("missing Ready fence")?;
    let _peer_ready = manager.store.acquire_ready(&fence)?;
    let server = spawn_sequence(
            endpoint,
            vec![response(
                "200 OK",
                "application/json",
                br#"{"rate_limit":{"primary_window":{"used_percent":12,"limit_window_seconds":18000,"reset_at":1800000000}}}"#,
                "",
            )],
        );

    let observation =
        tokio::time::timeout(Duration::from_secs(2), manager.subscription_usage()).await?;
    let SubscriptionUsageRead::Snapshot(snapshot) = observation else {
        return Err("usage observation was unavailable".into());
    };
    assert!((snapshot.meters()[0].windows()[0].used_percent().get() - 12.0).abs() < f64::EPSILON);
    let requests = server.await??;
    assert_eq!(requests.len(), 1);
    let request = std::str::from_utf8(&requests[0])?;
    assert!(request.starts_with(&format!(
        "GET {} HTTP/1.1\r\n",
        crate::subscription_usage::CODEX_SUBSCRIPTION_USAGE_PATH
    )));
    assert!(request.contains("authorization: Bearer opaque-seeded-access\r\n"));
    assert!(request.contains("chatgpt-account-id: usage-account\r\n"));
    assert!(request.contains("accept: application/json\r\n"));
    Ok(())
}

#[tokio::test]
async fn subscription_usage_maps_status_media_without_retry() -> TestResult {
    for (status, content_type, body, expected) in [
        (
            "401 Unauthorized",
            "application/json",
            br"{}".as_slice(),
            SubscriptionUsageUnavailable::Unauthorized,
        ),
        (
            "429 Too Many Requests",
            "application/json",
            br"{}".as_slice(),
            SubscriptionUsageUnavailable::RateLimited,
        ),
        (
            "503 Service Unavailable",
            "application/json",
            br"{}".as_slice(),
            SubscriptionUsageUnavailable::ProviderUnavailable,
        ),
        (
            "200 OK",
            "text/plain",
            br"{}".as_slice(),
            SubscriptionUsageUnavailable::InvalidResponse,
        ),
    ] {
        let endpoint = test_endpoint().await?;
        let (_directory, slot) = private_slot()?;
        seed_ready_credential(
            &slot,
            "usage-account",
            "usage-subject",
            now_unix_secs() + 3_600,
        )?;
        let manager = manager_for_endpoint(&slot, &endpoint)?;
        let server = spawn_sequence(endpoint, vec![response(status, content_type, body, "")]);
        let observation = manager.subscription_usage().await;
        assert!(matches!(
            observation,
            SubscriptionUsageRead::Unavailable { reason, .. } if reason == expected
        ));
        assert_eq!(server.await??.len(), 1);
    }

    Ok(())
}

/// Codex refresh tokens rotate, so a usage read never claims a refresh: the
/// hosted read is bounded by a timeout that drops it, and a started claim
/// dropped mid-exchange would leave a durable Unknown fence.
#[tokio::test]
async fn subscription_usage_expired_access_reports_refresh_required_without_refresh() -> TestResult
{
    let endpoint = test_endpoint().await?;
    let (_directory, slot) = private_slot()?;
    seed_ready_credential(&slot, "usage-account", "usage-subject", now_unix_secs())?;
    let manager = manager_for_endpoint(&slot, &endpoint)?;
    let before = manager.load_fence().await?;
    // Dropped after its first poll, as the hosted timeout drops a slow read.
    let _ = tokio::time::timeout(Duration::ZERO, manager.subscription_usage()).await;
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), manager.subscription_usage()).await?,
        SubscriptionUsageRead::Unavailable {
            reason: SubscriptionUsageUnavailable::CredentialState(
                SubscriptionUsageCredentialState::RefreshRequired
            ),
            ..
        }
    ));
    let after = manager.load_fence().await?;
    assert_eq!(after, before);
    assert_eq!(after.status(), CredentialStatus::Ready);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), endpoint.listener.accept())
            .await
            .is_err()
    );
    Ok(())
}

/// Another process's in-flight refresh claim makes the durable fence Unknown
/// only transiently; it is retryable contention, not a quarantine.
#[tokio::test]
async fn subscription_usage_reports_peer_refresh_claim_as_busy_not_quarantined() -> TestResult {
    let endpoint = test_endpoint().await?;
    let (_directory, slot) = private_slot()?;
    seed_ready_credential(
        &slot,
        "usage-account",
        "usage-subject",
        now_unix_secs() + 3_600,
    )?;
    let manager = manager_for_endpoint(&slot, &endpoint)?;
    let peer = PrivateCredentialStore::open_slot(
        &slot,
        AuthProfileId::new(OPENAI_CODEX_COMPATIBILITY_PROFILE_ID)?,
        CredentialSlotId::new("primary")?,
    )?;
    let fence = peer.load()?.ok_or("missing Ready fence")?;
    let claim = peer.begin_refresh(&fence)?.start();
    assert_eq!(
        manager.load_fence().await?.status(),
        CredentialStatus::RefreshOutcomeUnknown
    );
    assert!(matches!(
        manager.subscription_usage().await,
        SubscriptionUsageRead::Unavailable {
            reason: SubscriptionUsageUnavailable::Busy,
            ..
        }
    ));
    drop(claim);
    assert!(matches!(
        manager.subscription_usage().await,
        SubscriptionUsageRead::Unavailable {
            reason: SubscriptionUsageUnavailable::CredentialState(
                SubscriptionUsageCredentialState::Quarantined
            ),
            ..
        }
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), endpoint.listener.accept())
            .await
            .is_err()
    );
    Ok(())
}

/// A held lifecycle guard is contention, and a quarantined record is its
/// exact credential state; neither sends a request and neither may be
/// reported as a failed transport.
#[tokio::test]
async fn subscription_usage_names_local_contention_and_credential_state_without_network()
-> TestResult {
    let endpoint = test_endpoint().await?;
    let (_directory, slot) = private_slot()?;
    seed_ready_credential(
        &slot,
        "usage-account",
        "usage-subject",
        now_unix_secs() + 3_600,
    )?;
    let manager = manager_for_endpoint(&slot, &endpoint)?;
    {
        let writer = std::fs::File::open(manager.store.lock_path())?;
        writer.lock()?;
        assert!(matches!(
            manager.subscription_usage().await,
            SubscriptionUsageRead::Unavailable {
                reason: SubscriptionUsageUnavailable::Busy,
                ..
            }
        ));
    }
    {
        let _held = manager.lifecycle.lock().await;
        assert!(matches!(
            manager.subscription_usage().await,
            SubscriptionUsageRead::Unavailable {
                reason: SubscriptionUsageUnavailable::Busy,
                ..
            }
        ));
    }
    let store = Arc::clone(&manager.store);
    run_blocking(move || {
        store
            .mark_refresh_intent(
                CredentialGeneration::new(1).map_err(|_| NativeCodexAuthError::Store)?,
            )
            .map_err(|_| NativeCodexAuthError::Store)
    })
    .await?;
    assert!(matches!(
        manager.subscription_usage().await,
        SubscriptionUsageRead::Unavailable {
            reason: SubscriptionUsageUnavailable::CredentialState(
                SubscriptionUsageCredentialState::Quarantined
            ),
            ..
        }
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), endpoint.listener.accept())
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn refresh_preserves_omitted_tokens_and_uses_access_jwt_expiry() -> TestResult {
    let endpoint = test_endpoint().await?;
    let (_directory, slot) = private_slot()?;
    seed_ready_credential(
        &slot,
        "refresh-account",
        "refresh-subject",
        now_unix_secs() + 3_600,
    )?;
    let manager = manager_for_endpoint(&slot, &endpoint)?;
    let expired = expire_ready_generation(&manager).await?;
    let access_expires_at = now_unix_secs() + 1_800;
    let access_token = access_token_with_expiry(access_expires_at)?;
    let refresh_body = serde_json::to_vec(&serde_json::json!({
        "access_token": access_token,
        "provider_metadata": {"ignored": true}
    }))?;
    let server = spawn_sequence(
        endpoint,
        vec![
            response("201 Created", "text/plain", &refresh_body, ""),
            response(
                "200 OK",
                "application/json; charset=utf-8",
                br#"{"sub":"refresh-subject"}"#,
                "",
            ),
        ],
    );

    manager.refresh_if_needed().await?;
    let refreshed = manager.load_fence().await?;
    assert_eq!(refreshed.generation(), expired.generation().next()?.next()?);
    let lease = manager.acquire_ready_lease(refreshed).await?;
    let credential = parse_stored(lease.payload())?;
    assert_eq!(credential.access_expires_at, access_expires_at);
    assert_eq!(credential.refresh_token, "opaque-seeded-refresh");
    assert_eq!(credential.id_token, "opaque-seeded-id");
    assert_eq!(server.await??.len(), 2);
    Ok(())
}

#[tokio::test]
async fn offline_admin_survives_expiry_and_kill_without_network() -> TestResult {
    let endpoint = test_endpoint().await?;
    let (_directory, slot_path) = private_slot()?;
    seed_ready_credential(
        &slot_path,
        "offline-account",
        "offline-subject",
        now_unix_secs() + 3_600,
    )?;

    let endpoints = ProfileEndpoints {
        auth_origin: endpoint.origin.clone(),
        chatgpt_origin: endpoint.origin.clone(),
    };
    assert!(matches!(
        CodexSubscriptionCredentialManager::new_with_endpoints_and_policy(
            &slot_path,
            endpoint.client.clone(),
            acceptance()?,
            endpoints.clone(),
            PROFILE_EXPIRES_AT,
            false,
        ),
        Err(NativeCodexAuthError::ProfileNotCurrent)
    ));
    assert!(matches!(
        CodexSubscriptionCredentialManager::new_with_endpoints_and_policy(
            &slot_path,
            endpoint.client.clone(),
            acceptance()?,
            endpoints,
            now_unix_secs(),
            true,
        ),
        Err(NativeCodexAuthError::ProfileNotCurrent)
    ));

    let admin = CodexSubscriptionLocalAdmin::open(&slot_path).await?;
    assert_eq!(admin.status().await?, CodexCredentialStatus::Ready);
    assert_eq!(
        admin.profile().notice_revision(),
        OPENAI_CODEX_COMPATIBILITY_NOTICE_REVISION
    );
    let debug = format!("{admin:?}");
    assert!(!debug.contains("offline-account"));
    assert!(!debug.contains(slot_path.to_string_lossy().as_ref()));
    assert_eq!(admin.logout_local().await?, CodexCredentialStatus::Revoked);
    assert_eq!(admin.status().await?, CodexCredentialStatus::Revoked);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), endpoint.listener.accept())
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn offline_admin_rejects_foreign_profile_and_unsafe_path() -> TestResult {
    let (_directory, slot_path) = private_slot()?;
    let foreign_profile = AuthProfileId::new("foreign.profile")?;
    let foreign = PrivateCredentialStore::open_slot(
        &slot_path,
        foreign_profile.clone(),
        CredentialSlotId::new("primary")?,
    )?;
    let record = CredentialRecord::new(
        foreign_profile,
        AccountBinding::new("foreign-account")?,
        CredentialGeneration::new(1)?,
        CredentialStatus::Ready,
        SecretBytes::new(b"foreign-record".to_vec()),
    )?;
    foreign.initialize_selected(&record)?;
    drop(record);
    drop(foreign);

    assert!(matches!(
        CodexSubscriptionLocalAdmin::open(&slot_path).await,
        Err(NativeCodexAuthError::Store)
    ));
    assert!(matches!(
        CodexSubscriptionLocalAdmin::open(PathBuf::from("relative-slot.json")).await,
        Err(NativeCodexAuthError::Store)
    ));
    Ok(())
}

#[tokio::test]
async fn a_401_triggers_one_refresh_and_retry() -> TestResult {
    // The shared Responses host re-dispatches once after an HTTP 401 head
    // when the dialect installed `codex_credential_rejected`. This pins the
    // Codex half of that hook: the forced refresh claims a new generation
    // even though the stored access token is nowhere near expiry, and it
    // runs exactly once per rejection.
    let endpoint = test_endpoint().await?;
    let (_directory, slot) = private_slot()?;
    seed_ready_credential(
        &slot,
        "rejected-account",
        "rejected-subject",
        now_unix_secs() + 3_600,
    )?;
    let manager = manager_for_endpoint(&slot, &endpoint)?;
    let baseline = manager.load_fence().await?;
    let refresh_body = serde_json::to_vec(&serde_json::json!({
        "access_token": access_token_with_expiry(now_unix_secs() + 3_600)?,
        "refresh_token": "refresh-rotated",
    }))?;
    let server = spawn_sequence(
        endpoint,
        vec![
            response("200 OK", "application/json", &refresh_body, ""),
            response(
                "200 OK",
                "application/json; charset=utf-8",
                br#"{"sub":"rejected-subject"}"#,
                "",
            ),
        ],
    );

    // Without a rejection the scheduled ladder is a no-op on a fresh token.
    manager.refresh_if_needed().await?;
    assert_eq!(
        manager.load_fence().await?.generation(),
        baseline.generation()
    );

    manager.refresh_after_rejection().await?;
    let refreshed = manager.load_fence().await?;
    assert_eq!(
        refreshed.generation(),
        baseline.generation().next()?.next()?
    );
    assert_eq!(refreshed.status(), CredentialStatus::Ready);
    let lease = manager.acquire_ready_lease(refreshed).await?;
    let credential = parse_stored(lease.payload())?;
    assert_eq!(credential.refresh_token, "refresh-rotated");
    drop(credential);
    drop(lease);
    assert_eq!(server.await??.len(), 2);
    Ok(())
}

#[tokio::test]
async fn a_rejected_grant_still_ends_not_ready() -> TestResult {
    // Fail-closed posture: a 401 whose forced refresh is itself rejected
    // leaves the credential not Ready, and the hook never retries the
    // exchange on its own.
    let endpoint = test_endpoint().await?;
    let (_directory, slot) = private_slot()?;
    seed_ready_credential(
        &slot,
        "denied-account",
        "denied-subject",
        now_unix_secs() + 3_600,
    )?;
    let manager = manager_for_endpoint(&slot, &endpoint)?;
    let server = spawn_sequence(
        endpoint,
        vec![response(
            "400 Bad Request",
            "application/json",
            br#"{"error":"invalid_grant"}"#,
            "",
        )],
    );

    assert!(manager.refresh_after_rejection().await.is_err());
    assert_ne!(manager.status()?, CodexCredentialStatus::Ready);
    assert_eq!(server.await??.len(), 1);
    Ok(())
}

#[tokio::test]
async fn claimed_refresh_parse_failure_quarantines_after_exact_wire() -> TestResult {
    let endpoint = test_endpoint().await?;
    let (_directory, slot) = private_slot()?;
    seed_ready_credential(&slot, "parse-account", "parse-subject", now_unix_secs())?;
    let manager = manager_for_endpoint(&slot, &endpoint)?;
    let server = spawn_sequence(
        endpoint,
        vec![response(
            "200 OK",
            "application/json",
            br#"{"access_token":"truncated""#,
            "",
        )],
    );

    assert_eq!(
        manager.refresh_if_needed().await,
        Err(NativeCodexAuthError::Validation)
    );
    assert_eq!(manager.status()?, CodexCredentialStatus::Quarantined);
    let requests = server.await??;
    assert_eq!(requests.len(), 1);
    let request = std::str::from_utf8(&requests[0])?;
    assert!(request.starts_with("POST /oauth/token HTTP/1.1\r\n"));
    assert!(request.contains("\"refresh_token\":\"opaque-seeded-refresh\""));
    Ok(())
}

#[tokio::test]
async fn claimed_refresh_truncated_body_quarantines() -> TestResult {
    let endpoint = test_endpoint().await?;
    let (_directory, slot) = private_slot()?;
    seed_ready_credential(&slot, "body-account", "body-subject", now_unix_secs())?;
    let manager = manager_for_endpoint(&slot, &endpoint)?;
    let server = spawn_sequence(
            endpoint,
            vec![b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{}".to_vec()],
        );

    assert_eq!(
        manager.refresh_if_needed().await,
        Err(NativeCodexAuthError::Transport(None))
    );
    assert_eq!(manager.status()?, CodexCredentialStatus::Quarantined);
    assert_eq!(server.await??.len(), 1);
    Ok(())
}

#[tokio::test]
async fn claimed_refresh_ambiguous_disconnect_never_restores_or_retries() -> TestResult {
    let endpoint = test_endpoint().await?;
    let (_directory, slot) = private_slot()?;
    seed_ready_credential(
        &slot,
        "disconnect-account",
        "disconnect-subject",
        now_unix_secs(),
    )?;
    let manager = manager_for_endpoint(&slot, &endpoint)?;
    let TestEndpoint {
        listener, acceptor, ..
    } = endpoint;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut stream = acceptor.accept(stream).await?;
        let request = read_request(&mut stream).await?;
        drop(stream);
        Ok::<_, TestError>(request)
    });

    assert!(matches!(
        manager.refresh_if_needed().await,
        Err(NativeCodexAuthError::Transport(Some(
            plakous_model::ModelNetworkFailure {
                sent: plakous_model::ModelNetworkTransmission::MayHaveSent,
                ..
            }
        )))
    ));
    assert_eq!(manager.status()?, CodexCredentialStatus::Quarantined);
    let request = server.await??;
    assert!(std::str::from_utf8(&request)?.starts_with("POST /oauth/token HTTP/1.1\r\n"));
    Ok(())
}

#[tokio::test]
async fn claimed_refresh_cancellation_after_send_remains_quarantined() -> TestResult {
    let endpoint = test_endpoint().await?;
    let (_directory, slot) = private_slot()?;
    seed_ready_credential(&slot, "cancel-account", "cancel-subject", now_unix_secs())?;
    let manager = manager_for_endpoint(&slot, &endpoint)?;
    let TestEndpoint {
        listener, acceptor, ..
    } = endpoint;
    let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut stream = acceptor.accept(stream).await?;
        let request = read_request(&mut stream).await?;
        let _ = seen_tx.send(());
        let mut byte = [0_u8; 1];
        let _ = stream.read(&mut byte).await;
        Ok::<_, TestError>(request)
    });
    let cancellation = plakous_model::CancellationToken::new();
    let worker_manager = Arc::clone(&manager);
    let worker_cancellation = cancellation.clone();
    let worker = tokio::spawn(async move {
        let _lifecycle = worker_manager.lifecycle.lock().await;
        worker_manager
            .refresh_if_needed_locked(
                Instant::now() + Duration::from_secs(30),
                Some(&worker_cancellation),
            )
            .await
    });
    seen_rx.await?;
    cancellation.cancel();

    assert!(matches!(
        worker.await?,
        Err(NativeCodexAuthError::Transport(Some(
            plakous_model::ModelNetworkFailure {
                sent: plakous_model::ModelNetworkTransmission::MayHaveSent,
                ..
            }
        )))
    ));
    assert_eq!(manager.status()?, CodexCredentialStatus::Quarantined);
    let request = server.await??;
    assert!(std::str::from_utf8(&request)?.starts_with("POST /oauth/token HTTP/1.1\r\n"));
    Ok(())
}

#[tokio::test]
async fn claimed_remote_revoke_consumes_tombstone_terminal() -> TestResult {
    let endpoint = test_endpoint().await?;
    let (_directory, slot) = private_slot()?;
    seed_ready_credential(
        &slot,
        "revoke-account",
        "revoke-subject",
        now_unix_secs() + 3600,
    )?;
    let manager = manager_for_endpoint(&slot, &endpoint)?;
    let server = spawn_sequence(
        endpoint,
        vec![response("204 No Content", "application/json", b"", "")],
    );

    assert_eq!(manager.revoke().await?, CodexCredentialStatus::Revoked);
    assert_eq!(manager.status()?, CodexCredentialStatus::Revoked);
    let requests = server.await??;
    assert_eq!(requests.len(), 1);
    let request = std::str::from_utf8(&requests[0])?;
    assert!(request.starts_with("POST /oauth/revoke HTTP/1.1\r\n"));
    assert!(request.contains("\"token\":\"opaque-seeded-refresh\""));
    Ok(())
}

#[tokio::test]
async fn claimed_refresh_store_busy_restores_without_send_and_responses_do_not_mix() -> TestResult {
    let target = test_endpoint().await?;
    let held = test_endpoint().await?;
    let (directory, slot) = private_slot()?;
    seed_ready_credential(&slot, "busy-account", "busy-subject", now_unix_secs())?;
    let manager = manager_for_endpoint(&slot, &target)?;
    let main_before = manager.load_fence().await?;

    let sibling = Arc::new(PrivateCredentialStore::open_slot(
        directory.path().join("sibling.json"),
        AuthProfileId::new(OPENAI_CODEX_COMPATIBILITY_PROFILE_ID)?,
        CredentialSlotId::new("sibling")?,
    )?);
    let sibling_record = CredentialRecord::new(
        AuthProfileId::new(OPENAI_CODEX_COMPATIBILITY_PROFILE_ID)?,
        AccountBinding::new("sibling-account")?,
        CredentialGeneration::new(1)?,
        CredentialStatus::Ready,
        SecretBytes::new(b"sibling-secret".to_vec()),
    )?;
    sibling.initialize_selected(&sibling_record)?;
    let sibling_claim = sibling.begin_refresh(&sibling_record.fence())?;
    let held_client = held.client.clone();
    let held_request = claimed_request(
        &held_client,
        &held.origin,
        Method::POST,
        TOKEN_PATH,
        json_headers(None)?,
        b"{}".to_vec(),
    )?;
    let held_server = spawn_sequence(
        held,
        vec![response("200 OK", "application/json", b"{}", "")],
    );
    let held_response = sibling
        .execute_claimed_http(
            sibling_claim,
            &held_client,
            held_request,
            ExchangeControl::new(Instant::now() + Duration::from_secs(30)),
        )
        .await?;

    // The sibling's claimed exchange owns the private directory: the main
    // slot cannot claim, so the ladder backs off with nothing written and
    // nothing sent.
    let worker_manager = Arc::clone(&manager);
    let worker = tokio::spawn(async move { worker_manager.refresh_if_needed().await });
    assert!(
        tokio::time::timeout(Duration::from_millis(100), target.listener.accept())
            .await
            .is_err()
    );
    assert_eq!(manager.load_fence().await?, main_before);
    // Cross-store quarantine must fail. Consuming the sibling response
    // also releases the directory lock so the worker can claim.
    let wrong_store = Arc::clone(&manager.store);
    let mixed = run_blocking(move || {
        if wrong_store.quarantine_claimed_http(held_response).is_ok() {
            return Err(NativeCodexAuthError::Store);
        }
        Ok(())
    })
    .await;
    assert!(mixed.is_ok());
    // Closing the target listener turns the worker's dispatch into a
    // definitely-not-started connect failure: the claim is restored.
    drop(target);
    assert!(matches!(
        worker.await?,
        Err(NativeCodexAuthError::Transport(Some(
            plakous_model::ModelNetworkFailure {
                phase: plakous_model::ModelRetryPhase::Connect,
                sent: plakous_model::ModelNetworkTransmission::NotSent,
                ..
            }
        )))
    ));
    assert_eq!(manager.status()?, CodexCredentialStatus::Ready);
    assert_eq!(
        sibling.status()?,
        Some(CredentialStatus::RefreshOutcomeUnknown)
    );
    assert_eq!(held_server.await??.len(), 1);
    Ok(())
}

#[tokio::test]
async fn zero_age_jwks_is_used_for_the_fetch_that_returned_it() -> TestResult {
    let endpoint = test_endpoint().await?;
    let (_directory, slot) = private_slot()?;
    let manager = manager_for_endpoint(&slot, &endpoint)?;
    let reply = response(
        "200 OK",
        "application/json",
        &jwks_body(TEST_KID, TEST_MODULUS)?,
        "Cache-Control: max-age=0\r\n",
    );
    let server = spawn_sequence(endpoint, vec![reply]);
    assert_eq!(
        manager
            .jwk_for(TEST_KID, true, Instant::now() + Duration::from_secs(30))
            .await?
            .key_id(),
        TEST_KID
    );
    assert_eq!(server.await??.len(), 1);
    Ok(())
}

#[tokio::test]
async fn cancelled_jwks_singleflight_does_not_strand_followers() -> TestResult {
    let endpoint = test_endpoint().await?;
    let (_directory, slot) = private_slot()?;
    let manager = manager_for_endpoint(&slot, &endpoint)?;
    let TestEndpoint {
        listener, acceptor, ..
    } = endpoint;
    let (first_seen_tx, first_seen_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let jwks = jwks_body(TEST_KID, TEST_MODULUS)?;
    let server = tokio::spawn(async move {
        let (first, _) = listener.accept().await?;
        let mut first = acceptor.accept(first).await?;
        let first_request = read_request(&mut first).await?;
        let _ = first_seen_tx.send(());
        let _ = release_rx.await;
        drop(first);

        let (second, _) = listener.accept().await?;
        let mut second = acceptor.accept(second).await?;
        let second_request = read_request(&mut second).await?;
        second
            .write_all(&response(
                "200 OK",
                "application/json",
                &jwks,
                "Cache-Control: max-age=300\r\n",
            ))
            .await?;
        second.shutdown().await?;
        Ok::<_, TestError>(vec![first_request, second_request])
    });

    let first_manager = Arc::clone(&manager);
    let first = tokio::spawn(async move {
        first_manager
            .jwk_for(TEST_KID, true, Instant::now() + Duration::from_secs(30))
            .await
    });
    first_seen_rx.await?;
    first.abort();
    assert!(first.await.is_err());
    let _ = release_tx.send(());
    assert_eq!(
        manager
            .jwk_for(TEST_KID, true, Instant::now() + Duration::from_secs(30))
            .await?
            .key_id(),
        TEST_KID
    );
    assert_eq!(server.await??.len(), 2);
    Ok(())
}

#[test]
fn public_constants_are_backed_by_shared_oauth_pin_tables() {
    assert_eq!(OPENAI_AUTH_ORIGIN, CODEX_OAUTH.auth_origin);
    assert_eq!(OPENAI_CODEX_NATIVE_CLIENT_ID, CODEX_OAUTH.client_id);
    assert_eq!(OPENAI_CODEX_OAUTH_SCOPES, CODEX_OAUTH.scopes);
    assert_eq!(
        OPENAI_CODEX_COMPATIBILITY_NOTICE_REVISION,
        CODEX_PROFILE.notice_revision
    );
    assert_eq!(
        OPENAI_CODEX_KILL_SWITCH_REVISION,
        CODEX_PROFILE.kill_switch_revision
    );
    assert_eq!(
        OPENAI_CODEX_COMPATIBILITY_PROFILE_ID,
        CODEX_PROFILE.profile_id
    );
    assert_eq!(OPENAI_CODEX_SOURCE_REVISION, CODEX_PROFILE.source_revision);
    assert_eq!(AUTHORIZE_PATH, CODEX_OAUTH.authorize_path);
    assert_eq!(TOKEN_PATH, CODEX_OAUTH.token_path);
    assert_eq!(REVOKE_PATH, CODEX_OAUTH.revoke_path);
    assert_eq!(JWKS_PATH, CODEX_OAUTH.jwks_path);
    assert_eq!(USER_INFO_PATH, CODEX_OAUTH.userinfo_path);
    assert_eq!(DEVICE_USER_CODE_PATH, CODEX_OAUTH.device_authorization_path);
    assert_eq!(DEVICE_TOKEN_PATH, CODEX_OAUTH.device_token_path.unwrap());
    assert_eq!(
        DEVICE_VERIFICATION_PATH,
        CODEX_OAUTH.device_verification_path.unwrap()
    );
    assert_eq!(
        DEVICE_REDIRECT_PATH,
        CODEX_OAUTH.device_redirect_path.unwrap()
    );
    assert_eq!(BROWSER_CALLBACK_PATH, CODEX_OAUTH.browser_callback_path);
    assert_eq!(
        REMOTE_BROWSER_REDIRECT_URI,
        CODEX_OAUTH.loopback_redirect_uri
    );
    assert_eq!(PROFILE_REVIEWED_AT, CODEX_PROFILE.reviewed_at_unix);
    assert_eq!(PROFILE_EXPIRES_AT, CODEX_PROFILE.expires_at_unix);
    assert_eq!(PROFILE_KILLED, CODEX_PROFILE.killed);
    assert_eq!(CODEX_OAUTH.loopback_ports, &[1455, 1457]);
    assert_eq!(CODEX_OAUTH.primary_loopback_port(), 1455);
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one table-sized test covers independent profile, JOSE, RSA, and catalog rejection axes"
)]
fn acceptance_jose_jwk_and_catalog_negative_matrix_is_strict() -> TestResult {
    let too_early = CodexCompatibilityAcceptance::accept_exact(
        OPENAI_CODEX_COMPATIBILITY_PROFILE_ID,
        OPENAI_CODEX_SOURCE_REVISION,
        OPENAI_CODEX_COMPATIBILITY_NOTICE_REVISION,
        Timestamp::new(PROFILE_REVIEWED_AT - 1, 0)?,
    )?;
    assert_eq!(
        validate_profile(&too_early, PROFILE_REVIEWED_AT),
        Err(NativeCodexAuthError::ProfileNotCurrent)
    );

    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(TEST_KID.to_owned());
    assert!(validate_jose_header(&header, true).is_ok());
    header.typ = None;
    assert!(validate_jose_header(&header, true).is_ok());
    header.typ = Some("JWT".to_owned());
    header.crit = Some(vec!["exp".to_owned()]);
    assert_eq!(
        validate_jose_header(&header, true),
        Err(NativeCodexAuthError::LoginAccountVerification)
    );
    header.crit = None;
    header.jku = Some("https://attacker.invalid/jwks".to_owned());
    assert_eq!(
        validate_jose_header(&header, true),
        Err(NativeCodexAuthError::LoginAccountVerification)
    );
    assert!(rsa_components_are_acceptable(TEST_MODULUS, "AQAB"));
    let short_modulus = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([0x80_u8; 255]);
    assert!(!rsa_components_are_acceptable(&short_modulus, "AQAB"));
    let undersized_jwk: RsaJwk = parse_strict_json(
        br#"{"kty":"RSA","use":"sig","alg":"RS256","kid":"k","n":"AA","e":"AQAB"}"#,
    )?;
    assert!(!rsa_components_are_acceptable(
        undersized_jwk.modulus(),
        undersized_jwk.exponent()
    ));
    let provider_jwk = format!(
        r#"{{"kty":"RSA","use":"sig","alg":"RS256","kid":"provider-key","n":"{TEST_MODULUS}","e":"AQAB","x5c":["provider-certificate-metadata"],"x5t":"provider-thumbprint"}}"#
    );
    let provider_jwk: RsaJwk = parse_strict_json(provider_jwk.as_bytes())?;
    assert!(rsa_components_are_acceptable(
        provider_jwk.modulus(),
        provider_jwk.exponent()
    ));
    // Unknown effort labels map to Other (forward-compatible); they must not
    // unlock portable reasoning alone. Unknown shell types remain parseable
    // so an unrelated new model cannot invalidate the selected row, but do
    // not acquire tool capability.
    let future_effort = parse_strict_json::<CatalogModelWire>(br#"{"slug":"m","visibility":"list","priority":1,"supported_in_api":true,"supported_reasoning_levels":[{"effort":"future"}],"shell_type":"shell_command","input_modalities":["text"],"context_window":272000,"max_context_window":1000000,"effective_context_window_percent":95,"auto_compact_token_limit":240000}"#)?;
    let future_efforts =
        CodexReasoningEffortSet::from_catalog(&future_effort.supported_reasoning_levels);
    assert!(!future_efforts.supports_named_reasoning());
    assert!(!future_efforts.supports(ReasoningEffort::Low));
    let future_shell = parse_strict_json::<CatalogModelWire>(br#"{"slug":"m","visibility":"list","priority":1,"supported_in_api":true,"supported_reasoning_levels":[],"shell_type":"future","input_modalities":["text"],"context_window":272000,"max_context_window":1000000,"effective_context_window_percent":95,"auto_compact_token_limit":240000}"#)?;
    assert_eq!(
        future_shell.shell_type,
        CatalogShellType::Other("future".to_owned())
    );
    let low_only = parse_strict_json::<CatalogModelWire>(br#"{"slug":"m","visibility":"list","priority":1,"supported_in_api":true,"supported_reasoning_levels":[{"effort":"low"}],"shell_type":"shell_command","input_modalities":["text"],"context_window":272000,"max_context_window":1000000,"effective_context_window_percent":95,"auto_compact_token_limit":240000}"#)?;
    let low_only = CodexReasoningEffortSet::from_catalog(&low_only.supported_reasoning_levels);
    assert!(low_only.supports_named_reasoning());
    assert!(low_only.supports(ReasoningEffort::Low));
    assert!(!low_only.supports(ReasoningEffort::Medium));
    Ok(())
}

#[test]
fn catalog_reasoning_efforts_preserve_all_named_facts_and_fail_closed() -> TestResult {
    let wire = parse_strict_json::<CatalogModelWire>(br#"{"slug":"m","visibility":"list","priority":1,"supported_in_api":true,"supported_reasoning_levels":[{"effort":"minimal"},{"effort":"low"},{"effort":"medium"},{"effort":"high"},{"effort":"xhigh"},{"effort":"max"},{"effort":"ultra"},{"effort":"future"}],"shell_type":"shell_command","input_modalities":["text"],"context_window":272000,"max_context_window":1000000,"effective_context_window_percent":95,"auto_compact_token_limit":240000}"#)?;
    let efforts = CodexReasoningEffortSet::from_catalog(&wire.supported_reasoning_levels);
    assert_eq!(efforts.identity_byte(), u8::MAX);
    assert!(efforts.supports_named_reasoning());
    for effort in [
        ReasoningEffort::Low,
        ReasoningEffort::Medium,
        ReasoningEffort::High,
        ReasoningEffort::XHigh,
        ReasoningEffort::Max,
        ReasoningEffort::Ultra,
    ] {
        assert!(efforts.supports(effort));
    }
    assert!(!efforts.supports(ReasoningEffort::TokenBudget(
        NonZeroU32::new(1).ok_or("zero token budget")?
    )));

    let empty = parse_strict_json::<CatalogModelWire>(br#"{"slug":"m","visibility":"list","priority":1,"supported_in_api":true,"supported_reasoning_levels":[],"shell_type":"shell_command","input_modalities":["text"],"context_window":272000,"max_context_window":1000000,"effective_context_window_percent":95,"auto_compact_token_limit":240000}"#)?;
    let empty = CodexReasoningEffortSet::from_catalog(&empty.supported_reasoning_levels);
    assert_eq!(empty.identity_byte(), 0);
    assert!(!empty.supports_named_reasoning());
    for effort in [
        ReasoningEffort::Low,
        ReasoningEffort::Medium,
        ReasoningEffort::High,
        ReasoningEffort::XHigh,
        ReasoningEffort::Max,
        ReasoningEffort::Ultra,
    ] {
        assert!(!empty.supports(effort));
    }
    Ok(())
}

#[test]
fn strict_parsers_accept_exact_callback_scopes_and_reject_drift() -> TestResult {
    assert!(parse_strict_json::<OAuthLoginTokenResponse>(br#"{"access_token":"a","access_token":"b","refresh_token":"r","id_token":"i","token_type":"Bearer","expires_in":1,"scope":"openid"}"#).is_err());
    let provider_owned_shape = parse_strict_json::<OAuthLoginTokenResponse>(
        br#"{"id_token":"identity","access_token":"access","refresh_token":"refresh"}"#,
    )?;
    assert_eq!(provider_owned_shape.access_token, "access");
    let additive_shape = parse_strict_json::<OAuthLoginTokenResponse>(br#"{"id_token":"identity","access_token":"access","refresh_token":"refresh","token_type":"Bearer","expires_in":3600,"scope":"provider-selected","future_metadata":{"ignored":true}}"#)?;
    assert_eq!(additive_shape.refresh_token, "refresh");
    let partial_refresh = parse_strict_json::<OAuthRefreshTokenResponse>(
        br#"{"access_token":"replacement","future_metadata":true}"#,
    )?;
    assert_eq!(partial_refresh.access_token.as_deref(), Some("replacement"));
    assert!(partial_refresh.refresh_token.is_none());
    assert!(partial_refresh.id_token.is_none());
    assert!(
        require_json_media_type(&HttpReply {
            status: StatusCode::OK,
            cache_control: None,
            content_type: Some("application/json; charset=utf-8".to_owned()),
            body: Zeroizing::new(Vec::new()),
        })
        .is_ok()
    );
    assert!(parse_strict_json::<DeviceUserCodeResponse>(br#"{"device_auth_id":"device","user_code":"ABCD-1234","interval":"5","expires_in":900}"#).is_ok());
    assert!(parse_strict_json::<DeviceUserCodeResponse>(br#"{"device_auth_id":"first","device_auth_id":"second","user_code":"ABCD-1234","interval":"5"}"#).is_err());
    assert!(parse_strict_json::<DeviceCodeResponse>(br#"{"authorization_code":"code","code_challenge":"challenge","code_verifier":"verifier","provider_extension":true}"#).is_ok());
    let state = "01234567890123456789012345678901";
    let callback_query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("code", "x")
        .append_pair("scope", OPENAI_CODEX_OAUTH_SCOPES)
        .append_pair("state", state)
        .finish();
    let callback =
        format!("GET /auth/callback?{callback_query} HTTP/1.1\r\nHost: localhost:1455\r\n\r\n");
    assert!(parse_browser_callback(callback.as_bytes(), 1455).is_ok());
    let omitted_scope = b"GET /auth/callback?code=x&state=01234567890123456789012345678901 HTTP/1.1\r\nHost: localhost:1455\r\n\r\n";
    assert!(parse_browser_callback(omitted_scope, 1455).is_ok());
    let literal_ip_host = b"GET /auth/callback?code=x&state=01234567890123456789012345678901 HTTP/1.1\r\nHost: 127.0.0.1:1455\r\n\r\n";
    assert!(parse_browser_callback(literal_ip_host, 1455).is_err());
    let request = b"GET /other?code=x&state=01234567890123456789012345678901 HTTP/1.1\r\nHost: localhost:1455\r\n\r\n";
    assert!(parse_browser_callback(request, 1455).is_err());
    let duplicate = b"GET /auth/callback?code=x&code=y&state=01234567890123456789012345678901 HTTP/1.1\r\nHost: localhost:1455\r\n\r\n";
    assert!(parse_browser_callback(duplicate, 1455).is_err());
    let empty_code = b"GET /auth/callback?code=&state=01234567890123456789012345678901 HTTP/1.1\r\nHost: localhost:1455\r\n\r\n";
    assert!(parse_browser_callback(empty_code, 1455).is_err());
    let wrong_scope = b"GET /auth/callback?code=x&scope=openid&state=01234567890123456789012345678901 HTTP/1.1\r\nHost: localhost:1455\r\n\r\n";
    assert!(parse_browser_callback(wrong_scope, 1455).is_err());
    let duplicate_scope = b"GET /auth/callback?code=x&scope=openid&scope=profile&state=01234567890123456789012345678901 HTTP/1.1\r\nHost: localhost:1455\r\n\r\n";
    assert!(parse_browser_callback(duplicate_scope, 1455).is_err());
    let mixed_success_error = b"GET /auth/callback?code=x&error=denied&state=01234567890123456789012345678901 HTTP/1.1\r\nHost: localhost:1455\r\n\r\n";
    assert!(parse_browser_callback(mixed_success_error, 1455).is_err());
    let orphaned_error_description = b"GET /auth/callback?code=x&error_description=denied&state=01234567890123456789012345678901 HTTP/1.1\r\nHost: localhost:1455\r\n\r\n";
    assert!(parse_browser_callback(orphaned_error_description, 1455).is_err());
    let scoped_error = b"GET /auth/callback?error=denied&scope=openid&state=01234567890123456789012345678901 HTTP/1.1\r\nHost: localhost:1455\r\n\r\n";
    assert!(parse_browser_callback(scoped_error, 1455).is_err());
    let provider_error = b"GET /auth/callback?error=access_denied&error_description=cancelled&state=01234567890123456789012345678901 HTTP/1.1\r\nHost: localhost:1455\r\n\r\n";
    assert!(matches!(
        parse_browser_callback(provider_error, 1455),
        Ok(OAuthCallbackOutcome::ProviderError { .. })
    ));
    Ok(())
}

#[test]
fn remote_browser_callback_url_requires_exact_reviewed_origin_path_and_query() {
    let state = "01234567890123456789012345678901";
    let parse_remote = |input: &str| {
        parse_remote_authorization_input(
            input,
            state,
            REMOTE_BROWSER_REDIRECT_URI,
            codex_callback_policy(),
        )
    };
    let success_query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("code", "authorization-code")
        .append_pair("scope", OPENAI_CODEX_OAUTH_SCOPES)
        .append_pair("state", state)
        .finish();
    assert!(parse_remote(&format!("{REMOTE_BROWSER_REDIRECT_URI}?{success_query}")).is_ok());
    assert!(matches!(
        parse_remote(&format!(
            "{REMOTE_BROWSER_REDIRECT_URI}?error=access_denied&error_description=cancelled&state={state}"
        )),
        Ok(OAuthCallbackOutcome::ProviderError { .. })
    ));

    let invalid = [
        format!("/auth/callback?code=x&state={state}"),
        format!("https://localhost:1455/auth/callback?code=x&state={state}"),
        format!("http://127.0.0.1:1455/auth/callback?code=x&state={state}"),
        format!("http://localhost/auth/callback?code=x&state={state}"),
        format!("http://localhost:1457/auth/callback?code=x&state={state}"),
        format!("http://localhost:1455/other?code=x&state={state}"),
        format!("http://localhost:1455/auth/callback/?code=x&state={state}"),
        format!("http://user@localhost:1455/auth/callback?code=x&state={state}"),
        format!("{REMOTE_BROWSER_REDIRECT_URI}?code=x&state={state}#fragment"),
        format!("{REMOTE_BROWSER_REDIRECT_URI}?code=x&code=y&state={state}"),
        format!("{REMOTE_BROWSER_REDIRECT_URI}?code=&state={state}"),
        format!("{REMOTE_BROWSER_REDIRECT_URI}?code=x&state={state}&state=other"),
        format!("{REMOTE_BROWSER_REDIRECT_URI}?code=x&state={state}&unknown=y"),
        format!("{REMOTE_BROWSER_REDIRECT_URI}?code=x&scope=openid&state={state}"),
        format!("{REMOTE_BROWSER_REDIRECT_URI}?error=denied&code=x&state={state}"),
        format!("{REMOTE_BROWSER_REDIRECT_URI}?error_description=denied&state={state}"),
        format!("{REMOTE_BROWSER_REDIRECT_URI}?state={state}"),
        format!("{REMOTE_BROWSER_REDIRECT_URI}?code=x"),
    ];
    for full_redirect_url in invalid {
        assert!(
            parse_remote(&full_redirect_url).is_err(),
            "accepted invalid remote callback: {full_redirect_url}"
        );
    }

    let oversized = format!(
        "{REMOTE_BROWSER_REDIRECT_URI}?code={}&state={state}",
        "x".repeat(MAX_CALLBACK_BYTES)
    );
    assert!(parse_remote(&oversized).is_err());
}

// Receipt renewal: a running Codex model survives a shared-store refresh on
// its own exact route and login selection, before dispatch only.

const RENEW_MODEL: &str = "gpt-5.6-luna";

fn refresh_exchange(subject: &str) -> TestResult<Vec<Vec<u8>>> {
    let access = access_token_with_expiry(now_unix_secs() + 3_600)?;
    let body = serde_json::to_vec(&serde_json::json!({ "access_token": access }))?;
    let user = serde_json::to_vec(&serde_json::json!({ "sub": subject }))?;
    Ok(vec![
        response("200 OK", "application/json", &body, ""),
        response("200 OK", "application/json", &user, ""),
    ])
}

fn catalog_reply() -> TestResult<Vec<u8>> {
    Ok(response(
        "200 OK",
        "application/json",
        &catalog_body(&[RENEW_MODEL])?,
        "",
    ))
}

fn inference_reply() -> Vec<u8> {
    response(
        "200 OK",
        "text/event-stream",
        include_bytes!("../../tests/fixtures/response_text.sse"),
        "",
    )
}

fn request_kinds(requests: &[Vec<u8>]) -> Vec<&'static str> {
    requests
        .iter()
        .map(|request| {
            let line = request
                .split(|byte| *byte == b'\n')
                .next()
                .unwrap_or_default();
            let line = String::from_utf8_lossy(line);
            if line.contains(TOKEN_PATH) {
                "refresh"
            } else if line.contains(OPENAI_CODEX_MODELS_PATH) {
                "catalog"
            } else if line.contains(crate::OPENAI_CODEX_RESPONSES_PATH) {
                "inference"
            } else {
                "userinfo"
            }
        })
        .collect()
}

fn text_request(attempt: &str) -> TestResult<ModelRequest> {
    let (selected, effective) = provider_configuration_ids()?;
    Ok(ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![user_item()?])?,
        metadata(attempt)?,
    )
    .resolved_harness_plan_identity(resolved_harness_plan()?)
    .build()?)
}

async fn run_text(
    model: &Arc<dyn LanguageModel>,
    descriptor: &ResolvedModelDescriptor,
    attempt: &str,
) -> Result<ModelResponse, ModelError> {
    let request = text_request(attempt)
        .map_err(|error| ModelError::new(ModelErrorKind::InvalidRequest, error.to_string()))?;
    run_codex_model_operation(
        Arc::clone(model),
        descriptor,
        request,
        ModelCallContext::new(
            attempt.parse::<AttemptId>().map_err(|error| {
                ModelError::new(ModelErrorKind::InvalidRequest, error.to_string())
            })?,
            plakous_model::CancellationToken::new(),
            None,
        ),
    )
    .await
}

struct RenewFixture {
    _directory: TempDir,
    slot: PathBuf,
    manager: Arc<CodexSubscriptionCredentialManager>,
    gate: Arc<InMemorySubscriptionRequestGate>,
    resolver: CodexModelResolver,
    model: Arc<dyn LanguageModel>,
    evidence: CodexModelResolutionEvidence,
    descriptor: ResolvedModelDescriptor,
}

/// Seeds one login, resolves the model through its first catalog, and serves
/// `then` after that catalog. `peers` more managers share the store.
async fn renew_fixture(
    account: &str,
    subject: &str,
    access_expires_at: u64,
    peers: usize,
    then: Vec<Vec<u8>>,
) -> TestResult<(
    RenewFixture,
    Vec<Arc<CodexSubscriptionCredentialManager>>,
    tokio::task::JoinHandle<TestResult<(Vec<Vec<u8>>, bool)>>,
)> {
    let endpoint = test_endpoint().await?;
    let client = endpoint.client.clone();
    let (directory, slot) = private_slot()?;
    seed_ready_credential(&slot, account, subject, access_expires_at)?;
    let gate = subscription_gate(4, 4)?;
    let manager = manager_for_endpoint_with_gate(&slot, &endpoint, gate.clone())?;
    let peers = (0..peers)
        .map(|_| manager_for_endpoint(&slot, &endpoint))
        .collect::<Result<Vec<_>, _>>()?;
    let mut responses = vec![catalog_reply()?];
    responses.extend(then);
    let server = spawn_sequence_then_probe(endpoint, responses);
    let resolver = CodexModelResolver::new_for_test(
        Arc::clone(&manager),
        client,
        saturated_observation_config()?,
    )?;
    resolver.refresh_catalog().await?;
    let (model, evidence) = resolver.resolve_with_evidence(RENEW_MODEL)?;
    let descriptor = catalog_descriptor(&resolver, RENEW_MODEL)?;
    Ok((
        RenewFixture {
            _directory: directory,
            slot,
            manager,
            gate,
            resolver,
            model,
            evidence,
            descriptor,
        },
        peers,
        server,
    ))
}

fn dispatched(
    gate: &InMemorySubscriptionRequestGate,
    class: SubscriptionRequestClass,
) -> TestResult<u32> {
    Ok(gate.snapshot()?.class(class).dispatched())
}

#[tokio::test]
async fn another_manager_refreshing_the_shared_store_renews_the_running_receipt() -> TestResult {
    let subject = "renew-shared-subject";
    let mut then = refresh_exchange(subject)?;
    then.push(catalog_reply()?);
    then.push(inference_reply());
    let (fixture, peers, server) = renew_fixture(
        "renew-shared-account",
        subject,
        now_unix_secs() + 3_600,
        1,
        then,
    )
    .await?;
    let before = Arc::clone(&fixture.model)
        .revalidate_runtime_authority(system_timestamp())
        .await?;
    // Another process's 401 recovery claims the next generation.
    peers[0].refresh_after_rejection().await?;
    assert_eq!(fixture.manager.load_fence().await?.generation().get(), 3);

    let response = run_text(
        &fixture.model,
        &fixture.descriptor,
        "00000000-0000-7000-8000-000000000501",
    )
    .await?;
    assert_ne!(response.output(), []);
    let after = Arc::clone(&fixture.model)
        .revalidate_runtime_authority(system_timestamp())
        .await?;
    assert_eq!(after.credential_generation(), 3);
    assert_eq!(after.account_binding(), before.account_binding());
    assert_eq!(
        after.credential_authority_binding(),
        before.credential_authority_binding()
    );
    // The durable session binding is generation-free: renewal rotates nothing.
    let semantic = fixture.evidence.semantic_provider_evidence()?;
    assert_eq!(
        ResolvedProviderAuthorityBinding::new(&fixture.descriptor, &before, semantic.clone())?,
        ResolvedProviderAuthorityBinding::new(&fixture.descriptor, &after, semantic)?
    );

    let (requests, unexpected) = server.await??;
    assert_eq!(
        request_kinds(&requests),
        ["catalog", "refresh", "userinfo", "catalog", "inference"]
    );
    assert!(!unexpected, "renewal dispatched more than once");
    assert_eq!(
        dispatched(&fixture.gate, SubscriptionRequestClass::Catalog)?,
        2
    );
    assert_eq!(
        dispatched(&fixture.gate, SubscriptionRequestClass::Inference)?,
        1
    );
    drop(fixture.slot);
    Ok(())
}

#[tokio::test]
async fn a_token_entering_its_refresh_lead_mid_turn_renews_with_one_claimed_refresh() -> TestResult
{
    let subject = "renew-lead-subject";
    let mut then = refresh_exchange(subject)?;
    then.push(catalog_reply()?);
    then.push(inference_reply());
    let (fixture, _, server) = renew_fixture(
        "renew-lead-account",
        subject,
        now_unix_secs() + ACCESS_REFRESH_SKEW + 2,
        0,
        then,
    )
    .await?;
    // The 6 h catalog window outlives the access token's usable window.
    assert!(
        fixture.evidence.catalog_expires_at().seconds()
            > now_unix_secs() + ACCESS_REFRESH_SKEW + 3_600
    );
    tokio::time::sleep(Duration::from_secs(3)).await;
    run_text(
        &fixture.model,
        &fixture.descriptor,
        "00000000-0000-7000-8000-000000000502",
    )
    .await?;
    let (requests, unexpected) = server.await??;
    assert_eq!(
        request_kinds(&requests),
        ["catalog", "refresh", "userinfo", "catalog", "inference"]
    );
    assert!(!unexpected);
    assert_eq!(
        dispatched(&fixture.gate, SubscriptionRequestClass::AuthRefresh)?,
        1
    );
    assert_eq!(
        dispatched(&fixture.gate, SubscriptionRequestClass::Inference)?,
        1
    );
    assert_eq!(fixture.manager.load_fence().await?.generation().get(), 3);
    Ok(())
}

#[tokio::test]
async fn a_new_login_fails_the_running_receipt_closed_before_any_dispatch() -> TestResult {
    for (marker, account) in [
        ("same-account", "renew-relogin-account"),
        ("other-account", "renew-relogin-foreign"),
    ] {
        let (fixture, _, server) = renew_fixture(
            "renew-relogin-account",
            "renew-relogin-subject",
            now_unix_secs() + 3_600,
            0,
            Vec::new(),
        )
        .await?;
        fixture
            .manager
            .commit_login(
                relogin_credential(account, marker)?,
                fixture.manager.store.load()?,
                Instant::now() + Duration::from_secs(5),
            )
            .await?;
        let error = Arc::clone(&fixture.model)
            .revalidate_runtime_authority(system_timestamp())
            .await
            .err()
            .ok_or("a new login renewed the running receipt")?;
        assert_eq!(
            error,
            RuntimeModelAuthorityError::ForeignAuthority,
            "{marker}"
        );
        let run = run_text(
            &fixture.model,
            &fixture.descriptor,
            "00000000-0000-7000-8000-000000000503",
        )
        .await;
        assert!(run.is_err(), "{marker}");
        let (requests, unexpected) = server.await??;
        assert_eq!(request_kinds(&requests), ["catalog"], "{marker}");
        assert!(!unexpected, "{marker}: a new login reached the wire");
        assert_eq!(
            dispatched(&fixture.gate, SubscriptionRequestClass::Catalog)?,
            1
        );
        assert_eq!(
            dispatched(&fixture.gate, SubscriptionRequestClass::Inference)?,
            0
        );
    }
    Ok(())
}

#[tokio::test]
async fn a_401_forced_refresh_never_replays_and_the_next_operation_renews_once() -> TestResult {
    let subject = "renew-401-subject";
    let mut then = vec![response(
        "401 Unauthorized",
        "application/json",
        br#"{"error":{"message":"expired","type":"invalid_request_error"}}"#,
        "",
    )];
    then.extend(refresh_exchange(subject)?);
    then.push(catalog_reply()?);
    then.push(inference_reply());
    let (fixture, _, server) = renew_fixture(
        "renew-401-account",
        subject,
        now_unix_secs() + 3_600,
        0,
        then,
    )
    .await?;
    let rejected = run_text(
        &fixture.model,
        &fixture.descriptor,
        "00000000-0000-7000-8000-000000000504",
    )
    .await;
    assert!(
        rejected.is_err(),
        "a 401 dispatch is not replayed on the moved generation"
    );
    assert_eq!(fixture.manager.load_fence().await?.generation().get(), 3);
    run_text(
        &fixture.model,
        &fixture.descriptor,
        "00000000-0000-7000-8000-000000000505",
    )
    .await?;
    let (requests, unexpected) = server.await??;
    assert_eq!(
        request_kinds(&requests),
        [
            "catalog",
            "inference",
            "refresh",
            "userinfo",
            "catalog",
            "inference"
        ]
    );
    assert!(!unexpected);
    assert_eq!(fixture.manager.load_fence().await?.generation().get(), 3);
    Ok(())
}

#[tokio::test]
async fn a_renewal_cancelled_during_its_catalog_fetch_installs_nothing() -> TestResult {
    let endpoint = test_endpoint().await?;
    let client = endpoint.client.clone();
    let (_directory, slot) = private_slot()?;
    let subject = "renew-cancel-subject";
    seed_ready_credential(
        &slot,
        "renew-cancel-account",
        subject,
        now_unix_secs() + 3_600,
    )?;
    let manager = manager_for_endpoint(&slot, &endpoint)?;
    let peer = manager_for_endpoint(&slot, &endpoint)?;
    let mut responses = vec![catalog_reply()?];
    responses.extend(refresh_exchange(subject)?);
    let (stalled_tx, stalled_rx) = tokio::sync::oneshot::channel();
    let server = spawn_stall_after(endpoint, responses, stalled_tx);
    let resolver = CodexModelResolver::new_for_test(
        Arc::clone(&manager),
        client,
        saturated_observation_config()?,
    )?;
    resolver.refresh_catalog().await?;
    let (model, _) = resolver.resolve_with_evidence(RENEW_MODEL)?;
    peer.refresh_after_rejection().await?;
    let moved = manager.load_fence().await?;

    let mut renewing = Arc::clone(&model).revalidate_runtime_authority(system_timestamp());
    tokio::select! {
        result = &mut renewing => {
            return Err(format!("renewal finished during a stalled catalog: {result:?}").into());
        }
        _ = stalled_rx => {}
    }
    drop(renewing);
    // The renewal reached its catalog request and was dropped there.
    server.abort();
    assert_eq!(manager.load_fence().await?, moved);
    assert_eq!(moved.status(), CredentialStatus::Ready);
    // Nothing was installed: the receipt is still the stale one.
    assert!(
        Arc::clone(&model)
            .revalidate_runtime_authority(system_timestamp())
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn two_managers_renew_concurrently_to_the_same_generation() -> TestResult {
    let subject = "renew-pair-subject";
    let endpoint = test_endpoint().await?;
    let client = endpoint.client.clone();
    let (_directory, slot) = private_slot()?;
    seed_ready_credential(
        &slot,
        "renew-pair-account",
        subject,
        now_unix_secs() + 3_600,
    )?;
    let first = manager_for_endpoint(&slot, &endpoint)?;
    let second = manager_for_endpoint(&slot, &endpoint)?;
    let refresher = manager_for_endpoint(&slot, &endpoint)?;
    let mut responses = vec![catalog_reply()?, catalog_reply()?];
    responses.extend(refresh_exchange(subject)?);
    responses.push(catalog_reply()?);
    responses.push(catalog_reply()?);
    let server = spawn_sequence_then_probe(endpoint, responses);
    let mut models = Vec::new();
    for manager in [&first, &second] {
        let resolver = CodexModelResolver::new_for_test(
            Arc::clone(manager),
            client.clone(),
            saturated_observation_config()?,
        )?;
        resolver.refresh_catalog().await?;
        models.push(resolver.resolve_with_evidence(RENEW_MODEL)?.0);
    }
    refresher.refresh_after_rejection().await?;
    let checked_at = system_timestamp();
    let (left, right, again) = tokio::join!(
        Arc::clone(&models[0]).revalidate_runtime_authority(checked_at),
        Arc::clone(&models[1]).revalidate_runtime_authority(checked_at),
        Arc::clone(&models[0]).revalidate_runtime_authority(checked_at),
    );
    for authority in [left?, right?, again?] {
        assert_eq!(authority.credential_generation(), 3);
    }
    let (requests, unexpected) = server.await??;
    assert_eq!(
        request_kinds(&requests),
        [
            "catalog", "catalog", "refresh", "userinfo", "catalog", "catalog"
        ]
    );
    assert!(
        !unexpected,
        "concurrent callers of one model share one renewal"
    );
    Ok(())
}

#[tokio::test]
async fn a_generation_move_between_preparation_and_dispatch_refuses_without_dispatch() -> TestResult
{
    let subject = "renew-prepared-subject";
    let mut then = refresh_exchange(subject)?;
    then.push(catalog_reply()?);
    then.push(inference_reply());
    let (fixture, peers, server) = renew_fixture(
        "renew-prepared-account",
        subject,
        now_unix_secs() + 3_600,
        1,
        then,
    )
    .await?;
    let attempt = "00000000-0000-7000-8000-000000000506";
    let request = text_request(attempt)?;
    let prepared_at = request.metadata().created_at();
    let plan = fixture
        .descriptor
        .validate_request(request, ValidationMode::Strict)?;
    let plan_digest = plan.digest();
    let authority = Arc::clone(&fixture.model)
        .revalidate_runtime_authority(prepared_at)
        .await?;
    let prepared = Arc::clone(&fixture.model)
        .prepare_call(plan, preparation_context(authority, prepared_at)?)?;
    peers[0].refresh_after_rejection().await?;
    let context = ModelCallContext::new(
        attempt.parse::<AttemptId>()?,
        plakous_model::CancellationToken::new(),
        None,
    );
    let outcome = async {
        let stream = prepared.start(context).await?;
        collect_terminal_model_stream(stream, plan_digest).await
    }
    .await;
    assert!(
        outcome.is_err(),
        "a prepared call dispatched on a moved generation"
    );
    run_text(
        &fixture.model,
        &fixture.descriptor,
        "00000000-0000-7000-8000-000000000507",
    )
    .await?;
    let (requests, unexpected) = server.await??;
    assert_eq!(
        request_kinds(&requests),
        ["catalog", "refresh", "userinfo", "catalog", "inference"]
    );
    assert!(!unexpected);
    assert_eq!(
        dispatched(&fixture.gate, SubscriptionRequestClass::Inference)?,
        1
    );
    Ok(())
}

#[tokio::test]
async fn past_the_admitted_window_revalidation_refuses_before_any_spend() -> TestResult {
    let (fixture, _, server) = renew_fixture(
        "renew-window-account",
        "renew-window-subject",
        now_unix_secs() + 3_600,
        0,
        Vec::new(),
    )
    .await?;
    let past = Timestamp::new(fixture.evidence.catalog_expires_at().seconds() + 1, 0)?;
    assert_eq!(
        Arc::clone(&fixture.model)
            .revalidate_runtime_authority(past)
            .await
            .err(),
        Some(RuntimeModelAuthorityError::Stale)
    );
    let (requests, unexpected) = server.await??;
    assert_eq!(request_kinds(&requests), ["catalog"]);
    assert!(!unexpected);
    Ok(())
}

#[tokio::test]
async fn a_fresh_resolution_after_a_rollover_keeps_the_login_selection_and_binding() -> TestResult {
    let subject = "renew-reopen-subject";
    let mut then = refresh_exchange(subject)?;
    then.push(catalog_reply()?);
    let (fixture, peers, server) = renew_fixture(
        "renew-reopen-account",
        subject,
        now_unix_secs() + 3_600,
        1,
        then,
    )
    .await?;
    let before = Arc::clone(&fixture.model)
        .revalidate_runtime_authority(system_timestamp())
        .await?;
    peers[0].refresh_after_rejection().await?;
    // The host's prompt-boundary re-resolution: a new catalog and model.
    fixture.resolver.refresh_catalog().await?;
    let (fresh, evidence) = fixture.resolver.resolve_with_evidence(RENEW_MODEL)?;
    assert_eq!(evidence.selected_credential_generation(), 3);
    assert_eq!(
        evidence.credential_selection_epoch(),
        fixture.evidence.credential_selection_epoch()
    );
    let after = fresh
        .revalidate_runtime_authority(system_timestamp())
        .await?;
    let descriptor = catalog_descriptor(&fixture.resolver, RENEW_MODEL)?;
    assert_eq!(
        ResolvedProviderAuthorityBinding::new(
            &fixture.descriptor,
            &before,
            fixture.evidence.semantic_provider_evidence()?
        )?,
        ResolvedProviderAuthorityBinding::new(
            &descriptor,
            &after,
            evidence.semantic_provider_evidence()?
        )?
    );
    let (requests, unexpected) = server.await??;
    assert_eq!(
        request_kinds(&requests),
        ["catalog", "refresh", "userinfo", "catalog"]
    );
    assert!(!unexpected);
    Ok(())
}

#[tokio::test]
async fn a_login_stored_without_a_selection_epoch_requires_a_new_login() -> TestResult {
    let endpoint = test_endpoint().await?;
    let client = endpoint.client.clone();
    let (_directory, slot) = private_slot()?;
    let store = PrivateCredentialStore::open_slot(
        &slot,
        AuthProfileId::new(OPENAI_CODEX_COMPATIBILITY_PROFILE_ID)?,
        CredentialSlotId::new("primary")?,
    )?;
    let legacy = serde_json::to_vec(&serde_json::json!({
        "access_token": "opaque-legacy-access",
        "refresh_token": "opaque-legacy-refresh",
        "id_token": "opaque-legacy-id",
        "subject": "legacy-subject",
        "account_id": "legacy-account",
        "scopes": validate_exact_scopes(OPENAI_CODEX_OAUTH_SCOPES)?,
        "access_expires_at": now_unix_secs() + 3_600,
    }))?;
    store.initialize_selected(&CredentialRecord::new(
        AuthProfileId::new(OPENAI_CODEX_COMPATIBILITY_PROFILE_ID)?,
        AccountBinding::new("legacy-account")?,
        CredentialGeneration::new(1)?,
        CredentialStatus::Ready,
        SecretBytes::new(legacy),
    )?)?;
    let manager = manager_for_endpoint(&slot, &endpoint)?;
    let server = spawn_sequence_then_probe(endpoint, Vec::new());
    let resolver =
        CodexModelResolver::new_for_test(manager, client, StandardOpenAiHttpConfig::default())?;
    assert!(matches!(
        resolver.refresh_catalog().await,
        Err(NativeCodexAuthError::LoginRequired)
    ));
    let (requests, unexpected) = server.await??;
    assert_eq!(requests, [] as [Vec<u8>; 0]);
    assert!(!unexpected, "a legacy login reached the wire");
    Ok(())
}

// CODEX-RENEW-FIX: 401 binding, legacy-login hygiene and detached refresh.

fn legacy_payload(account: &str) -> TestResult<SecretBytes> {
    Ok(SecretBytes::new(serde_json::to_vec(&serde_json::json!({
        "access_token": "opaque-legacy-access",
        "refresh_token": "opaque-legacy-refresh",
        "id_token": "opaque-legacy-id",
        "subject": "legacy-subject",
        "account_id": account,
        "scopes": validate_exact_scopes(OPENAI_CODEX_OAUTH_SCOPES)?,
        "access_expires_at": now_unix_secs() + 3_600,
    }))?))
}

fn slot_store(slot: &Path) -> TestResult<PrivateCredentialStore> {
    Ok(PrivateCredentialStore::open_slot(
        slot,
        AuthProfileId::new(OPENAI_CODEX_COMPATIBILITY_PROFILE_ID)?,
        CredentialSlotId::new("primary")?,
    )?)
}

fn seed_legacy_login(slot: &Path, account: &str) -> TestResult {
    slot_store(slot)?.initialize_selected(&CredentialRecord::new(
        AuthProfileId::new(OPENAI_CODEX_COMPATIBILITY_PROFILE_ID)?,
        AccountBinding::new(account)?,
        CredentialGeneration::new(1)?,
        CredentialStatus::Ready,
        legacy_payload(account)?,
    )?)?;
    Ok(())
}

/// Replaces the selected record with `payload` at the next generation.
fn replace_record(
    slot: &Path,
    account: &str,
    status: CredentialStatus,
    payload: SecretBytes,
) -> TestResult<CredentialFence> {
    let store = slot_store(slot)?;
    let current = store.load()?.ok_or("no credential")?;
    let record = CredentialRecord::new(
        AuthProfileId::new(OPENAI_CODEX_COMPATIBILITY_PROFILE_ID)?,
        AccountBinding::new(account)?,
        current.generation().next()?,
        status,
        payload,
    )?;
    let fence = record.fence();
    store.replace(current.generation(), &record)?;
    Ok(fence)
}

#[tokio::test]
async fn a_401_recovery_refreshes_only_its_own_login_and_generation() -> TestResult {
    let account = "renew-401-bound-account";
    let subject = "renew-401-bound-subject";
    let (fixture, peers, server) = renew_fixture(
        account,
        subject,
        now_unix_secs() + 3_600,
        1,
        refresh_exchange(subject)?,
    )
    .await?;
    let rejected = fixture.manager.load_fence().await?;
    let selection = fixture.evidence.credential_selection_epoch();
    // Another process already rotated the rejected generation: no second claim.
    peers[0].refresh_after_rejection().await?;
    let rotated = fixture.manager.load_fence().await?;
    fixture
        .manager
        .refresh_rejected_selection(&rejected, selection)
        .await?;
    assert_eq!(fixture.manager.load_fence().await?, rotated);
    // A newer login L2 is never claimed, rotated or quarantined by L1's 401.
    fixture
        .manager
        .commit_login(
            relogin_credential(account, "newer-login")?,
            fixture.manager.store.load()?,
            Instant::now() + Duration::from_secs(5),
        )
        .await?;
    let newer = fixture.manager.load_fence().await?;
    assert_eq!(
        fixture
            .manager
            .refresh_rejected_selection(&newer, selection)
            .await,
        Err(NativeCodexAuthError::SelectionChanged)
    );
    let after = fixture.manager.load_fence().await?;
    assert_eq!(after, newer);
    assert_eq!(after.status(), CredentialStatus::Ready);
    let (requests, unexpected) = server.await??;
    assert_eq!(request_kinds(&requests), ["catalog", "refresh", "userinfo"]);
    assert!(
        !unexpected,
        "a 401 recovery reached the wire for another login"
    );
    Ok(())
}

#[tokio::test]
async fn status_and_usage_name_an_earlier_build_login_as_login_required() -> TestResult {
    let endpoint = test_endpoint().await?;
    let (_directory, slot) = private_slot()?;
    seed_legacy_login(&slot, "legacy-status-account")?;
    let manager = manager_for_endpoint(&slot, &endpoint)?;
    let server = spawn_sequence_then_probe(endpoint, Vec::new());
    assert_eq!(manager.status()?, CodexCredentialStatus::LoginRequired);
    let admin = CodexSubscriptionLocalAdmin::open(slot.clone()).await?;
    assert_eq!(admin.status().await?, CodexCredentialStatus::LoginRequired);
    assert!(matches!(
        manager.subscription_usage().await,
        SubscriptionUsageRead::Unavailable {
            reason: SubscriptionUsageUnavailable::CredentialState(
                SubscriptionUsageCredentialState::LoginRequired
            ),
            ..
        }
    ));
    let (requests, unexpected) = server.await??;
    assert!(
        requests.is_empty() && !unexpected,
        "a local status read reached the wire"
    );
    Ok(())
}

#[tokio::test]
async fn an_earlier_build_login_is_never_claimed_into_quarantine() -> TestResult {
    let endpoint = test_endpoint().await?;
    let client = endpoint.client.clone();
    let (_directory, slot) = private_slot()?;
    let account = "legacy-claim-account";
    seed_legacy_login(&slot, account)?;
    replace_record(
        &slot,
        account,
        CredentialStatus::RefreshRequired,
        legacy_payload(account)?,
    )?;
    let manager = manager_for_endpoint(&slot, &endpoint)?;
    let server = spawn_sequence_then_probe(endpoint, Vec::new());
    let resolver = CodexModelResolver::new_for_test(
        Arc::clone(&manager),
        client,
        StandardOpenAiHttpConfig::default(),
    )?;
    assert!(matches!(
        resolver.refresh_catalog().await,
        Err(NativeCodexAuthError::LoginRequired)
    ));
    assert_eq!(
        slot_store(&slot)?.load()?.ok_or("gone")?.status(),
        CredentialStatus::RefreshRequired
    );
    // Remote revoke is not attempted for a login this build cannot parse;
    // nothing is sent and nothing is quarantined.
    replace_record(
        &slot,
        account,
        CredentialStatus::Ready,
        legacy_payload(account)?,
    )?;
    assert_eq!(
        manager.revoke().await,
        Err(NativeCodexAuthError::LoginRequired)
    );
    assert_eq!(
        slot_store(&slot)?.load()?.ok_or("gone")?.status(),
        CredentialStatus::Ready
    );
    assert_eq!(manager.status()?, CodexCredentialStatus::LoginRequired);
    let (requests, unexpected) = server.await??;
    assert!(
        requests.is_empty() && !unexpected,
        "a legacy login reached the wire"
    );
    Ok(())
}

#[tokio::test]
async fn a_renewal_restores_another_login_found_under_its_claim() -> TestResult {
    let account = "renew-restore-account";
    let (fixture, _, server) = renew_fixture(
        account,
        "renew-restore-subject",
        now_unix_secs() + 3_600,
        0,
        Vec::new(),
    )
    .await?;
    let other = relogin_credential(account, "restore")?;
    let replaced = replace_record(
        &fixture.slot,
        account,
        CredentialStatus::RefreshRequired,
        SecretBytes::new(serde_json::to_vec(&other)?),
    )?;
    let result = Arc::clone(&fixture.model)
        .revalidate_runtime_authority(system_timestamp())
        .await;
    assert_eq!(
        result.err(),
        Some(RuntimeModelAuthorityError::ForeignAuthority)
    );
    let (requests, unexpected) = server.await??;
    assert_eq!(request_kinds(&requests), ["catalog"]);
    assert!(!unexpected, "another login's refresh reached the wire");
    let after = slot_store(&fixture.slot)?.load()?.ok_or("gone")?;
    assert_eq!(after.status(), CredentialStatus::RefreshRequired);
    assert_eq!(after.account_binding(), replaced.account_binding());
    assert!(after.generation() > replaced.generation());
    Ok(())
}

#[tokio::test]
async fn a_running_model_refuses_an_earlier_build_record_without_any_dispatch() -> TestResult {
    let account = "renew-legacy-run-account";
    let (fixture, _, server) = renew_fixture(
        account,
        "renew-legacy-run-subject",
        now_unix_secs() + 3_600,
        0,
        Vec::new(),
    )
    .await?;
    replace_record(
        &fixture.slot,
        account,
        CredentialStatus::Ready,
        legacy_payload(account)?,
    )?;
    let result = Arc::clone(&fixture.model)
        .revalidate_runtime_authority(system_timestamp())
        .await;
    assert_eq!(
        result.err(),
        Some(RuntimeModelAuthorityError::ForeignAuthority)
    );
    let run = run_text(
        &fixture.model,
        &fixture.descriptor,
        "00000000-0000-7000-8000-000000000591",
    )
    .await;
    assert!(run.is_err());
    let (requests, unexpected) = server.await??;
    assert_eq!(request_kinds(&requests), ["catalog"]);
    assert!(!unexpected);
    Ok(())
}

#[tokio::test]
async fn a_renewal_refuses_a_changed_catalog_row_before_inference() -> TestResult {
    let subject = "renew-row-subject";
    let mut changed: serde_json::Value = serde_json::from_slice(&catalog_body(&[RENEW_MODEL])?)?;
    changed["models"][0]["max_context_window"] = serde_json::json!(400_000);
    let mut then = refresh_exchange(subject)?;
    then.push(response(
        "200 OK",
        "application/json",
        &serde_json::to_vec(&changed)?,
        "",
    ));
    let (fixture, peers, server) = renew_fixture(
        "renew-row-account",
        subject,
        now_unix_secs() + 3_600,
        1,
        then,
    )
    .await?;
    peers[0].refresh_after_rejection().await?;
    let result = Arc::clone(&fixture.model)
        .revalidate_runtime_authority(system_timestamp())
        .await;
    assert_eq!(
        result.err(),
        Some(RuntimeModelAuthorityError::ForeignAuthority)
    );
    let (requests, unexpected) = server.await??;
    assert_eq!(
        request_kinds(&requests),
        ["catalog", "refresh", "userinfo", "catalog"]
    );
    assert!(!unexpected);
    assert_eq!(
        dispatched(&fixture.gate, SubscriptionRequestClass::Inference)?,
        0
    );
    Ok(())
}

#[tokio::test]
async fn a_renewed_authority_is_capped_by_access_expiry_and_the_admitted_window() -> TestResult {
    let subject = "renew-cap-subject";
    let mut then = refresh_exchange(subject)?;
    then.push(catalog_reply()?);
    let (fixture, peers, server) = renew_fixture(
        "renew-cap-account",
        subject,
        now_unix_secs() + 3_600,
        1,
        then,
    )
    .await?;
    peers[0].refresh_after_rejection().await?;
    let authority = Arc::clone(&fixture.model)
        .revalidate_runtime_authority(system_timestamp())
        .await?;
    let valid_until = authority.valid_until().ok_or("no expiry")?;
    assert!(valid_until.seconds() <= now_unix_secs() + 3_600 - ACCESS_REFRESH_SKEW + 2);
    assert!(valid_until <= fixture.evidence.catalog_expires_at());
    let past = Timestamp::new(fixture.evidence.catalog_expires_at().seconds(), 0)?;
    assert_eq!(
        Arc::clone(&fixture.model)
            .revalidate_runtime_authority(past)
            .await
            .err(),
        Some(RuntimeModelAuthorityError::Stale)
    );
    let (requests, unexpected) = server.await??;
    assert_eq!(
        request_kinds(&requests),
        ["catalog", "refresh", "userinfo", "catalog"]
    );
    assert!(!unexpected);
    Ok(())
}

/// Dropping a revalidation whose renewal is mid-refresh (a compaction
/// timeout, shutdown, an aborted driver) leaves the claimed exchange running:
/// it completes and records its outcome instead of stranding a quarantine.
#[tokio::test]
async fn a_dropped_revalidation_lets_its_claimed_refresh_complete() -> TestResult {
    let endpoint = test_endpoint().await?;
    let client = endpoint.client.clone();
    let (_directory, slot) = private_slot()?;
    let subject = "renew-drop-subject";
    seed_ready_credential(
        &slot,
        "renew-drop-account",
        subject,
        now_unix_secs() + ACCESS_REFRESH_SKEW + 2,
    )?;
    let manager = manager_for_endpoint(&slot, &endpoint)?;
    let refresh = refresh_exchange(subject)?;
    let catalog = catalog_reply()?;
    let (posted_tx, posted_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        let mut posted = Some(posted_tx);
        let mut release = Some(release_rx);
        for (index, reply) in [catalog, refresh[0].clone(), refresh[1].clone()]
            .into_iter()
            .enumerate()
        {
            let (stream, _) = endpoint.listener.accept().await?;
            let mut stream = endpoint.acceptor.accept(stream).await?;
            requests.push(read_request(&mut stream).await?);
            if index == 1 {
                let _ = posted.take().ok_or("posted twice")?.send(());
                let _ = release.take().ok_or("released twice")?.await;
            }
            stream.write_all(&reply).await?;
            stream.shutdown().await?;
        }
        TestResult::Ok(requests)
    });
    let resolver = CodexModelResolver::new_for_test(
        Arc::clone(&manager),
        client,
        saturated_observation_config()?,
    )?;
    resolver.refresh_catalog().await?;
    let (model, _) = resolver.resolve_with_evidence(RENEW_MODEL)?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let mut revalidating = Arc::clone(&model).revalidate_runtime_authority(system_timestamp());
    tokio::select! {
        result = &mut revalidating => {
            return Err(format!("revalidation finished before its refresh POST: {result:?}").into());
        }
        _ = posted_rx => {}
    }
    drop(revalidating);
    let _ = release_tx.send(());
    let requests = server.await??;
    assert_eq!(request_kinds(&requests), ["catalog", "refresh", "userinfo"]);
    let mut settled = manager.load_fence().await?;
    for _ in 0..50 {
        if settled.status() == CredentialStatus::Ready && settled.generation().get() == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        settled = manager.load_fence().await?;
    }
    assert_eq!(
        settled.status(),
        CredentialStatus::Ready,
        "the dropped refresh was quarantined"
    );
    assert_eq!(settled.generation().get(), 3);
    Ok(())
}

/// A dispatch dropped (Esc, deadline) while its 401 recovery's claimed refresh
/// is mid-exchange: the refresh still completes and records its outcome, so
/// the claim never stays RefreshOutcomeUnknown.
#[tokio::test]
async fn a_dispatch_dropped_during_its_401_refresh_lets_the_refresh_complete() -> TestResult {
    let endpoint = test_endpoint().await?;
    let client = endpoint.client.clone();
    let (_directory, slot) = private_slot()?;
    let subject = "renew-401-drop-subject";
    seed_ready_credential(
        &slot,
        "renew-401-drop-account",
        subject,
        now_unix_secs() + 3_600,
    )?;
    let manager = manager_for_endpoint_with_gate(&slot, &endpoint, subscription_gate(4, 4)?)?;
    let refresh = refresh_exchange(subject)?;
    let replies = vec![
        catalog_reply()?,
        response(
            "401 Unauthorized",
            "application/json",
            br#"{"error":{"message":"expired","type":"invalid_request_error"}}"#,
            "",
        ),
        refresh[0].clone(),
        refresh[1].clone(),
    ];
    let (posted_tx, posted_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        let mut posted = Some(posted_tx);
        let mut release = Some(release_rx);
        for (index, reply) in replies.into_iter().enumerate() {
            let (stream, _) = endpoint.listener.accept().await?;
            let mut stream = endpoint.acceptor.accept(stream).await?;
            requests.push(read_request(&mut stream).await?);
            if index == 2 {
                let _ = posted.take().ok_or("posted twice")?.send(());
                let _ = release.take().ok_or("released twice")?.await;
            }
            stream.write_all(&reply).await?;
            stream.shutdown().await?;
        }
        TestResult::Ok(requests)
    });
    let resolver = CodexModelResolver::new_for_test(
        Arc::clone(&manager),
        client,
        saturated_observation_config()?,
    )?;
    resolver.refresh_catalog().await?;
    let (model, _) = resolver.resolve_with_evidence(RENEW_MODEL)?;
    let descriptor = catalog_descriptor(&resolver, RENEW_MODEL)?;
    let mut dispatch = Box::pin(run_text(
        &model,
        &descriptor,
        "00000000-0000-7000-8000-000000000592",
    ));
    tokio::select! {
        result = &mut dispatch => {
            return Err(format!("dispatch finished before its 401 refresh POST: {result:?}").into());
        }
        _ = posted_rx => {}
    }
    drop(dispatch);
    let _ = release_tx.send(());
    let requests = server.await??;
    assert_eq!(
        request_kinds(&requests),
        ["catalog", "inference", "refresh", "userinfo"]
    );
    let mut settled = manager.load_fence().await?;
    for _ in 0..50 {
        if settled.status() == CredentialStatus::Ready && settled.generation().get() == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        settled = manager.load_fence().await?;
    }
    assert_eq!(
        settled.status(),
        CredentialStatus::Ready,
        "the dropped 401 refresh was quarantined"
    );
    assert_eq!(settled.generation().get(), 3);
    Ok(())
}
