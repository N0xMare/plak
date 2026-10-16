use std::error::Error;
use std::fs;
use std::num::NonZeroU32;
use std::os::unix::fs::PermissionsExt as _;
use std::pin::Pin;
use std::str::FromStr as _;
use std::sync::Arc;
use std::task::{Context, Poll};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use futures_util::StreamExt as _;
use jsonwebtoken::{EncodingKey, Header, encode};
use plakous_auth::subscription_limit::{
    InMemorySubscriptionRequestGate, SubscriptionLimitError, SubscriptionLimitSnapshot,
    SubscriptionRequestLimits, SubscriptionRequestPermit, SystemSubscriptionGateClock,
};
use plakous_http::{Limits, TlsConfigIdentity};
use plakous_model::{
    ActiveFamilyArtifact, Actor, AttemptId, CachePolicy, CanonicalHistory, CapabilityRequirement,
    ContentBlock, ContextEvidenceClass, ConversationItem, ConversationView, DataClassification,
    EffectiveProviderConfigurationId, EntryId, GenerationLimits, HistorySegment,
    HistorySegmentKind, ModelCacheAffinity, ModelCallContext, ModelError, ModelErrorKind,
    ModelEvent, ModelPreparationContext, ModelRequest, ModelResponse, ModelStream, OpaqueBlock,
    OpaqueFallback, PreparedCacheUse, PreparedModelCall, Provenance, ReasoningEffort,
    ReasoningPolicy, ReplayEnvelope, ReplayKindId, ReplayScope, RequestMetadata, RunId,
    SealedFamilyArtifactCandidate, SelectedProviderConfigurationId, TextBlock, ToolConcurrency,
    ToolDefinition, ToolEffect, ToolExecutionOwner, ToolInputContract, ToolName,
    ToolOutputContract, ToolResultBlock, ToolResultKind, ToolResultPart, TurnId, ValidationMode,
    WorkingProjection,
};
use plakous_responses::{RESPONSES_ARTIFACT_CODEC_REVISION, RESPONSES_ARTIFACT_SCHEMA_REVISION};
use plakous_responses_dialect::{ResponsesRequestConfig, lower_responses_request};
use rcgen::generate_simple_self_signed;
use rustls::pki_types::PrivatePkcs8KeyDer;
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use serde_json::json;
use tempfile::{TempDir, tempdir};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, Notify, mpsc, oneshot};
use tokio_rustls::TlsAcceptor;

use super::*;
use plakous_auth::oauth_workflow::parse_absolute_redirect_query;

#[test]
fn supergrok_dispatch_generation_binds_rotation_and_login_selection() {
    let baseline = supergrok_generation_fingerprint(7, "selection-a");
    assert_ne!(baseline, supergrok_generation_fingerprint(8, "selection-a"));
    assert_ne!(baseline, supergrok_generation_fingerprint(7, "selection-b"));
}

type TestError = Box<dyn Error + Send + Sync>;
type TestResult<T = ()> = Result<T, TestError>;

fn test_cache_affinity() -> TestResult<ModelCacheAffinity> {
    Ok(ModelCacheAffinity::from_runtime_entropy(
        "00000000-0000-4000-8000-000000000501".parse::<plakous_model::SessionId>()?,
        "00000000-0000-4000-8000-000000000502".parse::<plakous_model::BranchId>()?,
        0,
        [0x11; 32],
    )?)
}

#[test]
fn forked_cache_affinity_does_not_reuse_source_conv_id_identity() -> TestResult {
    let source = test_cache_affinity()?;
    let fork = ModelCacheAffinity::from_runtime_entropy(
        source.session(),
        "00000000-0000-4000-8000-000000000503".parse::<plakous_model::BranchId>()?,
        0,
        [0x22; 32],
    )?;
    let recovered =
        ModelCacheAffinity::from_runtime_entropy(source.session(), fork.branch(), 0, [0x22; 32])?;
    assert_ne!(source.identity(), fork.identity());
    assert_eq!(fork.identity(), recovered.identity());
    Ok(())
}

const TEST_KID: &str = "xai-test-key";
const PRIVATE_DER_B64: &str = "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgWTFfCGljY6aw3HrtkHmPRiazukxPLb6ilpRAewjW8nihRANCAATDskChT-Altkm9X7MI69T3IUmrQU0L950IxEzvw_x5BMEINRMrXLBJhqzO9Bm-d6JbqA21YQmd1Kt4RzLJR1W-";
const JWK_X: &str = "w7JAoU_gJbZJvV-zCOvU9yFJq0FNC_edCMRM78P8eQQ";
const JWK_Y: &str = "wQg1EytcsEmGrM70Gb53oluoDbVhCZ3Uq3hHMslHVb4";
static AUTH_TEST_GATE: Mutex<()> = Mutex::const_new(());

struct MockResponse {
    status: &'static str,
    content_type: &'static str,
    body: String,
    extra_headers: String,
    disconnect_after_request: bool,
    truncate_after: Option<usize>,
    before_head: Option<oneshot::Receiver<()>>,
    before_body: Option<oneshot::Receiver<()>>,
}

struct MockExchange {
    response: MockResponse,
    acknowledgement: oneshot::Sender<MockExchangeResult>,
}

type MockExchangeResult = Result<(), String>;
type MockAcknowledgement = oneshot::Receiver<MockExchangeResult>;

struct MockResponses {
    sender: mpsc::Sender<MockExchange>,
    pending: Arc<Mutex<Vec<MockAcknowledgement>>>,
}

impl MockResponses {
    async fn send(&self, response: MockResponse) -> TestResult {
        let (acknowledgement, receiver) = oneshot::channel();
        self.sender
            .send(MockExchange {
                response,
                acknowledgement,
            })
            .await
            .map_err(|_| std::io::Error::other("mock peer stopped before enqueue"))?;
        self.pending.lock().await.push(receiver);
        Ok(())
    }

    async fn wait(&self) -> TestResult {
        let pending = std::mem::take(&mut *self.pending.lock().await);
        for acknowledgement in pending {
            let result = tokio::time::timeout(Duration::from_secs(2), acknowledgement).await??;
            result.map_err(std::io::Error::other)?;
        }
        Ok(())
    }
}

struct MockPeer {
    origin: String,
    client: Client,
    responses: MockResponses,
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for MockPeer {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl MockPeer {
    async fn shutdown(mut self) -> TestResult {
        let exchanges = self.responses.wait().await;
        let Some(task) = self.task.take() else {
            return exchanges;
        };
        task.abort();
        let joined = tokio::time::timeout(Duration::from_secs(1), task).await?;
        if let Err(error) = joined
            && !error.is_cancelled()
        {
            return Err(error.into());
        }
        exchanges
    }

    async fn abort_join(mut self) -> TestResult {
        let Some(task) = self.task.take() else {
            return Ok(());
        };
        task.abort();
        let joined = tokio::time::timeout(Duration::from_secs(1), task).await?;
        if let Err(error) = joined
            && !error.is_cancelled()
        {
            return Err(error.into());
        }
        Ok(())
    }
}

async fn mock_peer() -> TestResult<MockPeer> {
    let certified = generate_simple_self_signed(vec!["127.0.0.1".to_owned()])?;
    let certificate = certified.cert.der().clone();
    let key = PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der());
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut server = ServerConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(vec![certificate.clone()], key.into())?;
    server.alpn_protocols = vec![b"http/1.1".to_vec()];
    let tls_identity =
        TlsConfigIdentity::from_parts(1, &[certificate.as_ref(), b"xai-test-rustls-ring-http2"])?;
    let mut roots = RootCertStore::empty();
    roots.add(certificate)?;
    let client_config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let client =
        Client::with_tls_config_and_identity(Limits::default(), client_config, tls_identity)?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let origin = format!("https://127.0.0.1:{}", listener.local_addr()?.port());
    let acceptor = TlsAcceptor::from(Arc::new(server));
    let (send, mut receive) = mpsc::channel::<MockExchange>(32);
    let pending = Arc::new(Mutex::new(Vec::new()));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&requests);
    let task = tokio::spawn(async move {
        while let Some(exchange) = receive.recv().await {
            let MockExchange {
                mut response,
                acknowledgement,
            } = exchange;
            let result = async {
                let (tcp, _) = listener.accept().await.map_err(|error| error.to_string())?;
                let mut tls = acceptor
                    .accept(tcp)
                    .await
                    .map_err(|error| error.to_string())?;
                let request = read_http_request(&mut tls)
                    .await
                    .map_err(|error| error.to_string())?;
                observed.lock().await.push(request);
                if response.disconnect_after_request {
                    drop(tls);
                    return Ok(());
                }
                if let Some(gate) = response.before_head.take() {
                    gate.await
                        .map_err(|_| "mock response-head gate was dropped".to_owned())?;
                }
                let head = format!(
                    "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n",
                    response.status,
                    response.content_type,
                    response.body.len(),
                    response.extra_headers,
                );
                tls.write_all(head.as_bytes())
                    .await
                    .map_err(|error| error.to_string())?;
                tls.flush().await.map_err(|error| error.to_string())?;
                if let Some(gate) = response.before_body.take() {
                    gate.await
                        .map_err(|_| "mock response-body gate was dropped".to_owned())?;
                }
                let body = response.body.as_bytes();
                let body = &body[..response.truncate_after.unwrap_or(body.len()).min(body.len())];
                tls.write_all(body)
                    .await
                    .map_err(|error| error.to_string())?;
                tls.flush().await.map_err(|error| error.to_string())?;
                drop(tls);
                Ok(())
            }
            .await;
            let failed = result.is_err();
            let _ = acknowledgement.send(result);
            if failed {
                return;
            }
        }
    });
    Ok(MockPeer {
        origin,
        client,
        responses: MockResponses {
            sender: send,
            pending,
        },
        requests,
        task: Some(task),
    })
}

async fn read_http_request<S>(stream: &mut S) -> Result<Vec<u8>, std::io::Error>
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut request = Vec::new();
    let mut buffer = [0u8; 2048];
    let mut expected = None;
    loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
        if expected.is_none()
            && let Some(head_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
        {
            let head = String::from_utf8_lossy(&request[..head_end + 4]);
            let content_length = head
                .lines()
                .find_map(|line| {
                    line.split_once(':').and_then(|(name, value)| {
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                })
                .unwrap_or(0);
            expected = Some(head_end + 4 + content_length);
        }
        if expected.is_some_and(|length| request.len() >= length) {
            break;
        }
    }
    Ok(request)
}

fn response(body: &serde_json::Value) -> MockResponse {
    MockResponse {
        status: "200 OK",
        content_type: "application/json; charset=utf-8",
        body: body.to_string(),
        extra_headers: String::new(),
        disconnect_after_request: false,
        truncate_after: None,
        before_head: None,
        before_body: None,
    }
}

fn raw_response(
    status: &'static str,
    content_type: &'static str,
    body: impl Into<String>,
) -> MockResponse {
    MockResponse {
        status,
        content_type,
        body: body.into(),
        extra_headers: String::new(),
        disconnect_after_request: false,
        truncate_after: None,
        before_head: None,
        before_body: None,
    }
}

fn gated_response(body: &serde_json::Value) -> (MockResponse, oneshot::Sender<()>) {
    let (release, before_head) = oneshot::channel();
    (
        MockResponse {
            status: "200 OK",
            content_type: "application/json",
            body: body.to_string(),
            extra_headers: String::new(),
            disconnect_after_request: false,
            truncate_after: None,
            before_head: Some(before_head),
            before_body: None,
        },
        release,
    )
}

fn disconnect_after_request() -> MockResponse {
    MockResponse {
        status: "200 OK",
        content_type: "application/json",
        body: String::new(),
        extra_headers: String::new(),
        disconnect_after_request: true,
        truncate_after: None,
        before_head: None,
        before_body: None,
    }
}

fn redirect(location: &str) -> MockResponse {
    MockResponse {
        status: "302 Found",
        content_type: "text/plain",
        body: String::new(),
        extra_headers: format!("Location: {location}\r\n"),
        disconnect_after_request: false,
        truncate_after: None,
        before_head: None,
        before_body: None,
    }
}

fn gated_sse(body: String) -> (MockResponse, oneshot::Sender<()>, oneshot::Sender<()>) {
    let (release_head, before_head) = oneshot::channel();
    let (release_body, before_body) = oneshot::channel();
    (
        MockResponse {
            status: "200 OK",
            content_type: "text/event-stream",
            body,
            extra_headers: String::new(),
            disconnect_after_request: false,
            truncate_after: None,
            before_head: Some(before_head),
            before_body: Some(before_body),
        },
        release_head,
        release_body,
    )
}

fn private_store(temp: &TempDir) -> TestResult<PathBuf> {
    let directory = temp.path().join("credentials");
    fs::create_dir(&directory)?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    Ok(directory.join("xai.json"))
}

fn manager(temp: &TempDir, peer: &MockPeer) -> TestResult<XaiNativeAuthManager> {
    Ok(XaiNativeAuthManager::open_test_slot(
        private_store(temp)?,
        peer.client.clone(),
        &peer.origin,
    )?)
}

/// A manager whose browser login must bind the port `occupied` already holds,
/// so any bind attempt fails and a flow that succeeds provably never bound.
fn manager_with_occupied_loopback(
    temp: &TempDir,
    peer: &MockPeer,
    occupied: &TcpListener,
) -> TestResult<XaiNativeAuthManager> {
    let mut manager = manager(temp, peer)?;
    let port: &'static [u16] = Box::leak(Box::new([occupied.local_addr()?.port()]));
    Arc::get_mut(&mut manager.core)
        .ok_or("fresh manager core is unshared")?
        .endpoints
        .loopback_ports = port;
    Ok(manager)
}

#[derive(Serialize)]
struct TestClaims<'a> {
    iss: &'a str,
    aud: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    azp: Option<&'a str>,
    sub: &'a str,
    exp: u64,
    iat: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    nonce: Option<&'a str>,
}

fn id_token(subject: &str, nonce: Option<&str>) -> TestResult<String> {
    id_token_with_exp(subject, nonce, system_unix_seconds()? + 3600)
}

fn id_token_with_exp(subject: &str, nonce: Option<&str>, exp: u64) -> TestResult<String> {
    id_token_with_azp_exp(subject, nonce, Some(XAI_PUBLIC_CLIENT_ID), exp)
}

fn id_token_with_azp(subject: &str, nonce: Option<&str>, azp: Option<&str>) -> TestResult<String> {
    id_token_with_azp_exp(subject, nonce, azp, system_unix_seconds()? + 3600)
}

fn id_token_with_azp_exp(
    subject: &str,
    nonce: Option<&str>,
    azp: Option<&str>,
    exp: u64,
) -> TestResult<String> {
    let now = system_unix_seconds()?;
    let private = STANDARD.decode(PRIVATE_DER_B64.replace('-', "+").replace('_', "/"))?;
    let mut header = Header::new(Algorithm::ES256);
    header.kid = Some(TEST_KID.to_owned());
    Ok(encode(
        &header,
        &TestClaims {
            iss: XAI_ISSUER,
            aud: XAI_PUBLIC_CLIENT_ID,
            azp,
            sub: subject,
            exp,
            iat: now,
            nonce,
        },
        &EncodingKey::from_ec_der(&private),
    )?)
}

fn jwks(kid: &str) -> serde_json::Value {
    json!({"keys":[{
        "kty":"EC", "crv":"P-256", "use":"sig", "alg":"ES256",
        "kid":kid, "x":JWK_X, "y":JWK_Y
    }]})
}

fn token_response(access: &str, refresh: &str, id: &str) -> serde_json::Value {
    json!({
        "access_token":access,
        "refresh_token":refresh,
        "id_token":id,
        "token_type":"Bearer",
        "expires_in":3600,
        "scope":XAI_OAUTH_SCOPE
    })
}

fn access_token_with_expiry(expires_at: u64) -> TestResult<String> {
    let payload = serde_json::to_vec(&json!({
        "exp": expires_at,
        "provider_metadata": "ignored"
    }))?;
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload);
    Ok(format!("e30.{payload}.provider-signature"))
}

async fn enqueue_login_validation(
    peer: &MockPeer,
    token: serde_json::Value,
    subject: &str,
) -> TestResult {
    peer.responses.send(response(&token)).await?;
    peer.responses.send(response(&jwks(TEST_KID))).await?;
    peer.responses
        .send(response(&json!({"sub":subject})))
        .await?;
    Ok(())
}

fn assert_profile_revision_rejections(accepted_at: Timestamp) {
    assert!(matches!(
        XaiCompatibilityAcceptance::accept_exact(
            "xai.grok.compat.2026-07-31.r0",
            XAI_OPENCODE_SOURCE_REVISION,
            XAI_GROK_BUILD_SOURCE_REVISION,
            XAI_GROK_COMPATIBILITY_NOTICE_REVISION,
            XAI_GROK_KILL_SWITCH_REVISION,
            accepted_at,
        ),
        Err(XaiNativeAuthError::CompatibilityNotAccepted)
    ));
    for (source, official, notice, kill) in [
        (
            "wrong-source",
            XAI_GROK_BUILD_SOURCE_REVISION,
            XAI_GROK_COMPATIBILITY_NOTICE_REVISION,
            XAI_GROK_KILL_SWITCH_REVISION,
        ),
        (
            XAI_OPENCODE_SOURCE_REVISION,
            "wrong-official-source",
            XAI_GROK_COMPATIBILITY_NOTICE_REVISION,
            XAI_GROK_KILL_SWITCH_REVISION,
        ),
        (
            XAI_OPENCODE_SOURCE_REVISION,
            XAI_GROK_BUILD_SOURCE_REVISION,
            "wrong-notice",
            XAI_GROK_KILL_SWITCH_REVISION,
        ),
        (
            XAI_OPENCODE_SOURCE_REVISION,
            XAI_GROK_BUILD_SOURCE_REVISION,
            XAI_GROK_COMPATIBILITY_NOTICE_REVISION,
            "wrong-kill",
        ),
    ] {
        assert!(matches!(
            XaiCompatibilityAcceptance::accept_exact(
                XAI_GROK_COMPATIBILITY_PROFILE,
                source,
                official,
                notice,
                kill,
                accepted_at,
            ),
            Err(XaiNativeAuthError::CompatibilityNotAccepted)
        ));
    }
}

#[test]
fn public_constants_are_backed_by_shared_oauth_pin_tables() {
    assert_eq!(XAI_PUBLIC_CLIENT_ID, XAI_OAUTH.client_id);
    assert_eq!(XAI_ISSUER, XAI_OAUTH.auth_origin);
    assert_eq!(XAI_OIDC_DISCOVERY, XAI_OAUTH.oidc_discovery_url.unwrap());
    assert_eq!(XAI_OAUTH_SCOPE, XAI_OAUTH.scopes);
    assert_eq!(XAI_LOOPBACK_REDIRECT, XAI_OAUTH.loopback_redirect_uri);
    assert_eq!(XAI_OAUTH.primary_loopback_port(), 56121);
    assert_eq!(AuthEndpoints::production().loopback_ports, &[56121]);
    assert_eq!(
        format!(
            "http://127.0.0.1:{}{}",
            XAI_OAUTH.primary_loopback_port(),
            XAI_OAUTH.browser_callback_path
        ),
        XAI_LOOPBACK_REDIRECT
    );
    assert_eq!(XAI_GROK_COMPATIBILITY_PROFILE, XAI_PROFILE.profile_id);
    assert_eq!(XAI_OPENCODE_SOURCE_REVISION, XAI_PROFILE.source_revision);
    assert_eq!(XAI_OPENCODE_SOURCE, XAI_PROFILE.source_locator.unwrap());
    assert_eq!(
        XAI_GROK_COMPATIBILITY_NOTICE_REVISION,
        XAI_PROFILE.notice_revision
    );
    assert_eq!(
        XAI_GROK_KILL_SWITCH_REVISION,
        XAI_PROFILE.kill_switch_revision
    );
    assert_eq!(COMPATIBILITY_REVIEWED_UNIX, XAI_PROFILE.reviewed_at_unix);
    assert_eq!(COMPATIBILITY_EXPIRES_UNIX, XAI_PROFILE.expires_at_unix);
    assert_eq!(COMPATIBILITY_KILLED, XAI_PROFILE.killed);
    assert_eq!(AUTHORIZATION_PATH, XAI_OAUTH.authorize_path);
    assert_eq!(TOKEN_PATH, XAI_OAUTH.token_path);
    assert_eq!(DEVICE_PATH, XAI_OAUTH.device_authorization_path);
    let endpoints = AuthEndpoints::production();
    assert_eq!(
        endpoints.catalog().origin,
        crate::SUPERGROK_RESPONSES_ORIGIN
    );
    assert_eq!(
        endpoints.catalog().path,
        crate::responses_dialect::SUPERGROK_MODELS_PATH
    );
    assert_eq!(endpoints.usage().origin, crate::SUPERGROK_RESPONSES_ORIGIN);
    assert_eq!(
        endpoints.usage().path,
        crate::subscription_usage::GROK_SUBSCRIPTION_USAGE_PATH
    );
    assert_ne!(
        endpoints.catalog().origin,
        crate::platform::XAI_PLATFORM_API_KEY_ORIGIN
    );
}

#[test]
fn profile_is_exact_expiring_and_truthfully_attributed() -> TestResult {
    let profile = XaiSubscriptionIntegrationProfile;
    assert_eq!(profile.id(), XAI_GROK_COMPATIBILITY_PROFILE);
    assert_eq!(profile.source_revision(), XAI_OPENCODE_SOURCE_REVISION);
    assert_eq!(
        profile.official_model_source_revision(),
        XAI_GROK_BUILD_SOURCE_REVISION
    );
    assert_eq!(
        profile.provenance(),
        XaiCompatibilityProvenance::PinnedCompatibility
    );
    assert_eq!(profile.reviewed_at(), COMPATIBILITY_REVIEWED_UNIX);
    assert_eq!(profile.expires_at(), COMPATIBILITY_EXPIRES_UNIX);
    assert_eq!(
        profile.notice_revision(),
        XAI_GROK_COMPATIBILITY_NOTICE_REVISION
    );
    assert_eq!(
        profile.kill_switch_revision(),
        XAI_GROK_KILL_SWITCH_REVISION
    );
    assert!(!profile.is_killed());
    let accepted_at = Timestamp::new(COMPATIBILITY_REVIEWED_UNIX, 0)?;
    let accepted = XaiCompatibilityAcceptance::accept_exact(
        XAI_GROK_COMPATIBILITY_PROFILE,
        XAI_OPENCODE_SOURCE_REVISION,
        XAI_GROK_BUILD_SOURCE_REVISION,
        XAI_GROK_COMPATIBILITY_NOTICE_REVISION,
        XAI_GROK_KILL_SWITCH_REVISION,
        accepted_at,
    )?;
    assert!(!format!("{accepted:?}").contains(XAI_OPENCODE_SOURCE_REVISION));
    assert_profile_revision_rejections(accepted_at);
    assert!(matches!(
        XaiCompatibilityAcceptance::accept_exact(
            XAI_GROK_COMPATIBILITY_PROFILE,
            XAI_OPENCODE_SOURCE_REVISION,
            XAI_GROK_BUILD_SOURCE_REVISION,
            XAI_GROK_COMPATIBILITY_NOTICE_REVISION,
            XAI_GROK_KILL_SWITCH_REVISION,
            Timestamp::new(COMPATIBILITY_REVIEWED_UNIX - 1, 0)?,
        ),
        Err(XaiNativeAuthError::CompatibilityNotAccepted)
    ));
    let pkce = PkceS256::generate()?;
    let callback = CallbackBinding::generate()?;
    let url = authorization_url(
        &AuthEndpoints::production(),
        XAI_LOOPBACK_REDIRECT,
        &pkce,
        &callback,
    )?;
    assert!(url.starts_with("https://auth.x.ai/oauth2/authorize?"));
    let parsed = Url::parse(&url)?;
    let pairs = parsed.query_pairs().collect::<BTreeMap<_, _>>();
    assert_eq!(
        pairs.get("client_id").map(AsRef::as_ref),
        Some(XAI_PUBLIC_CLIENT_ID)
    );
    assert_eq!(
        pairs.get("redirect_uri").map(AsRef::as_ref),
        Some(XAI_LOOPBACK_REDIRECT)
    );
    assert_eq!(pairs.get("scope").map(AsRef::as_ref), Some(XAI_OAUTH_SCOPE));
    assert_eq!(pairs.get("plan").map(AsRef::as_ref), Some("generic"));
    assert_eq!(pairs.get("referrer").map(AsRef::as_ref), Some("plak"));
    assert!(!url.contains("referrer=opencode"));
    assert!(!url.contains("grok-build"));
    assert_eq!(
        XaiCompatibilityProvenance::PinnedCompatibility,
        XaiCompatibilityProvenance::PinnedCompatibility
    );
    assert!(ensure_compatibility(COMPATIBILITY_EXPIRES_UNIX - 1).is_ok());
    assert!(matches!(
        ensure_compatibility(COMPATIBILITY_EXPIRES_UNIX),
        Err(XaiNativeAuthError::CompatibilityInactive)
    ));
    assert!(matches!(
        ensure_compatibility_state(COMPATIBILITY_REVIEWED_UNIX, true),
        Err(XaiNativeAuthError::CompatibilityInactive)
    ));
    Ok(())
}

#[tokio::test]
async fn subscription_usage_sends_exact_one_attempt_profile_and_retains_ready_lease() -> TestResult
{
    let peer = mock_peer().await?;
    let directory = tempdir()?;
    let manager = manager(&directory, &peer)?;
    install_stored(
        &manager,
        "usage-subject",
        "opaque-usage-access",
        "opaque-usage-refresh",
        system_unix_seconds()?.saturating_add(3_600),
    )?;
    peer.responses
        .send(raw_response(
            "200 OK",
            "application/json; charset=utf-8",
            r#"{"config":{"creditUsagePercent":12.25,"currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY","start":"2026-08-20T12:00:00Z","end":"2036-08-27T12:00:00Z"}}}"#,
        ))
        .await?;

    let observation =
        tokio::time::timeout(Duration::from_secs(2), manager.subscription_usage()).await?;
    let SubscriptionUsageRead::Snapshot(snapshot) = observation else {
        return Err("Grok usage observation was unavailable".into());
    };
    assert_eq!(snapshot.meters().len(), 1);
    assert_eq!(snapshot.meters()[0].id(), "included-credits");
    assert!((snapshot.meters()[0].windows()[0].used_percent().get() - 12.25).abs() < f64::EPSILON);
    peer.responses.wait().await?;
    let requests = peer.requests.lock().await;
    assert_eq!(requests.len(), 1);
    let request = std::str::from_utf8(&requests[0])?;
    assert!(request.starts_with(&format!(
        "GET {} HTTP/1.1\r\n",
        crate::subscription_usage::GROK_SUBSCRIPTION_USAGE_PATH
    )));
    for header in [
        "authorization: Bearer opaque-usage-access\r\n",
        "x-userid: usage-subject\r\n",
        "x-xai-token-auth: xai-grok-cli\r\n",
        "x-grok-client-version: 1.0.46\r\n",
        "x-grok-client-mode: interactive\r\n",
    ] {
        assert!(request.contains(header), "missing exact header {header:?}");
    }
    for forbidden in [
        "x-authenticateresponse:",
        "x-grok-client-identifier:",
        "content-type:",
    ] {
        assert!(
            !request.contains(forbidden),
            "usage request added foreign header {forbidden:?}"
        );
    }
    drop(requests);
    peer.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn subscription_usage_maps_status_media_schema_and_body_bound_without_retry() -> TestResult {
    for (status, content_type, body, expected) in [
        (
            "401 Unauthorized",
            "application/json",
            "{}".to_owned(),
            SubscriptionUsageUnavailable::Unauthorized,
        ),
        (
            "403 Forbidden",
            "application/json",
            "{}".to_owned(),
            SubscriptionUsageUnavailable::Unauthorized,
        ),
        (
            "429 Too Many Requests",
            "application/json",
            "{}".to_owned(),
            SubscriptionUsageUnavailable::RateLimited,
        ),
        (
            "503 Service Unavailable",
            "application/json",
            "{}".to_owned(),
            SubscriptionUsageUnavailable::ProviderUnavailable,
        ),
        (
            "200 OK",
            "text/plain",
            "{}".to_owned(),
            SubscriptionUsageUnavailable::InvalidResponse,
        ),
        (
            "200 OK",
            "application/json",
            "not-json".to_owned(),
            SubscriptionUsageUnavailable::InvalidResponse,
        ),
        (
            "200 OK",
            "application/json",
            "x".repeat(crate::subscription_usage::MAX_GROK_SUBSCRIPTION_USAGE_BODY_BYTES + 1),
            SubscriptionUsageUnavailable::InvalidResponse,
        ),
    ] {
        let peer = mock_peer().await?;
        let directory = tempdir()?;
        let manager = manager(&directory, &peer)?;
        install_stored(
            &manager,
            "usage-subject",
            "opaque-usage-access",
            "opaque-usage-refresh",
            system_unix_seconds()?.saturating_add(3_600),
        )?;
        peer.responses
            .send(raw_response(status, content_type, body))
            .await?;
        assert!(matches!(
            manager.subscription_usage().await,
            SubscriptionUsageRead::Unavailable { reason, .. } if reason == expected
        ));
        peer.responses.wait().await?;
        assert_eq!(peer.requests.lock().await.len(), 1);
        peer.shutdown().await?;
    }
    Ok(())
}

/// The hosted usage read is bounded by a timeout that drops it, so a usage
/// read never claims a refresh: an expired access token is `RefreshRequired`
/// with no network I/O and no durable write, whether the read is dropped
/// after its first poll or read to completion.
#[tokio::test]
async fn subscription_usage_expired_access_reports_refresh_required_and_never_claims_a_refresh()
-> TestResult {
    let peer = mock_peer().await?;
    let directory = tempdir()?;
    let manager = manager(&directory, &peer)?;
    install_stored(
        &manager,
        "usage-subject",
        "opaque-usage-access",
        "opaque-usage-refresh",
        system_unix_seconds()?,
    )?;
    let before = manager.core.store.load()?.ok_or("missing Ready fence")?;
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
    assert_eq!(manager.core.store.load()?, Some(before.clone()));
    assert_eq!(before.status(), CredentialStatus::Ready);
    assert!(peer.requests.lock().await.is_empty());
    peer.abort_join().await?;
    Ok(())
}

/// Another process's in-flight refresh claim makes the durable fence Unknown
/// only transiently; it is retryable contention, not a quarantine.
#[tokio::test]
async fn subscription_usage_reports_peer_refresh_claim_as_busy_not_quarantined() -> TestResult {
    let peer = mock_peer().await?;
    let directory = tempdir()?;
    let manager = manager(&directory, &peer)?;
    install_stored(
        &manager,
        "usage-subject",
        "opaque-usage-access",
        "opaque-usage-refresh",
        system_unix_seconds()?.saturating_add(3_600),
    )?;
    let peer_store = PrivateCredentialStore::open_slot(
        directory.path().join("credentials").join("xai.json"),
        auth_profile_id()?,
        CredentialSlotId::new("test-selected")?,
    )?;
    let fence = required(peer_store.load()?, "peer fence")?;
    let claim = peer_store.begin_refresh(&fence)?.start();
    assert_eq!(
        required(manager.status()?, "peer claim")?.status(),
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
    assert!(peer.requests.lock().await.is_empty());
    peer.abort_join().await?;
    Ok(())
}

/// A held refresh flight is contention, and a quarantined record is its
/// exact credential state; neither sends a request and neither may be
/// reported as a failed transport.
#[tokio::test]
async fn subscription_usage_names_local_contention_and_credential_state_without_network()
-> TestResult {
    let peer = mock_peer().await?;
    let directory = tempdir()?;
    let manager = manager(&directory, &peer)?;
    install_stored(
        &manager,
        "usage-subject",
        "opaque-usage-access",
        "opaque-usage-refresh",
        system_unix_seconds()?.saturating_add(3_600),
    )?;
    {
        let _held = manager.core.refresh_flight.lock().await;
        assert!(matches!(
            manager.subscription_usage().await,
            SubscriptionUsageRead::Unavailable {
                reason: SubscriptionUsageUnavailable::Busy,
                ..
            }
        ));
    }
    manager
        .core
        .store
        .mark_refresh_intent(CredentialGeneration::new(1)?)?;
    assert!(matches!(
        manager.subscription_usage().await,
        SubscriptionUsageRead::Unavailable {
            reason: SubscriptionUsageUnavailable::CredentialState(
                SubscriptionUsageCredentialState::Quarantined
            ),
            ..
        }
    ));
    assert!(peer.requests.lock().await.is_empty());
    peer.abort_join().await?;
    Ok(())
}

#[test]
fn oauth_objects_reject_duplicates_and_accept_provider_optional_metadata() -> TestResult {
    let duplicate = br#"{"access_token":"a","refresh_token":"r","token_type":"Bearer","expires_in":3600,"scope":"openid","future":1,"future":2}"#;
    assert!(matches!(
        decode_json::<TokenResponse>(duplicate, MAX_TOKEN_BYTES),
        Err(XaiNativeAuthError::InvalidTokenResponse(
            CredentialRefreshFailureReason::TokenShape
        ))
    ));
    let minimum = br#"{"access_token":"a","refresh_token":"r"}"#;
    let parsed = decode_json::<TokenResponse>(minimum, MAX_TOKEN_BYTES)?;
    assert!(parsed.validate().is_ok());
    assert_eq!(parsed.expires_in, None);
    let additive = br#"{"access_token":"a","refresh_token":"r","id_token":"i","token_type":"provider-owned","expires_in":7200,"scope":"provider selected","future":{"ignored":true}}"#;
    let parsed = decode_json::<TokenResponse>(additive, MAX_TOKEN_BYTES)?;
    assert!(parsed.validate().is_ok());
    assert_eq!(parsed.expires_in, Some(7_200));
    let zero_expiry = br#"{"access_token":"a","refresh_token":"r","expires_in":0}"#;
    let parsed = decode_json::<TokenResponse>(zero_expiry, MAX_TOKEN_BYTES)?;
    assert!(matches!(
        parsed.validate(),
        Err(XaiNativeAuthError::InvalidTokenResponse(
            CredentialRefreshFailureReason::ExpiresIn
        ))
    ));
    Ok(())
}

#[test]
fn browser_callback_url_parser_accepts_only_exact_success_or_provider_error_grammar() {
    assert!(matches!(
        parse_callback_url(
            "http://127.0.0.1:56121/callback?code=one%2Ftwo&state=correlation"
        ),
        Ok(BrowserCallback::AuthorizationCode { code, state })
            if code.as_str() == "one/two" && state.as_str() == "correlation"
    ));
    let scoped_success = format!(
        "http://127.0.0.1:56121/callback?code=one&state=correlation&scope={}",
        url::form_urlencoded::byte_serialize(XAI_OAUTH_SCOPE.as_bytes()).collect::<String>()
    );
    assert!(matches!(
        parse_callback_url(&scoped_success),
        Ok(BrowserCallback::AuthorizationCode { code, state })
            if code.as_str() == "one" && state.as_str() == "correlation"
    ));
    assert!(matches!(
        parse_callback_url(
            "http://127.0.0.1:56121/callback?error=access_denied&state=correlation&error_description=Owner+declined&error_uri=https%3A%2F%2Fauth.x.ai%2Ferrors%2Fdenied"
        ),
        Ok(BrowserCallback::ProviderError { state }) if state.as_str() == "correlation"
    ));

    let oversized = format!(
        "http://127.0.0.1:56121/callback?code={}&state=s",
        "x".repeat(MAX_CALLBACK_BYTES)
    );
    let invalid = [
        "https://127.0.0.1:56121/callback?code=c&state=s",
        "http://localhost:56121/callback?code=c&state=s",
        "http://127.0.0.1/callback?code=c&state=s",
        "http://127.0.0.1:56120/callback?code=c&state=s",
        "http://127.0.0.1:56121/callback/?code=c&state=s",
        "http://127.0.0.1:56121/callback/../callback?code=c&state=s",
        "http://user@127.0.0.1:56121/callback?code=c&state=s",
        "http://127.0.0.1:56121/callback?code=c&state=s#fragment",
        "/callback?code=c&state=s",
        "http://127.0.0.1:56121/callback?code=c",
        "http://127.0.0.1:56121/callback?state=s",
        "http://127.0.0.1:56121/callback?code=&state=s",
        "http://127.0.0.1:56121/callback?code=c&state=",
        "http://127.0.0.1:56121/callback?code=c&state=s&future=x",
        "http://127.0.0.1:56121/callback?code=c&code=d&state=s",
        "http://127.0.0.1:56121/callback?code=c&state=s&state=t",
        "http://127.0.0.1:56121/callback?code=c&state=s&error=access_denied",
        "http://127.0.0.1:56121/callback?error=access_denied&state=s&code=c",
        "http://127.0.0.1:56121/callback?error=access_denied",
        "http://127.0.0.1:56121/callback?error=&state=s",
        "http://127.0.0.1:56121/callback?error=access_denied&state=s&error_description=x&error_description=y",
        "http://127.0.0.1:56121/callback?error_description=x&state=s",
        "http://127.0.0.1:56121/callback?code=c&state=s&error_uri=https%3A%2F%2Fx.invalid",
        "http://127.0.0.1:56121/callback?code=c&state=s&scope=openid",
        "http://127.0.0.1:56121/callback?error=access_denied&state=s&scope=openid",
        "http://127.0.0.1:56121/callback?code=c&state=s&",
        "http://127.0.0.1:56121/callback?code=c&&state=s",
        "http://127.0.0.1:56121/callback?code=c&state",
        "http://127.0.0.1:56121/callback?code=c&state=s%0Ahidden",
    ];
    for callback in invalid {
        assert!(
            matches!(
                parse_callback_url(callback),
                Err(XaiNativeAuthError::InvalidCallback)
            ),
            "callback grammar unexpectedly accepted invalid case"
        );
    }
    assert!(matches!(
        parse_callback_url(&oversized),
        Err(XaiNativeAuthError::InvalidCallback)
    ));
}

fn parse_callback_url(full_redirect_url: &str) -> Result<BrowserCallback, XaiNativeAuthError> {
    parse_absolute_redirect_query(
        full_redirect_url,
        XAI_LOOPBACK_REDIRECT,
        xai_callback_policy(),
    )
    .map(oauth_callback_to_browser)
    .map_err(|_| XaiNativeAuthError::InvalidCallback)
}

#[test]
fn remote_authorization_input_accepts_callback_url_or_bare_code_with_attempt_state() {
    let parse_remote = |input: &str| {
        parse_remote_authorization_input(
            input,
            "attempt-state-xyz",
            XAI_LOOPBACK_REDIRECT,
            xai_callback_policy(),
        )
        .map(oauth_callback_to_browser)
        .map_err(map_oauth_workflow_error)
    };
    // Full redirect URL still preferred / accepted.
    assert!(matches!(
        parse_remote(
            "http://127.0.0.1:56121/callback?code=one%2Ftwo&state=correlation"
        ),
        Ok(BrowserCallback::AuthorizationCode { code, state })
            if code.as_str() == "one/two" && state.as_str() == "correlation"
    ));
    // Bare code + attempt state → AuthorizationCode bound to attempt.
    assert!(matches!(
        parse_remote("AbC012_-xai-remote-authorization-code"),
        Ok(BrowserCallback::AuthorizationCode { code, state })
            if code.as_str() == "AbC012_-xai-remote-authorization-code"
                && state.as_str() == "attempt-state-xyz"
    ));

    for invalid in [
        "",
        "contains a space",
        "https://accounts.x.ai/oauth2/consent?code=not-a-callback",
        "http://localhost:56121/callback?code=c&state=s",
        "http://127.0.0.1:56121/callback?code=c",
    ] {
        assert!(matches!(
            parse_remote(invalid),
            Err(XaiNativeAuthError::InvalidCallback)
        ));
    }
    assert!(matches!(
        parse_remote(&"x".repeat(2_049)),
        Err(XaiNativeAuthError::InvalidCallback)
    ));
}

#[test]
fn browser_callback_request_parser_requires_exact_http11_host_and_origin_form() {
    assert!(matches!(
        parse_callback_request(
            "GET /callback?code=c&state=s HTTP/1.1\r\nHost: 127.0.0.1:56121\r\nConnection: close\r\n\r\n",
            XAI_LOOPBACK_REDIRECT,
        ),
        Ok(BrowserCallback::AuthorizationCode { code, state })
            if code.as_str() == "c" && state.as_str() == "s"
    ));
    for request in [
        "GET /callback?code=c&state=s HTTP/1.1\r\nConnection: close\r\n\r\n",
        "GET /callback?code=c&state=s HTTP/1.1\r\nHost: localhost:56121\r\n\r\n",
        "GET /callback?code=c&state=s HTTP/1.1\r\nHost: 127.0.0.1:56121\r\nHost: 127.0.0.1:56121\r\n\r\n",
        "GET /callback?code=c&state=s HTTP/1.0\r\nHost: 127.0.0.1:56121\r\n\r\n",
        "GET  /callback?code=c&state=s HTTP/1.1\r\nHost: 127.0.0.1:56121\r\n\r\n",
        "GET http://127.0.0.1:56121/callback?code=c&state=s HTTP/1.1\r\nHost: 127.0.0.1:56121\r\n\r\n",
        "GET /callback?code=c&state=s HTTP/1.1\r\nHost: 127.0.0.1:56121\r\n folded: value\r\n\r\n",
        "GET /callback?code=c&state=s HTTP/1.1\nHost: 127.0.0.1:56121\n\n",
    ] {
        assert!(matches!(
            parse_callback_request(request, XAI_LOOPBACK_REDIRECT),
            Err(XaiNativeAuthError::InvalidCallback)
        ));
    }
}

#[test]
fn login_stage_errors_are_payload_free_and_redaction_safe() {
    for (error, expected) in [
        (
            XaiNativeAuthError::LoginTokenExchange,
            "the xAI login token exchange failed",
        ),
        (
            XaiNativeAuthError::LoginSigningKeys,
            "the xAI login signing-key verification failed",
        ),
        (
            XaiNativeAuthError::LoginAccountVerification,
            "the xAI login account verification failed",
        ),
        (
            XaiNativeAuthError::LoginPersistence,
            "the xAI login credential could not be persisted",
        ),
    ] {
        let rendered = error.to_string();
        assert_eq!(rendered, expected);
        for sentinel in [
            "secret-token",
            "https://provider.invalid/private",
            "provider-response-body",
            "account-identifier",
            "/private/credential/path",
        ] {
            assert!(!rendered.contains(sentinel));
        }
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one linear test proves the complete payload-free login-stage matrix"
)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browser_and_device_login_failures_report_only_the_narrow_stage() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;

    let browser_temp = tempdir()?;
    let browser_peer = mock_peer().await?;
    let browser_manager = manager(&browser_temp, &browser_peer)?;
    let browser = browser_manager.begin_remote_browser_login()?;
    let parsed = Url::parse(browser.authorization_url())?;
    let pairs = parsed.query_pairs().collect::<BTreeMap<_, _>>();
    let state = required(pairs.get("state"), "browser stage-test state")?;
    let nonce = required(pairs.get("nonce"), "browser stage-test nonce")?;
    let mut invalid_token = token_response(
        "secret-token",
        "secret-refresh",
        &id_token("account-identifier", Some(nonce))?,
    );
    invalid_token["expires_in"] = json!(0);
    browser_peer
        .responses
        .send(response(&invalid_token))
        .await?;
    let browser_error = browser
        .complete(&format!(
            "http://127.0.0.1:56121/callback?code=secret-code&state={state}"
        ))
        .await;
    assert!(matches!(
        browser_error,
        Err(XaiNativeAuthError::LoginTokenExchange)
    ));
    assert!(browser_manager.status()?.is_none());
    browser_peer.shutdown().await?;

    let device_temp = tempdir()?;
    let device_peer = mock_peer().await?;
    let device_manager = manager(&device_temp, &device_peer)?;
    device_peer
        .responses
        .send(response(&json!({
            "device_code":"secret-device", "user_code":"SAFE-1234",
            "verification_uri":"https://x.ai/device", "expires_in":60, "interval":1
        })))
        .await?;
    let device = device_manager.begin_device_login().await?;
    device_peer
        .responses
        .send(MockResponse {
            status: "200 OK",
            content_type: "text/plain",
            body: "provider-response-body".to_owned(),
            extra_headers: String::new(),
            disconnect_after_request: false,
            truncate_after: None,
            before_head: None,
            before_body: None,
        })
        .await?;
    assert!(matches!(
        device.complete().await,
        Err(XaiNativeAuthError::LoginTokenExchange)
    ));
    assert!(device_manager.status()?.is_none());
    device_peer.shutdown().await?;

    let signing_temp = tempdir()?;
    let signing_peer = mock_peer().await?;
    let signing_manager = manager(&signing_temp, &signing_peer)?;
    signing_peer
        .responses
        .send(response(&json!({"keys":[]})))
        .await?;
    let signing_tokens: TokenResponse = serde_json::from_value(token_response(
        "signing-access",
        "signing-refresh",
        &id_token("signing-subject", None)?,
    ))?;
    assert!(matches!(
        signing_manager
            .core
            .validate_tokens(
                signing_tokens,
                None,
                None,
                None,
                None,
                Some(Instant::now() + Duration::from_secs(2)),
            )
            .await,
        Err(XaiNativeAuthError::LoginSigningKeys)
    ));
    assert!(signing_manager.status()?.is_none());
    signing_peer.shutdown().await?;

    let account_temp = tempdir()?;
    let account_peer = mock_peer().await?;
    let account_manager = manager(&account_temp, &account_peer)?;
    account_peer
        .responses
        .send(response(&jwks(TEST_KID)))
        .await?;
    account_peer
        .responses
        .send(response(&json!({"sub":"different-account"})))
        .await?;
    let account_tokens: TokenResponse = serde_json::from_value(token_response(
        "account-access",
        "account-refresh",
        &id_token("signed-account", None)?,
    ))?;
    assert!(matches!(
        account_manager
            .core
            .validate_tokens(
                account_tokens,
                None,
                None,
                None,
                None,
                Some(Instant::now() + Duration::from_secs(2)),
            )
            .await,
        Err(XaiNativeAuthError::LoginAccountVerification)
    ));
    assert!(account_manager.status()?.is_none());
    account_peer.shutdown().await?;

    let persistence_temp = tempdir()?;
    let persistence_peer = mock_peer().await?;
    let persistence_manager = manager(&persistence_temp, &persistence_peer)?;
    install_stored(
        &persistence_manager,
        "existing-account",
        "existing-access",
        "existing-refresh",
        system_unix_seconds()? + 3_600,
    )?;
    let fence = required(
        persistence_manager.core.store.load()?,
        "persistence stage-test fence",
    )?;
    let stored = persistence_manager.core.read_stored(&fence)?;
    assert!(matches!(
        persistence_manager.core.persist_login(
            None,
            &stored,
            Some(Instant::now() + Duration::from_secs(2)),
        ),
        Err(XaiNativeAuthError::LoginPersistence)
    ));
    persistence_peer.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn device_login_validates_es256_jwks_userinfo_and_exact_wire() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let peer = mock_peer().await?;
    let manager = manager(&temp, &peer)?;
    peer.responses
        .send(response(&json!({
            "device_code":"device-secret", "user_code":"ABCD-1234",
            "verification_uri":"https://x.ai/device", "expires_in":600, "interval":1
        })))
        .await?;
    let login = manager.begin_device_login().await?;
    assert_eq!(login.user_code(), "ABCD-1234");
    assert_eq!(login.verification_uri(), "https://x.ai/device");
    enqueue_login_validation(
        &peer,
        token_response(
            "opaque-access",
            "rotating-refresh",
            &id_token("subject-device", None)?,
        ),
        "subject-device",
    )
    .await?;
    let status = tokio::time::timeout(Duration::from_secs(5), login.complete()).await??;
    assert_eq!(status.status(), CredentialStatus::Ready);
    assert_eq!(status.generation().get(), 1);
    assert_eq!(
        required(manager.status()?, "device status")?.account_binding(),
        status.account_binding()
    );

    wait_for_requests(&peer, 4).await?;
    let requests = peer.requests.lock().await;
    let device = String::from_utf8_lossy(&requests[0]).to_ascii_lowercase();
    assert!(device.starts_with("post /oauth2/device/code http/1.1"));
    assert!(device.contains("user-agent: plak/"));
    assert!(device.contains("client_id=b1a00492-073a-47ea-816f-4c329264a828"));
    assert!(!device.contains("opencode/"));
    assert!(!device.contains("grok-build"));
    let token = String::from_utf8_lossy(&requests[1]);
    assert!(token.contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"));
    assert!(token.contains("device_code=device-secret"));
    let userinfo = String::from_utf8_lossy(&requests[3]).to_ascii_lowercase();
    assert!(userinfo.starts_with("get /oauth2/userinfo http/1.1"));
    assert!(userinfo.contains("authorization: bearer opaque-access"));
    drop(requests);
    peer.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn device_pending_and_slow_down_are_bounded_before_success() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let peer = mock_peer().await?;
    let manager = manager(&temp, &peer)?;
    peer.responses
        .send(response(&json!({
            "device_code":"device-slow", "user_code":"SLOW-1234",
            "verification_uri":"https://x.ai/device", "expires_in":60, "interval":1
        })))
        .await?;
    let mut login = manager.begin_device_login().await?;
    login.interval = Duration::from_millis(1);
    login.deadline = Instant::now() + Duration::from_secs(10);
    peer.responses
        .send(MockResponse {
            status: "400 Bad Request",
            content_type: "application/json",
            body: json!({"error":"authorization_pending"}).to_string(),
            extra_headers: String::new(),
            disconnect_after_request: false,
            truncate_after: None,
            before_head: None,
            before_body: None,
        })
        .await?;
    peer.responses
        .send(MockResponse {
            status: "400 Bad Request",
            content_type: "application/json",
            body: json!({"error":"slow_down"}).to_string(),
            extra_headers: String::new(),
            disconnect_after_request: false,
            truncate_after: None,
            before_head: None,
            before_body: None,
        })
        .await?;
    enqueue_login_validation(
        &peer,
        token_response(
            "slow-access",
            "slow-refresh",
            &id_token("slow-subject", None)?,
        ),
        "slow-subject",
    )
    .await?;
    let status = tokio::time::timeout(Duration::from_secs(15), login.complete()).await??;
    assert_eq!(status.account_binding().as_str(), "slow-subject");
    wait_for_requests(&peer, 6).await?;
    let requests = peer.requests.lock().await;
    assert_eq!(
        requests
            .iter()
            .filter(|request| String::from_utf8_lossy(request).starts_with("POST /oauth2/token"))
            .count(),
        3
    );
    drop(requests);
    peer.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn device_login_rejects_missing_and_zero_expiry_without_poll_or_persistence() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    for body in [
        json!({
            "device_code":"device-missing", "user_code":"MISS-1234",
            "verification_uri":"https://x.ai/device", "interval":1
        }),
        json!({
            "device_code":"device-zero", "user_code":"ZERO-1234",
            "verification_uri":"https://x.ai/device", "expires_in":0, "interval":1
        }),
    ] {
        let temp = tempdir()?;
        let peer = mock_peer().await?;
        let manager = manager(&temp, &peer)?;
        peer.responses.send(response(&body)).await?;
        assert!(matches!(
            manager.begin_device_login().await,
            Err(XaiNativeAuthError::InvalidTokenResponse(_))
        ));
        wait_for_requests(&peer, 1).await?;
        assert_eq!(peer.requests.lock().await.len(), 1);
        assert!(manager.status()?.is_none());
        peer.shutdown().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn successful_device_poll_at_deadline_never_validates_or_persists() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let peer = mock_peer().await?;
    let manager = manager(&temp, &peer)?;
    peer.responses
        .send(response(&json!({
            "device_code":"device-deadline", "user_code":"LATE-1234",
            "verification_uri":"https://x.ai/device", "expires_in":60, "interval":1
        })))
        .await?;
    let mut login = manager.begin_device_login().await?;
    login.deadline = Instant::now();
    let successful_poll = Zeroizing::new(
        token_response(
            "late-access",
            "late-refresh",
            &id_token("late-subject", None)?,
        )
        .to_string()
        .into_bytes(),
    );
    assert!(matches!(
        login.complete_success(successful_poll).await,
        Err(XaiNativeAuthError::LoginExpired)
    ));
    assert!(manager.status()?.is_none());
    wait_for_requests(&peer, 1).await?;
    assert_eq!(peer.requests.lock().await.len(), 1);
    peer.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn device_completion_rechecks_profile_after_userinfo_before_persistence() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let peer = mock_peer().await?;
    let manager = Arc::new(manager(&temp, &peer)?);
    peer.responses
        .send(response(&json!({
            "device_code":"device-expiry", "user_code":"EXP-1234",
            "verification_uri":"https://x.ai/device", "expires_in":60, "interval":1
        })))
        .await?;
    let login = manager.begin_device_login().await?;
    peer.responses
        .send(response(&token_response(
            "expiry-access",
            "expiry-refresh",
            &id_token("expiry-subject", None)?,
        )))
        .await?;
    peer.responses.send(response(&jwks(TEST_KID))).await?;
    let (userinfo, release_userinfo) = gated_response(&json!({"sub":"expiry-subject"}));
    peer.responses.send(userinfo).await?;
    let completion = tokio::spawn(async move { login.complete().await });
    wait_until_request_count(&peer, 4).await?;
    manager
        .core
        .compatibility_expires_unix
        .store(system_unix_seconds()?, Ordering::Relaxed);
    release_userinfo
        .send(())
        .map_err(|()| std::io::Error::other("userinfo gate receiver was lost"))?;
    let result = tokio::time::timeout(Duration::from_secs(2), completion).await??;
    assert!(matches!(
        result,
        Err(XaiNativeAuthError::CompatibilityInactive)
    ));
    assert!(manager.status()?.is_none());
    peer.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browser_login_correlates_pkce_state_and_signed_nonce() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let peer = mock_peer().await?;
    let manager = manager(&temp, &peer)?;
    let login = manager.begin_browser_login().await?;
    let port = login.listener.local_addr()?.port();
    assert_ne!(port, XAI_OAUTH.primary_loopback_port());
    let redirect = format!("http://127.0.0.1:{port}/callback");
    let parsed = Url::parse(login.authorization_url())?;
    let pairs = parsed.query_pairs().collect::<BTreeMap<_, _>>();
    assert_eq!(
        pairs.get("redirect_uri").map(AsRef::as_ref),
        Some(redirect.as_str())
    );
    let state = required(pairs.get("state"), "browser state")?.to_string();
    let nonce = required(pairs.get("nonce"), "browser nonce")?.to_string();
    enqueue_login_validation(
        &peer,
        token_response(
            "browser-access",
            "browser-refresh",
            &id_token("subject-browser", Some(&nonce))?,
        ),
        "subject-browser",
    )
    .await?;
    let completion = tokio::spawn(async move { login.complete().await });
    let mut preflight = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
    preflight
        .write_all(
            format!("OPTIONS /callback HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: https://accounts.x.ai\r\nAccess-Control-Request-Method: GET\r\nAccess-Control-Request-Private-Network: true\r\nConnection: close\r\n\r\n")
            .as_bytes(),
        )
        .await?;
    let mut preflight_response = Vec::new();
    preflight.read_to_end(&mut preflight_response).await?;
    let preflight_response = String::from_utf8_lossy(&preflight_response);
    assert!(preflight_response.starts_with("HTTP/1.1 204 No Content"));
    assert!(preflight_response.contains("Access-Control-Allow-Origin: https://accounts.x.ai\r\n"));
    assert!(preflight_response.contains("Access-Control-Allow-Private-Network: true\r\n"));
    let mut callback = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
    let request = format!(
        "GET /callback?code=browser-code&state={state} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    );
    for fragment in request.as_bytes().chunks(3) {
        callback.write_all(fragment).await?;
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let mut response = Vec::new();
    callback.read_to_end(&mut response).await?;
    let response = String::from_utf8_lossy(&response);
    assert!(response.starts_with("HTTP/1.1 200 OK"));
    assert!(response.contains("Cache-Control: no-store\r\n"));
    assert!(response.contains("Pragma: no-cache\r\n"));
    let status = tokio::time::timeout(Duration::from_secs(5), completion).await???;
    assert_eq!(status.account_binding().as_str(), "subject-browser");
    wait_for_requests(&peer, 3).await?;
    let requests = peer.requests.lock().await;
    let token = String::from_utf8_lossy(&requests[0]);
    assert!(token.starts_with("POST /oauth2/token HTTP/1.1"));
    assert!(token.contains("grant_type=authorization_code"));
    assert!(token.contains(&format!(
        "redirect_uri=http%3A%2F%2F127.0.0.1%3A{port}%2Fcallback"
    )));
    assert!(token.contains("code_verifier="));
    drop(requests);
    peer.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_browser_login_never_binds_and_uses_shared_complete_lifecycle() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let occupied = TcpListener::bind(("127.0.0.1", 0)).await?;
    let temp = tempdir()?;
    let peer = mock_peer().await?;
    let manager = manager_with_occupied_loopback(&temp, &peer, &occupied)?;
    assert!(matches!(
        manager.begin_browser_login().await,
        Err(XaiNativeAuthError::LoopbackUnavailable)
    ));
    let bare = manager.begin_remote_browser_login()?;
    assert_eq!(
        format!("{bare:?}"),
        "XaiRemoteBrowserLogin(<redacted non-cloneable login>)"
    );
    // Nonsense paste (not a code, not a URL) still fails closed before exchange.
    assert!(matches!(
        bare.complete("nope").await,
        Err(XaiNativeAuthError::InvalidCallback)
    ));
    let login = manager.begin_remote_browser_login()?;
    let parsed = Url::parse(login.authorization_url())?;
    let pairs = parsed.query_pairs().collect::<BTreeMap<_, _>>();
    let state = required(pairs.get("state"), "remote browser state")?.to_string();
    let nonce = required(pairs.get("nonce"), "remote browser nonce")?.to_string();
    assert_eq!(
        pairs.get("redirect_uri").map(AsRef::as_ref),
        Some(XAI_LOOPBACK_REDIRECT)
    );
    enqueue_login_validation(
        &peer,
        token_response(
            "remote-browser-access",
            "remote-browser-refresh",
            &id_token("remote-browser-subject", Some(&nonce))?,
        ),
        "remote-browser-subject",
    )
    .await?;
    let status = login
        .complete(&format!(
            "http://127.0.0.1:56121/callback?code=remote-browser-code&state={state}"
        ))
        .await?;
    assert_eq!(status.account_binding().as_str(), "remote-browser-subject");
    assert_eq!(status.generation().get(), 1);
    assert_eq!(
        required(manager.status()?, "remote browser status")?.account_binding(),
        status.account_binding()
    );
    wait_for_requests(&peer, 3).await?;
    let requests = peer.requests.lock().await;
    let token = String::from_utf8_lossy(&requests[0]);
    assert!(token.contains("code=remote-browser-code"));
    assert!(token.contains("code_verifier="));
    drop(requests);
    drop(occupied);
    peer.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_browser_provider_error_is_correlated_and_never_dispatches() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let occupied = TcpListener::bind(("127.0.0.1", 0)).await?;
    let temp = tempdir()?;
    let peer = mock_peer().await?;
    let manager = manager_with_occupied_loopback(&temp, &peer, &occupied)?;
    let login = manager.begin_remote_browser_login()?;
    let parsed = Url::parse(login.authorization_url())?;
    let pairs = parsed.query_pairs().collect::<BTreeMap<_, _>>();
    let state = required(pairs.get("state"), "remote denial state")?;
    let result = login
        .complete(&format!(
            "http://127.0.0.1:56121/callback?error=access_denied&state={state}&error_description=Owner+declined"
        ))
        .await;
    assert!(matches!(
        result,
        Err(XaiNativeAuthError::AuthorizationDenied)
    ));
    assert!(manager.status()?.is_none());
    assert!(peer.requests.lock().await.is_empty());
    drop(occupied);
    peer.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_browser_wrong_state_is_invalid_callback_without_dispatch() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let occupied = TcpListener::bind(("127.0.0.1", 0)).await?;
    let temp = tempdir()?;
    let peer = mock_peer().await?;
    let manager = manager_with_occupied_loopback(&temp, &peer, &occupied)?;
    let login = manager.begin_remote_browser_login()?;
    let result = login
        .complete("http://127.0.0.1:56121/callback?code=must-not-dispatch&state=wrong-state")
        .await;
    assert!(matches!(result, Err(XaiNativeAuthError::InvalidCallback)));
    assert!(manager.status()?.is_none());
    assert!(peer.requests.lock().await.is_empty());
    drop(occupied);
    peer.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_browser_rejects_wrong_host_without_dispatch_and_returns_no_store() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let peer = mock_peer().await?;
    let manager = manager(&temp, &peer)?;
    let login = manager.begin_browser_login().await?;
    let port = login.listener.local_addr()?.port();
    let parsed = Url::parse(login.authorization_url())?;
    let pairs = parsed.query_pairs().collect::<BTreeMap<_, _>>();
    let state = required(pairs.get("state"), "wrong-host state")?;
    let completion = tokio::spawn(async move { login.complete().await });
    let mut callback = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
    callback
        .write_all(
            format!(
                "GET /callback?code=must-not-dispatch&state={state} HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await?;
    let mut response = Vec::new();
    callback.read_to_end(&mut response).await?;
    let response = String::from_utf8_lossy(&response);
    assert!(response.starts_with("HTTP/1.1 400 Bad Request"));
    assert!(response.contains("Cache-Control: no-store\r\n"));
    assert!(!response.contains("must-not-dispatch"));
    let result = tokio::time::timeout(Duration::from_secs(2), completion).await??;
    assert!(matches!(result, Err(XaiNativeAuthError::InvalidCallback)));
    assert!(manager.status()?.is_none());
    assert!(peer.requests.lock().await.is_empty());
    peer.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browser_callback_silent_after_fragment_is_bounded_by_one_deadline() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let peer = mock_peer().await?;
    let manager = manager(&temp, &peer)?;
    let mut login = manager.begin_browser_login().await?;
    let port = login.listener.local_addr()?.port();
    login.flow.deadline = Instant::now() + Duration::from_millis(100);
    let completion = tokio::spawn(async move { login.complete().await });
    let mut callback = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
    callback
        .write_all(b"GET /callback?code=partial HTTP/1.1\r\nHost:")
        .await?;
    let result = tokio::time::timeout(Duration::from_secs(5), completion).await??;
    assert!(matches!(result, Err(XaiNativeAuthError::LoginExpired)));
    assert!(manager.status()?.is_none());
    assert!(peer.requests.lock().await.is_empty());
    drop(callback);
    peer.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browser_completion_rechecks_profile_after_userinfo_before_persistence() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let peer = mock_peer().await?;
    let manager = Arc::new(manager(&temp, &peer)?);
    let login = manager.begin_browser_login().await?;
    let port = login.listener.local_addr()?.port();
    let parsed = Url::parse(login.authorization_url())?;
    let pairs = parsed.query_pairs().collect::<BTreeMap<_, _>>();
    let state = required(pairs.get("state"), "browser state")?.to_string();
    let nonce = required(pairs.get("nonce"), "browser nonce")?.to_string();
    peer.responses
        .send(response(&token_response(
            "browser-expiry-access",
            "browser-expiry-refresh",
            &id_token("browser-expiry-subject", Some(&nonce))?,
        )))
        .await?;
    peer.responses.send(response(&jwks(TEST_KID))).await?;
    let (userinfo, release_userinfo) = gated_response(&json!({"sub":"browser-expiry-subject"}));
    peer.responses.send(userinfo).await?;

    let completion = tokio::spawn(async move { login.complete().await });
    let mut callback = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
    callback
        .write_all(
            format!(
                "GET /callback?code=browser-expiry-code&state={state} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await?;
    wait_until_request_count(&peer, 3).await?;
    manager
        .core
        .compatibility_expires_unix
        .store(system_unix_seconds()?, Ordering::Relaxed);
    release_userinfo
        .send(())
        .map_err(|()| std::io::Error::other("userinfo gate receiver was lost"))?;
    let mut callback_response = Vec::new();
    callback.read_to_end(&mut callback_response).await?;
    assert!(String::from_utf8_lossy(&callback_response).starts_with("HTTP/1.1 400"));
    let result = tokio::time::timeout(Duration::from_secs(2), completion).await??;
    assert!(matches!(
        result,
        Err(XaiNativeAuthError::CompatibilityInactive)
    ));
    assert!(manager.status()?.is_none());
    peer.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(
    clippy::large_futures,
    reason = "test joins two refresh futures; size is not a production concern"
)]
async fn refresh_is_same_account_single_flight_and_rotates_durably() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let peer = mock_peer().await?;
    let manager = manager(&temp, &peer)?;
    install_stored(
        &manager,
        "refresh-subject",
        "old-access",
        "old-refresh",
        system_unix_seconds()? + 1,
    )?;
    peer.responses
        .send(response(&token_response(
            "new-access",
            "new-refresh",
            &id_token("refresh-subject", None)?,
        )))
        .await?;
    peer.responses.send(response(&jwks(TEST_KID))).await?;
    let (first, second) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(manager.refresh_if_needed(), manager.refresh_if_needed())
    })
    .await?;
    first?;
    second?;
    let status = required(manager.status()?, "refresh status")?;
    assert_eq!(status.status(), CredentialStatus::Ready);
    assert_eq!(status.generation().get(), 3);
    let refreshed_fence = required(manager.core.store.load()?, "refreshed fence")?;
    let refreshed = manager.core.read_stored(&refreshed_fence)?;
    assert_eq!(
        refreshed.selection_epoch, "selection-epoch-one-0123456789abcdefghijklm",
        "refresh must retain the login-selection epoch"
    );
    wait_for_requests(&peer, 2).await?;
    let requests = peer.requests.lock().await;
    assert_eq!(
        requests
            .iter()
            .filter(|request| String::from_utf8_lossy(request).starts_with("POST /oauth2/token"))
            .count(),
        1
    );
    let token = String::from_utf8_lossy(&requests[0]);
    assert!(token.contains("grant_type=refresh_token"));
    assert!(token.contains("refresh_token=old-refresh"));
    drop(requests);
    peer.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_nonce_and_userinfo_account_change_never_persist() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let nonce_temp = tempdir()?;
    let nonce_peer = mock_peer().await?;
    let nonce_manager = manager(&nonce_temp, &nonce_peer)?;
    nonce_peer.responses.send(response(&jwks(TEST_KID))).await?;
    let nonce_tokens: TokenResponse = serde_json::from_value(token_response(
        "nonce-access",
        "nonce-refresh",
        &id_token("nonce-subject", Some("actual-nonce"))?,
    ))?;
    let nonce_result = tokio::time::timeout(
        Duration::from_secs(5),
        nonce_manager.core.validate_tokens(
            nonce_tokens,
            Some("expected-nonce"),
            None,
            None,
            None,
            None,
        ),
    )
    .await?;
    assert!(matches!(
        nonce_result,
        Err(XaiNativeAuthError::InvalidIdToken(
            CredentialRefreshFailureReason::IdTokenClaims
        ))
    ));
    assert!(nonce_manager.status()?.is_none());

    let account_temp = tempdir()?;
    let account_peer = mock_peer().await?;
    let account_manager = manager(&account_temp, &account_peer)?;
    account_peer
        .responses
        .send(response(&jwks(TEST_KID)))
        .await?;
    account_peer
        .responses
        .send(response(&json!({"sub":"different-subject"})))
        .await?;
    let account_tokens: TokenResponse = serde_json::from_value(token_response(
        "account-access",
        "account-refresh",
        &id_token("signed-subject", None)?,
    ))?;
    let account_result = tokio::time::timeout(
        Duration::from_secs(5),
        account_manager
            .core
            .validate_tokens(account_tokens, None, None, None, None, None),
    )
    .await?;
    assert!(matches!(
        account_result,
        Err(XaiNativeAuthError::AccountChanged)
    ));
    assert!(account_manager.status()?.is_none());
    nonce_peer.shutdown().await?;
    account_peer.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_optional_token_metadata_id_token_and_azp_are_compatible() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;

    let minimum_temp = tempdir()?;
    let minimum_peer = mock_peer().await?;
    let minimum_manager = manager(&minimum_temp, &minimum_peer)?;
    minimum_peer
        .responses
        .send(response(&json!({"sub":"userinfo-subject"})))
        .await?;
    let access_expires_at = system_unix_seconds()? + 900;
    let minimum_access = access_token_with_expiry(access_expires_at)?;
    let minimum: TokenResponse = serde_json::from_value(json!({
        "access_token":minimum_access,
        "refresh_token":"minimum-refresh"
    }))?;
    let stored = minimum_manager
        .core
        .validate_tokens(
            minimum,
            Some("unused-without-id-token"),
            None,
            None,
            None,
            None,
        )
        .await?;
    assert_eq!(stored.subject, "userinfo-subject");
    assert_eq!(stored.expires_at, access_expires_at);
    minimum_peer.shutdown().await?;

    let azp_temp = tempdir()?;
    let azp_peer = mock_peer().await?;
    let azp_manager = manager(&azp_temp, &azp_peer)?;
    azp_peer.responses.send(response(&jwks(TEST_KID))).await?;
    azp_peer
        .responses
        .send(response(&json!({"sub":"signed-subject"})))
        .await?;
    let without_azp: TokenResponse = serde_json::from_value(token_response(
        "signed-access",
        "signed-refresh",
        &id_token_with_azp("signed-subject", Some("nonce"), None)?,
    ))?;
    let stored = azp_manager
        .core
        .validate_tokens(without_azp, Some("nonce"), None, None, None, None)
        .await?;
    assert_eq!(stored.subject, "signed-subject");
    azp_peer.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ambiguous_refresh_restores_ready_and_retries() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let peer = mock_peer().await?;
    let manager = manager(&temp, &peer)?;
    install_stored(
        &manager,
        "quarantine-subject",
        "old-access",
        "old-refresh",
        system_unix_seconds()? + 1,
    )?;
    peer.responses.send(disconnect_after_request()).await?;
    let result = tokio::time::timeout(Duration::from_secs(5), manager.refresh_if_needed()).await?;
    assert!(matches!(result, Err(XaiNativeAuthError::Transport(_))));
    let status = required(manager.status()?, "restored status")?;
    assert_eq!(status.status(), CredentialStatus::Ready);
    assert_eq!(status.generation().get(), 3);
    peer.responses
        .send(response(&token_response(
            "retry-access",
            "retry-refresh",
            &id_token("quarantine-subject", None)?,
        )))
        .await?;
    peer.responses.send(response(&jwks(TEST_KID))).await?;
    tokio::time::timeout(Duration::from_secs(5), manager.refresh_if_needed()).await??;
    let status = required(manager.status()?, "second refresh status")?;
    assert_eq!(status.status(), CredentialStatus::Ready);
    assert_eq!(status.generation().get(), 5);
    peer.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_claimed_refresh_after_request_observation_restores_ready() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let peer = mock_peer().await?;
    let manager = Arc::new(manager(&temp, &peer)?);
    install_stored(
        &manager,
        "cancel-refresh-subject",
        "cancel-refresh-access",
        "cancel-refresh-token",
        system_unix_seconds()? + 1,
    )?;
    let (release_head, before_head) = oneshot::channel();
    peer.responses
        .send(MockResponse {
            status: "200 OK",
            content_type: "application/json",
            body: token_response(
                "cancel-new-access",
                "cancel-new-refresh",
                &id_token("cancel-refresh-subject", None)?,
            )
            .to_string(),
            extra_headers: String::new(),
            disconnect_after_request: false,
            truncate_after: None,
            before_head: Some(before_head),
            before_body: None,
        })
        .await?;
    let refresh_manager = Arc::clone(&manager);
    let refresh = tokio::spawn(async move { refresh_manager.refresh_if_needed().await });
    wait_until_request_count(&peer, 1).await?;
    refresh.abort();
    let joined = tokio::time::timeout(Duration::from_secs(1), refresh).await?;
    assert!(joined.is_err_and(|error| error.is_cancelled()));
    drop(release_head);
    assert_eq!(
        required(manager.status()?, "cancel restore")?.status(),
        CredentialStatus::Ready
    );
    peer.abort_join().await?;
    Ok(())
}

/// A refresh cancelled while it waits behind a peer's claim never claimed
/// anything, so it must neither restore the peer's Unknown record nor block
/// its drop on the peer's held store.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_refresh_waiting_on_a_peer_claim_leaves_the_peer_claim_alone() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let peer = mock_peer().await?;
    let manager = Arc::new(manager(&temp, &peer)?);
    install_stored(
        &manager,
        "peer-wait-subject",
        "peer-wait-access",
        "peer-wait-refresh",
        system_unix_seconds()?,
    )?;
    let peer_store = PrivateCredentialStore::open_slot(
        temp.path().join("credentials").join("xai.json"),
        auth_profile_id()?,
        CredentialSlotId::new("test-selected")?,
    )?;
    let fence = required(peer_store.load()?, "peer fence")?;
    let peer_claim = peer_store.begin_refresh(&fence)?.start();
    let unknown = peer_claim.fence();
    let refresh_manager = Arc::clone(&manager);
    let refresh = tokio::spawn(async move { refresh_manager.refresh_if_needed().await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!refresh.is_finished());
    refresh.abort();
    let joined = tokio::time::timeout(Duration::from_secs(1), refresh).await?;
    assert!(joined.is_err_and(|error| error.is_cancelled()));
    assert_eq!(peer_store.load()?, Some(unknown.clone()));
    drop(peer_claim);
    assert_eq!(peer_store.load()?, Some(unknown));
    assert!(peer.requests.lock().await.is_empty());
    peer.abort_join().await?;
    Ok(())
}

/// After our POST fails, a peer may reclaim our Unknown record before the
/// post-start-failure hook runs. The hook restores only its own claimed
/// fence, so the peer's Unknown claim survives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_start_failure_restore_leaves_a_peer_reclaimed_unknown_alone() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let peer = mock_peer().await?;
    let manager = manager(&temp, &peer)?;
    install_stored(
        &manager,
        "peer-reclaim-subject",
        "peer-reclaim-access",
        "peer-reclaim-refresh",
        system_unix_seconds()?,
    )?;
    let peer_store = PrivateCredentialStore::open_slot(
        temp.path().join("credentials").join("xai.json"),
        auth_profile_id()?,
        CredentialSlotId::new("test-selected")?,
    )?;
    let ready = required(peer_store.load()?, "ready fence")?;
    let ours = peer_store.begin_refresh(&ready)?.start().fence();
    let peers = peer_store
        .begin_refresh_reclaiming_unknown(&ours)?
        .start()
        .fence();
    assert_ne!(ours, peers);

    let core = &*manager.core;
    let mut hooks = XaiRefreshHooks {
        cancel_guard: XaiRefreshCancelGuard::new(&core.store, None),
        core,
        expected_account: None,
        expected_selection_epoch: None,
        expected_authority: None,
        _flight: core.refresh_flight.lock().await,
        request_permit: None,
        started: Instant::now(),
        force: false,
        detail: CredentialRefreshDetail::default(),
    };
    hooks.cancel_guard.claimed(ours);
    hooks.after_post_start_failure(&core.store, &XaiNativeAuthError::Transport(None));
    hooks.cancel_guard.finished();
    drop(hooks);

    assert_eq!(peer_store.load()?, Some(peers));
    peer.abort_join().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claimed_store_busy_rejects_before_wire_and_restores_exact_claim() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let held = mock_peer().await?;
    let target = mock_peer().await?;
    let manager = manager(&temp, &target)?;
    let expires_at = system_unix_seconds()?.saturating_sub(1);
    install_stored(
        &manager,
        "busy-primary",
        "busy-access",
        "busy-refresh",
        expires_at,
    )?;

    let sibling = PrivateCredentialStore::open_slot(
        manager.core.store.path().with_file_name("sibling.json"),
        auth_profile_id()?,
        CredentialSlotId::new("sibling")?,
    )?;
    sibling.initialize_selected(&stored_record(
        "busy-sibling",
        "sibling-access",
        "sibling-refresh",
        expires_at,
        CredentialGeneration::new(1)?,
    )?)?;
    held.responses.send(response(&json!({}))).await?;

    let held_request = manager.core.form_request(
        AuthEndpoints::test(&held.origin).token(),
        urlencoded_form(&[("fixture", "held")]),
    )?;
    let primary_before = manager.core.store.load()?.ok_or("missing primary")?;
    let sibling_claim = sibling.begin_refresh(&sibling.load()?.ok_or("missing sibling")?)?;
    let held_response = sibling
        .execute_claimed_http(
            sibling_claim,
            &held.client,
            held_request,
            ExchangeControl::new(Instant::now() + REQUEST_TIMEOUT),
        )
        .await?;

    // The sibling's claimed exchange owns the private directory. The
    // primary cannot claim at all while it is held: the store reports the
    // local contention fact before any durable write, the ladder backs
    // off, and the primary record stays exactly as it was.
    let worker_core = Arc::clone(&manager.core);
    let mut worker =
        tokio::spawn(async move { worker_core.refresh_if_needed(None, None, None).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(80), &mut worker)
            .await
            .is_err(),
        "worker must still be backing off while the sibling holds the lock"
    );
    assert!(target.requests.lock().await.is_empty());
    assert_eq!(
        manager.core.store.load()?.ok_or("missing primary")?,
        primary_before
    );

    // Releasing the directory lets the primary claim and dispatch. Its
    // exchange never completes a handshake (no response is queued), which
    // is a definitely-not-started transport failure: the exact claim is
    // restored at the next generation.
    sibling.quarantine_claimed_http(held_response)?;
    assert!(matches!(
        worker.await?,
        Err(XaiNativeAuthError::Transport(_))
    ));
    let restored = manager
        .core
        .store
        .load()?
        .ok_or("missing restored primary")?;
    assert_eq!(restored.status(), CredentialStatus::Ready);
    assert_eq!(restored.generation(), CredentialGeneration::new(3)?);
    held.shutdown().await?;
    target.abort_join().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claimed_refresh_body_parse_and_account_failures_consume_quarantine_terminal() -> TestResult
{
    let _serial = AUTH_TEST_GATE.lock().await;

    let body_temp = tempdir()?;
    let body_peer = mock_peer().await?;
    let body_manager = manager(&body_temp, &body_peer)?;
    install_stored(
        &body_manager,
        "body-subject",
        "body-access",
        "body-refresh",
        system_unix_seconds()? + 1,
    )?;
    let body = token_response(
        "new-body-access",
        "new-body-refresh",
        &id_token("body-subject", None)?,
    )
    .to_string();
    body_peer
        .responses
        .send(MockResponse {
            status: "200 OK",
            content_type: "application/json",
            body,
            extra_headers: String::new(),
            disconnect_after_request: false,
            truncate_after: Some(8),
            before_head: None,
            before_body: None,
        })
        .await?;
    assert!(matches!(
        body_manager.refresh_if_needed().await,
        Err(XaiNativeAuthError::Transport(_))
    ));
    assert_eq!(
        required(body_manager.status()?, "body restore")?.status(),
        CredentialStatus::Ready
    );
    wait_for_requests(&body_peer, 1).await?;
    body_peer.shutdown().await?;

    let parse_temp = tempdir()?;
    let parse_peer = mock_peer().await?;
    let parse_manager = manager(&parse_temp, &parse_peer)?;
    install_stored(
        &parse_manager,
        "parse-subject",
        "parse-access",
        "parse-refresh",
        system_unix_seconds()? + 1,
    )?;
    parse_peer
        .responses
        .send(response(&json!({"unexpected":"token-shape"})))
        .await?;
    assert!(matches!(
        parse_manager.refresh_if_needed().await,
        Err(XaiNativeAuthError::InvalidTokenResponse(_))
    ));
    assert_eq!(
        required(parse_manager.status()?, "parse restore")?.status(),
        CredentialStatus::Ready
    );
    wait_for_requests(&parse_peer, 1).await?;
    parse_peer.shutdown().await?;

    let account_temp = tempdir()?;
    let account_peer = mock_peer().await?;
    let account_manager = manager(&account_temp, &account_peer)?;
    install_stored(
        &account_manager,
        "expected-refresh-subject",
        "account-access",
        "account-refresh",
        system_unix_seconds()? + 1,
    )?;
    account_peer
        .responses
        .send(response(&token_response(
            "new-account-access",
            "new-account-refresh",
            &id_token("changed-refresh-subject", None)?,
        )))
        .await?;
    account_peer
        .responses
        .send(response(&jwks(TEST_KID)))
        .await?;
    assert!(matches!(
        account_manager.refresh_if_needed().await,
        Err(XaiNativeAuthError::AccountChanged)
    ));
    assert_eq!(
        required(account_manager.status()?, "account quarantine")?.status(),
        CredentialStatus::RefreshOutcomeUnknown
    );
    wait_for_requests(&account_peer, 2).await?;
    account_peer.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_accepts_id_token_within_clock_leeway_not_refresh_lead() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let peer = mock_peer().await?;
    let manager = manager(&temp, &peer)?;
    install_stored(
        &manager,
        "leeway-subject",
        "leeway-access",
        "leeway-refresh",
        system_unix_seconds()? + 1,
    )?;
    let now = system_unix_seconds()?;
    peer.responses
        .send(response(&token_response(
            "leeway-new-access",
            "leeway-new-refresh",
            &id_token_with_exp("leeway-subject", None, now + 90)?,
        )))
        .await?;
    peer.responses.send(response(&jwks(TEST_KID))).await?;
    tokio::time::timeout(Duration::from_secs(5), manager.refresh_if_needed()).await??;
    let status = required(manager.status()?, "leeway refresh status")?;
    assert_eq!(status.status(), CredentialStatus::Ready);
    assert_eq!(status.generation().get(), 3);
    peer.shutdown().await?;
    Ok(())
}

#[derive(Default)]
struct RecordingRefreshObserver {
    events: std::sync::Mutex<Vec<CredentialRefreshEvent>>,
}

impl CredentialRefreshObserver for RecordingRefreshObserver {
    fn observe(&self, event: CredentialRefreshEvent) {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_failure_names_the_rejecting_check_and_response_timing() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let peer = mock_peer().await?;
    let observer = Arc::new(RecordingRefreshObserver::default());
    let manager = manager(&temp, &peer)?.with_credential_refresh_observer(observer.clone())?;
    install_stored(
        &manager,
        "expired-id-subject",
        "expired-id-access",
        "expired-id-refresh",
        system_unix_seconds()? + 1,
    )?;
    let now = system_unix_seconds()?;
    peer.responses
        .send(response(&token_response(
            "expired-id-new-access",
            "expired-id-new-refresh",
            &id_token_with_exp("expired-id-subject", None, now - 600)?,
        )))
        .await?;
    peer.responses.send(response(&jwks(TEST_KID))).await?;
    let result = tokio::time::timeout(Duration::from_secs(5), manager.refresh_if_needed()).await?;
    assert!(
        matches!(
            result,
            Err(XaiNativeAuthError::InvalidIdToken(
                CredentialRefreshFailureReason::IdTokenExp
            ))
        ),
        "{result:?}"
    );
    let events = observer
        .events
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let last = events.last().ok_or("no refresh observation")?;
    assert_eq!(last.phase, CredentialRefreshPhase::Restored);
    assert_eq!(
        last.failure_class,
        Some(CredentialRefreshFailureClass::Validation)
    );
    assert_eq!(
        last.detail.reason,
        Some(CredentialRefreshFailureReason::IdTokenExp)
    );
    assert_eq!(last.detail.expires_in_secs, Some(3600));
    let exp_in = last
        .detail
        .id_token_exp_in_secs
        .ok_or("exp delta missing")?;
    assert!((-700..=-590).contains(&exp_in), "{exp_in}");
    let iat_age = last
        .detail
        .id_token_iat_age_secs
        .ok_or("iat delta missing")?;
    assert!((0..=10).contains(&iat_age), "{iat_age}");
    assert!(
        events
            .iter()
            .all(|event| event.phase == CredentialRefreshPhase::Began
                || event.detail.reason == Some(CredentialRefreshFailureReason::IdTokenExp))
    );
    peer.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_http_400_stays_durably_quarantined() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let peer = mock_peer().await?;
    let manager = manager(&temp, &peer)?;
    install_stored(
        &manager,
        "bad-request-subject",
        "bad-request-access",
        "bad-request-refresh",
        system_unix_seconds()? + 1,
    )?;
    peer.responses
        .send(MockResponse {
            status: "400 Bad Request",
            content_type: "application/json",
            body: json!({"error":"invalid_grant"}).to_string(),
            extra_headers: String::new(),
            disconnect_after_request: false,
            truncate_after: None,
            before_head: None,
            before_body: None,
        })
        .await?;
    assert!(matches!(
        manager.refresh_if_needed().await,
        Err(XaiNativeAuthError::RefreshQuarantined)
    ));
    assert_eq!(
        required(manager.status()?, "400 quarantine")?.status(),
        CredentialStatus::RefreshOutcomeUnknown
    );
    peer.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_http_503_restores_ready() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let peer = mock_peer().await?;
    let manager = manager(&temp, &peer)?;
    install_stored(
        &manager,
        "server-error-subject",
        "server-error-access",
        "server-error-refresh",
        system_unix_seconds()? + 1,
    )?;
    peer.responses
        .send(MockResponse {
            status: "503 Service Unavailable",
            content_type: "application/json",
            body: json!({"error":"temporarily_unavailable"}).to_string(),
            extra_headers: String::new(),
            disconnect_after_request: false,
            truncate_after: None,
            before_head: None,
            before_body: None,
        })
        .await?;
    assert!(matches!(
        manager.refresh_if_needed().await,
        Err(XaiNativeAuthError::Transport(_))
    ));
    assert_eq!(
        required(manager.status()?, "503 restore")?.status(),
        CredentialStatus::Ready
    );
    peer.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_refresh_losing_the_execution_lock_restores_ready() -> TestResult {
    // A peer process (a second store handle over the same slot) owns the
    // refresh execution lock. Losing that lock is a local fact: nothing is
    // written, no request reaches the provider, and when the peer's refresh
    // completes the new Ready generation is adopted as fresh.
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let peer = mock_peer().await?;
    let manager = manager(&temp, &peer)?;
    install_stored(
        &manager,
        "peer-subject",
        "peer-access",
        "peer-refresh",
        system_unix_seconds()? + 1,
    )?;
    let peer_store = PrivateCredentialStore::open_slot(
        temp.path().join("credentials").join("xai.json"),
        auth_profile_id()?,
        CredentialSlotId::new("test-selected")?,
    )?;
    let fence = required(peer_store.load()?, "peer fence")?;
    let peer_claim = peer_store.begin_refresh(&fence)?;
    assert_eq!(
        required(manager.status()?, "claimed status")?.status(),
        CredentialStatus::RefreshOutcomeUnknown
    );
    let replacement = stored_record(
        "peer-subject",
        "peer-new-access",
        "peer-new-refresh",
        system_unix_seconds()? + 7_200,
        CredentialGeneration::new(3)?,
    )?;
    let completer = tokio::task::spawn_blocking(move || {
        std::thread::sleep(Duration::from_millis(200));
        peer_store.complete_refresh(peer_claim.start(), replacement)
    });
    tokio::time::timeout(Duration::from_secs(5), manager.refresh_if_needed()).await??;
    completer.await??;
    let status = required(manager.status()?, "adopted status")?;
    assert_eq!(status.status(), CredentialStatus::Ready);
    assert_eq!(status.generation().get(), 3);
    assert!(
        peer.requests.lock().await.is_empty(),
        "the losing refresh must not send its own token exchange"
    );
    peer.abort_join().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_spawn_gate_refreshes_a_quarantined_xai_record() -> TestResult {
    // The spawn gate refreshes before it reads status. A durable Unknown
    // (for example a crash mid-refresh) is reclaimed on the stored refresh
    // token and ends Ready, so the child is admitted instead of refused.
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let peer = mock_peer().await?;
    let manager = manager(&temp, &peer)?;
    install_stored(
        &manager,
        "gate-subject",
        "gate-access",
        "gate-refresh",
        system_unix_seconds()? + 7_200,
    )?;
    manager
        .core
        .store
        .mark_refresh_intent(CredentialGeneration::new(1)?)?;
    assert_eq!(
        required(manager.status()?, "seeded quarantine")?.status(),
        CredentialStatus::RefreshOutcomeUnknown
    );
    peer.responses
        .send(response(&token_response(
            "gate-new-access",
            "gate-new-refresh",
            &id_token("gate-subject", None)?,
        )))
        .await?;
    peer.responses.send(response(&jwks(TEST_KID))).await?;
    tokio::time::timeout(Duration::from_secs(5), manager.refresh_if_needed()).await??;
    let status = required(manager.status()?, "refreshed status")?;
    assert_eq!(status.status(), CredentialStatus::Ready);
    assert_eq!(status.generation().get(), 4);
    peer.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rejected_grant_still_ends_not_ready() -> TestResult {
    // The negative: only local and transport facts are retryable. A
    // provider rejection of the grant is terminal and the durable record
    // must not be painted Ready by any restore path.
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let peer = mock_peer().await?;
    let manager = manager(&temp, &peer)?;
    install_stored(
        &manager,
        "rejected-subject",
        "rejected-access",
        "rejected-refresh",
        system_unix_seconds()? + 1,
    )?;
    peer.responses
        .send(MockResponse {
            status: "400 Bad Request",
            content_type: "application/json",
            body: json!({"error":"invalid_grant"}).to_string(),
            extra_headers: String::new(),
            disconnect_after_request: false,
            truncate_after: None,
            before_head: None,
            before_body: None,
        })
        .await?;
    let error = manager
        .refresh_if_needed()
        .await
        .err()
        .ok_or("rejected grant unexpectedly refreshed")?;
    assert!(matches!(error, XaiNativeAuthError::RefreshQuarantined));
    assert!(!xai_refresh_failure_is_retryable(&error));
    assert!(xai_refresh_failure_is_retryable(
        &XaiNativeAuthError::Store(PrivateStoreError::StoreBusy)
    ));
    let status = required(manager.status()?, "rejected status")?;
    assert_eq!(status.status(), CredentialStatus::RefreshOutcomeUnknown);
    assert_ne!(status.status(), CredentialStatus::Ready);
    assert_eq!(status.generation().get(), 2);
    peer.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoke_is_fixed_remote_then_tombstone_and_logout_is_local() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let revoke_temp = tempdir()?;
    let revoke_peer = mock_peer().await?;
    let revoke_manager = manager(&revoke_temp, &revoke_peer)?;
    install_stored(
        &revoke_manager,
        "revoke-subject",
        "access",
        "refresh-to-revoke",
        system_unix_seconds()? + 3600,
    )?;
    revoke_peer.responses.send(response(&json!({}))).await?;
    let generation =
        tokio::time::timeout(Duration::from_secs(5), revoke_manager.revoke()).await??;
    assert_eq!(generation.get(), 3);
    assert_eq!(
        required(revoke_manager.status()?, "revoke tombstone")?.status(),
        CredentialStatus::Revoked
    );
    wait_for_requests(&revoke_peer, 1).await?;
    let wire = String::from_utf8_lossy(&revoke_peer.requests.lock().await[0]).to_ascii_lowercase();
    assert!(wire.starts_with("post /oauth2/revoke http/1.1"));
    assert!(wire.contains("token=refresh-to-revoke"));
    assert!(wire.contains("token_type_hint=refresh_token"));
    assert!(wire.contains("client_id=b1a00492-073a-47ea-816f-4c329264a828"));

    let logout_temp = tempdir()?;
    let logout_peer = mock_peer().await?;
    let logout_manager = manager(&logout_temp, &logout_peer)?;
    install_stored(
        &logout_manager,
        "logout-subject",
        "access",
        "refresh",
        system_unix_seconds()? + 3600,
    )?;
    assert_eq!(logout_manager.logout()?.get(), 2);
    assert_eq!(
        required(logout_manager.status()?, "logout tombstone")?.status(),
        CredentialStatus::Revoked
    );
    assert!(logout_peer.requests.lock().await.is_empty());
    revoke_peer.shutdown().await?;
    logout_peer.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_kid_forced_refresh_is_single_flight() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let peer = mock_peer().await?;
    let manager = manager(&temp, &peer)?;
    peer.responses.send(response(&jwks(TEST_KID))).await?;
    let (first, second) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            manager.core.jwk_for("unknown-key", None),
            manager.core.jwk_for("unknown-key", None)
        )
    })
    .await?;
    assert!(matches!(first, Err(XaiNativeAuthError::UnknownSigningKey)));
    assert!(matches!(second, Err(XaiNativeAuthError::UnknownSigningKey)));
    assert!(matches!(
        manager.core.jwk_for("unknown-key", None).await,
        Err(XaiNativeAuthError::UnknownSigningKey)
    ));
    wait_for_requests(&peer, 1).await?;
    assert_eq!(peer.requests.lock().await.len(), 1);
    peer.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolver_rejects_requested_alias_or_bogus_id_before_credentials_and_wire() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let manager = manager(&temp, &auth)?;
    install_stored(
        &manager,
        "resolver-subject",
        "resolver-access",
        "resolver-refresh",
        system_unix_seconds()? + 3_600,
    )?;
    // Every identity outside the exact reviewed table is rejected before
    // credential acquisition or catalog transport, regardless of shape.
    for arbitrary in [
        "gpt-not-a-xai-model",
        "grok-4.20-0309-reasoning",
        XAI_GROK_BUILD_MODEL,
        "grok-build-latest",
        "grok-build-0.1-alias",
    ] {
        assert!(matches!(
            manager.resolve_model(arbitrary, auth.client.clone()).await,
            Err(XaiNativeAuthError::RequestedModelNotAdmitted)
        ));
    }
    assert!(auth.requests.lock().await.is_empty());

    // The catalog still includes aliases to prove that authenticated
    // diagnostics cannot expand the closed local model table.
    let catalog_body = json!({
        "object":"list",
        "data":[{
            "id":XAI_GROK_PRIMARY_CODING_MODEL,
            "aliases":["grok-build-latest", "grok-build-0.1-alias"],
            "object":"model",
            "created":1,
            "owned_by":"xai",
            "contextWindow":4096
        }]
    });
    auth.responses.send(response(&catalog_body)).await?;
    // Exact catalog id still resolves.
    let (_model, evidence) = manager
        .resolve_model_with_evidence(
            XAI_GROK_PRIMARY_CODING_MODEL,
            auth.client.clone(),
            StandardXaiHttpConfig::default(),
        )
        .await?;
    assert_eq!(
        evidence
            .catalog_diagnostics()
            .reported_context_length()
            .get(),
        4096
    );
    let route = manager.subscription_route()?;
    let expected_resolved = XaiResolvedModel::new(
        route.clone(),
        XaiModel::new(
            ModelId::new(XAI_GROK_PRIMARY_CODING_MODEL)?,
            Some(ModelFamilyId::new("responses-family")?),
        ),
        GrokCodingProfile::pinned_compatibility(),
        evidence.catalog_observed_at(),
    )?;
    let expected_descriptor = expected_resolved.descriptor();
    assert_eq!(
        expected_descriptor.requested_model().as_str(),
        XAI_GROK_PRIMARY_CODING_MODEL
    );
    let identity = required(route.identity(), "native route identity")?;
    assert_eq!(evidence.account_fingerprint(), identity.account_binding());
    assert_eq!(
        evidence.credential_authority_id(),
        identity.credential_authority()
    );
    assert_eq!(
        evidence.credential_authority_binding(),
        identity.credential_authority_binding()
    );
    assert_eq!(evidence.selected_credential_generation(), 1);
    assert_eq!(
        evidence.credential_selection_epoch(),
        "selection-epoch-one-0123456789abcdefghijklm"
    );
    assert_eq!(evidence.route_fingerprint(), identity.fingerprint());
    assert_eq!(
        evidence.descriptor_digest(),
        expected_descriptor.binding_digest()?
    );
    assert_eq!(
        expected_descriptor.route().as_str(),
        crate::XaiRouteKind::GrokSubscription.route_id()
    );
    assert_ne!(evidence.route_fingerprint(), "");
    assert_eq!(
        evidence.dialect(),
        XaiDialect::GrokSubscriptionResponses2026_07
    );
    assert_eq!(evidence.adapter_revision(), XAI_RESPONSES_ADAPTER_REVISION);
    assert_eq!(evidence.profile_revision(), XAI_GROK_COMPATIBILITY_PROFILE);
    assert_eq!(evidence.profile_id(), XAI_GROK_COMPATIBILITY_PROFILE);
    let catalog = required(
        manager
            .core
            .catalog
            .lock()
            .map_err(|_| std::io::Error::other("catalog lock poisoned"))?
            .clone(),
        "resolution catalog receipt",
    )?;
    assert_eq!(evidence.catalog_observed_at(), catalog.public.observed_at());
    assert_eq!(evidence.catalog_expires_at(), catalog.public.expires_at());
    // Only the exact reviewed identity reaches authenticated discovery.
    wait_for_requests(&auth, 1).await?;
    let requests = auth.requests.lock().await;
    assert_eq!(requests.len(), 1);
    let catalog_wire = String::from_utf8_lossy(&requests[0]).to_ascii_lowercase();
    assert!(catalog_wire.starts_with("get /v1/models http/1.1"));
    assert!(catalog_wire.contains("authorization: bearer resolver-access"));
    assert!(catalog_wire.contains("x-xai-token-auth: xai-grok-cli"));
    assert!(catalog_wire.contains("x-userid: resolver-subject"));
    assert!(catalog_wire.contains("x-grok-client-version: 1.0.46"));
    assert!(catalog_wire.contains("x-grok-client-identifier: plakous"));
    assert!(catalog_wire.contains("x-grok-client-mode: interactive"));
    assert!(!catalog_wire.contains("x-authenticateresponse"));
    assert!(!catalog_wire.contains("x-email"));
    drop(requests);
    auth.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolver_rejects_generation_only_relogin_race_before_return() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let manager = Arc::new(manager(&temp, &auth)?);
    install_stored(
        &manager,
        "generation-race-subject",
        "generation-race-access-one",
        "generation-race-refresh-one",
        system_unix_seconds()? + 3_600,
    )?;
    let reached = Arc::new(Notify::new());
    let resume = Arc::new(Notify::new());
    *manager
        .core
        .resolution_pause
        .lock()
        .map_err(|_| std::io::Error::other("resolution pause lock poisoned"))? =
        Some(ResolutionPause {
            reached: Arc::clone(&reached),
            resume: Arc::clone(&resume),
        });
    auth.responses
        .send(response(&json!({
            "object":"list",
            "data":[{
                "id":XAI_GROK_PRIMARY_CODING_MODEL,
                "object":"model",
                "created":1,
                "owned_by":"xai",
                "contextWindow":4096
            }]
        })))
        .await?;
    let resolver = {
        let manager = Arc::clone(&manager);
        let client = auth.client.clone();
        tokio::spawn(async move {
            manager
                .resolve_model_with_evidence(
                    XAI_GROK_PRIMARY_CODING_MODEL,
                    client,
                    StandardXaiHttpConfig::default(),
                )
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(5), reached.notified()).await?;

    let baseline = required(manager.core.store.load()?, "resolution race fence")?;
    let replacement = StoredTokens {
        version: STORED_TOKENS_VERSION,
        issuer: XAI_ISSUER.to_owned(),
        client_id: XAI_PUBLIC_CLIENT_ID.to_owned(),
        subject: "generation-race-subject".to_owned(),
        scope: XAI_OAUTH_SCOPE.to_owned(),
        access_token: "generation-race-access-two".to_owned(),
        refresh_token: "generation-race-refresh-two".to_owned(),
        expires_at: system_unix_seconds()? + 3_600,
        selection_epoch: "selection-epoch-one-0123456789abcdefghijklm".to_owned(),
    };
    let mutation = manager.core.store.replace_selected_login(
        &baseline,
        credential_record(
            AccountBinding::new("generation-race-subject")?,
            baseline.generation().next()?,
            &replacement,
        )?,
    );
    resume.notify_one();
    mutation?;
    let result = tokio::time::timeout(Duration::from_secs(5), resolver).await??;
    assert!(matches!(result, Err(XaiNativeAuthError::SelectionChanged)));
    assert_eq!(auth.requests.lock().await.len(), 1);
    auth.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolver_rejects_catalog_receipt_mutation_before_return() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let manager = Arc::new(manager(&temp, &auth)?);
    install_stored(
        &manager,
        "catalog-mutation-subject",
        "catalog-mutation-access",
        "catalog-mutation-refresh",
        system_unix_seconds()? + 3_600,
    )?;
    let reached = Arc::new(Notify::new());
    let resume = Arc::new(Notify::new());
    *manager
        .core
        .resolution_pause
        .lock()
        .map_err(|_| std::io::Error::other("resolution pause lock poisoned"))? =
        Some(ResolutionPause {
            reached: Arc::clone(&reached),
            resume: Arc::clone(&resume),
        });
    auth.responses
        .send(response(&json!({
            "object":"list",
            "data":[{
                "id":XAI_GROK_PRIMARY_CODING_MODEL,
                "object":"model",
                "created":1,
                "owned_by":"xai",
                "contextWindow":4096
            }]
        })))
        .await?;
    let resolver = {
        let manager = Arc::clone(&manager);
        let client = auth.client.clone();
        tokio::spawn(async move {
            manager
                .resolve_model_with_evidence(
                    XAI_GROK_PRIMARY_CODING_MODEL,
                    client,
                    StandardXaiHttpConfig::default(),
                )
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(5), reached.notified()).await?;
    *manager
        .core
        .catalog
        .lock()
        .map_err(|_| std::io::Error::other("catalog lock poisoned"))? = None;
    resume.notify_one();
    let result = tokio::time::timeout(Duration::from_secs(5), resolver).await??;
    assert!(matches!(
        result,
        Err(XaiNativeAuthError::CatalogUnavailable)
    ));
    assert_eq!(auth.requests.lock().await.len(), 1);
    auth.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolution_evidence_debug_and_serde_redact_credentials_account_and_paths() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let manager = manager(&temp, &auth)?;
    install_stored(
        &manager,
        "redaction-account-label",
        "redaction-bearer-token",
        "redaction-refresh-token",
        system_unix_seconds()? + 3_600,
    )?;
    auth.responses
        .send(response(&json!({
            "object":"list",
            "data":[{
                "id":XAI_GROK_PRIMARY_CODING_MODEL,
                "object":"model",
                "created":1,
                "owned_by":"xai",
                "contextWindow":4096
            }]
        })))
        .await?;
    let (_, evidence) = manager
        .resolve_model_with_evidence(
            XAI_GROK_PRIMARY_CODING_MODEL,
            auth.client.clone(),
            StandardXaiHttpConfig::default(),
        )
        .await?;
    let rendered = format!("{:?}\n{}", evidence, serde_json::to_string(&evidence)?);
    let store_path = temp.path().to_string_lossy().into_owned();
    for forbidden in [
        "redaction-account-label",
        "redaction-bearer-token",
        "redaction-refresh-token",
        store_path.as_str(),
        CATALOG_PATH,
        "/v1/responses",
    ] {
        assert!(
            !rendered.contains(forbidden),
            "resolution evidence exposed forbidden credential, account, or path material"
        );
    }
    assert!(rendered.contains(evidence.credential_selection_epoch()));
    assert!(rendered.contains(evidence.route_fingerprint()));
    assert!(rendered.contains(evidence.profile_revision()));
    auth.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_authority_binds_exact_model_route_catalog_selection_and_profile_without_secrets()
-> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let manager = manager(&temp, &auth)?;
    let credential_expires_at = system_unix_seconds()? + CATALOG_TTL_SECONDS;
    install_stored(
        &manager,
        "runtime-authority-account",
        "runtime-authority-access-token",
        "runtime-authority-refresh-token",
        credential_expires_at,
    )?;
    auth.responses
        .send(response(&json!({
            "object":"list",
            "data":[
                {
                    "id":XAI_GROK_PRIMARY_CODING_MODEL,
                    "object":"model",
                    "created":1,
                    "owned_by":"xai",
                    "contextWindow":4096
                },
                {
                    "id":XAI_GROK_46_CODING_MODEL,
                    "object":"model",
                    "created":1,
                    "owned_by":"xai",
                    "contextWindow":8192
                }
            ]
        })))
        .await?;
    let catalog = manager.refresh_catalog().await?;
    let first_peer = mock_peer().await?;
    let second_peer = mock_peer().await?;
    let first_route =
        manager.test_subscription_route(crate::XaiOrigin::new(first_peer.origin.clone())?)?;
    let second_route =
        manager.test_subscription_route(crate::XaiOrigin::new(second_peer.origin.clone())?)?;
    let first_resolved = XaiResolvedModel::new(
        first_route.clone(),
        XaiModel::new(ModelId::new(XAI_GROK_PRIMARY_CODING_MODEL)?, None),
        GrokCodingProfile::pinned_compatibility(),
        catalog.observed_at(),
    )?;
    let alternate_model_resolved = XaiResolvedModel::new(
        first_route,
        XaiModel::new(ModelId::new(XAI_GROK_46_CODING_MODEL)?, None),
        GrokCodingProfile::pinned_compatibility(),
        catalog.observed_at(),
    )?;
    let alternate_route_resolved = XaiResolvedModel::new(
        second_route,
        XaiModel::new(ModelId::new(XAI_GROK_PRIMARY_CODING_MODEL)?, None),
        GrokCodingProfile::pinned_compatibility(),
        catalog.observed_at(),
    )?;
    let first_descriptor = first_resolved.descriptor().clone();
    let first: Arc<dyn LanguageModel> = Arc::new(XaiLanguageModel::new(
        first_resolved,
        first_peer.client.clone(),
    )?);
    let alternate_model: Arc<dyn LanguageModel> = Arc::new(XaiLanguageModel::new(
        alternate_model_resolved,
        first_peer.client.clone(),
    )?);
    let alternate_route: Arc<dyn LanguageModel> = Arc::new(XaiLanguageModel::new(
        alternate_route_resolved,
        second_peer.client.clone(),
    )?);
    let checked_at = now_timestamp()?;
    let authority = Arc::clone(&first)
        .revalidate_runtime_authority(checked_at)
        .await?;
    Arc::clone(&first)
        .revalidate_runtime_authority(Timestamp::new(
            catalog.observed_at().seconds().saturating_add(31 * 60),
            0,
        )?)
        .await?;
    let other_model_authority = alternate_model
        .revalidate_runtime_authority(checked_at)
        .await?;
    let other_route_authority = alternate_route
        .revalidate_runtime_authority(checked_at)
        .await?;
    authority.validate(&first_descriptor, checked_at)?;
    assert_eq!(authority.credential_generation(), 1);
    assert_eq!(authority.observed_at(), checked_at);
    assert_eq!(
        authority.valid_until(),
        Some(Timestamp::new(
            credential_expires_at - ACCESS_REFRESH_LEAD_SECONDS,
            0,
        )?)
    );
    assert_eq!(
        authority.credential_authority_binding(),
        other_model_authority.credential_authority_binding(),
        "the stable route credential authority must not depend on the model descriptor"
    );
    assert_ne!(
        authority.execution_authority_binding()?,
        other_model_authority.execution_authority_binding()?,
        "complete execution authority must still bind the exact model descriptor"
    );
    assert_ne!(
        authority.credential_authority_binding(),
        other_route_authority.credential_authority_binding(),
        "the same selected account must not make authority portable across routes"
    );
    let rendered = format!("{:?}\n{}", authority, serde_json::to_string(&authority)?);
    for forbidden in [
        "runtime-authority-account",
        "runtime-authority-access-token",
        "runtime-authority-refresh-token",
        "selection-epoch-one-0123456789abcdefghijklm",
    ] {
        assert!(
            !rendered.contains(forbidden),
            "durable runtime authority exposed selected-account material"
        );
    }
    assert_eq!(auth.requests.lock().await.len(), 1);
    auth.shutdown().await?;
    first_peer.shutdown().await?;
    second_peer.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_authority_revalidation_rejects_catalog_receipt_and_generation_drift() -> TestResult
{
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let manager = manager(&temp, &auth)?;
    install_stored(
        &manager,
        "runtime-drift-account",
        "runtime-drift-access-one",
        "runtime-drift-refresh-one",
        system_unix_seconds()? + 3_600,
    )?;
    let catalog = admit_catalog(&manager, &auth).await?;
    let inference = mock_peer().await?;
    let route =
        manager.test_subscription_route(crate::XaiOrigin::new(inference.origin.clone())?)?;
    let native_authority = route
        .credential_authority()
        .ok_or_else(|| std::io::Error::other("missing native authority"))?
        .clone();
    // The receipt fence is pinned on the route itself: the model would renew
    // a stale receipt (covered by the renewal tests), the route never does.
    let receipt_route = route.clone();
    let resolved = XaiResolvedModel::new(
        route,
        XaiModel::new(ModelId::new(XAI_GROK_PRIMARY_CODING_MODEL)?, None),
        GrokCodingProfile::pinned_compatibility(),
        catalog.observed_at(),
    )?;
    let descriptor = resolved.descriptor().clone();
    let receipt_model = ModelId::new(XAI_GROK_PRIMARY_CODING_MODEL)?;
    let receipt_revalidation = |checked_at| {
        receipt_route.revalidate_runtime_authority(
            &descriptor,
            &receipt_model,
            descriptor.profile(),
            checked_at,
        )
    };
    let model: Arc<dyn LanguageModel> =
        Arc::new(XaiLanguageModel::new(resolved, inference.client.clone())?);
    let checked_at = now_timestamp()?;
    Arc::clone(&model)
        .revalidate_runtime_authority(checked_at)
        .await?;
    assert!(matches!(
        Arc::clone(&model)
            .revalidate_runtime_authority(Timestamp::new(
                catalog.observed_at().seconds().saturating_sub(1),
                0,
            )?)
            .await,
        Err(RuntimeModelAuthorityError::Stale)
    ));

    let original_catalog = {
        let mut current = manager
            .core
            .catalog
            .lock()
            .map_err(|_| std::io::Error::other("catalog lock poisoned"))?;
        let original = required(current.clone(), "runtime authority catalog")?;
        let mut drifted = original.clone();
        let entry = drifted
            .public
            .entries
            .first_mut()
            .ok_or_else(|| std::io::Error::other("runtime authority catalog was empty"))?;
        entry.diagnostics = XaiCatalogDiagnosticFacts {
            reported_context_length: NonZeroU32::new(
                entry.diagnostics.reported_context_length().get() + 1,
            )
            .ok_or_else(|| std::io::Error::other("diagnostic mutation became zero"))?,
        };
        *current = Some(drifted);
        original
    };
    assert!(matches!(
        receipt_revalidation(checked_at),
        Err(RuntimeModelAuthorityError::Stale)
    ));
    {
        let mut current = manager
            .core
            .catalog
            .lock()
            .map_err(|_| std::io::Error::other("catalog lock poisoned"))?;
        let mut moved_window = original_catalog.clone();
        moved_window.public.expires_at = Timestamp::new(
            original_catalog.public.expires_at.seconds(),
            original_catalog.public.expires_at.nanoseconds() + 1,
        )?;
        *current = Some(moved_window);
    }
    // The catalog window is in the admission witness, not just a freshness
    // hint: the old model cannot authorize a newer receipt even if its rows,
    // account, selection epoch and credential generation are unchanged.
    assert!(matches!(
        receipt_revalidation(checked_at),
        Err(RuntimeModelAuthorityError::Stale)
    ));
    *manager
        .core
        .catalog
        .lock()
        .map_err(|_| std::io::Error::other("catalog lock poisoned"))? = Some(original_catalog);

    rotate_selected_generation(
        &manager,
        "runtime-drift-account",
        "runtime-drift-access-two",
        "runtime-drift-refresh-two",
    )?;
    assert!(matches!(
        native_authority.revalidate_runtime_authority(checked_at),
        Err(RuntimeModelAuthorityError::Stale)
    ));
    assert!(matches!(
        receipt_revalidation(checked_at),
        Err(RuntimeModelAuthorityError::Stale)
    ));
    assert!(inference.requests.lock().await.is_empty());
    assert_eq!(auth.requests.lock().await.len(), 1);
    auth.shutdown().await?;
    inference.shutdown().await?;
    Ok(())
}

#[test]
fn only_release_enrolled_models_receive_supergrok_capabilities() {
    assert!(is_subscription_coding_model_id(
        XAI_GROK_PRIMARY_CODING_MODEL
    ));
    assert!(is_subscription_coding_model_id(XAI_GROK_46_CODING_MODEL));
    assert!(!is_subscription_coding_model_id(XAI_GROK_BUILD_MODEL));
    for arbitrary in [
        "grok-4.3",
        "grok-4.20-0309-reasoning",
        "grok-build-latest",
        "grok-imagine-image",
        "grok-imagine-video",
        "not-grok",
    ] {
        assert!(!is_subscription_coding_model_id(arbitrary));
        assert!(!subscription_model_supports_reasoning(arbitrary));
    }
    assert!(subscription_model_supports_reasoning(
        XAI_GROK_PRIMARY_CODING_MODEL
    ));
    assert!(subscription_model_supports_reasoning(
        XAI_GROK_46_CODING_MODEL
    ));
    assert!(!subscription_model_supports_reasoning(XAI_GROK_BUILD_MODEL));
    assert_eq!(
        subscription_model_reasoning_effort_options(XAI_GROK_PRIMARY_CODING_MODEL),
        &[
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High
        ]
    );
    assert_eq!(
        subscription_model_reasoning_effort_options(XAI_GROK_46_CODING_MODEL),
        &[
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::XHigh
        ]
    );
    assert_eq!(
        subscription_model_reasoning_effort_options(XAI_GROK_BUILD_MODEL),
        []
    );
}

#[test]
fn grok_47_requires_its_exact_catalog_identity_and_does_not_admit_fast_aliases() -> TestResult {
    assert!(is_subscription_coding_model_id(XAI_GROK_47_CODING_MODEL));
    assert_eq!(
        subscription_model_reasoning_effort_options(XAI_GROK_47_CODING_MODEL),
        &[
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::XHigh
        ]
    );
    assert_eq!(
        model_catalog_source_revision(XAI_GROK_47_CODING_MODEL),
        XAI_GROK_47_ENROLLMENT_REVISION
    );
    assert!(is_subscription_coding_model_id(
        XAI_GROK_47_FAST_CODING_MODEL
    ));
    for unreviewed in ["grok-4.7-latest", "grok-4.7-build"] {
        assert!(!is_subscription_coding_model_id(unreviewed));
    }
    let catalog: CatalogResponse = serde_json::from_value(json!({"data": [
        {"id": "grok-4.7", "model": "grok-4.7", "contextWindow": 500_000},
        {"id": "grok-4.7-build-fast", "model": "grok-4.7-build-fast", "contextWindow": 500_000}
    ]}))?;
    let entries = catalog.admitted_coding_entries()?;
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].model().as_str(), "grok-4.7");
    assert_eq!(entries[1].model().as_str(), "grok-4.7-build-fast");
    assert_eq!(
        entries[0].diagnostics().reported_context_length().get(),
        500_000
    );
    let alias: CatalogResponse = serde_json::from_value(json!({"data": [
        {"id": "grok-4.7", "model": "grok-4.7-build-fast", "contextWindow": 500_000}
    ]}))?;
    assert!(matches!(
        alias.admitted_coding_entries(),
        Err(XaiNativeAuthError::CatalogUnavailable)
    ));
    Ok(())
}

#[test]
fn catalog_admits_only_release_enrolled_rows() -> TestResult {
    let catalog: CatalogResponse = serde_json::from_value(json!({
        "data":[
            {
                "model":XAI_GROK_PRIMARY_CODING_MODEL,
                "modelId":"ignored-by-model-precedence",
                "id":XAI_GROK_PRIMARY_CODING_MODEL,
                "contextWindow":8192,
                "context_window":1,
                "_meta":{"contextWindow":2,"totalContextTokens":3}
            },
            {
                "model":XAI_GROK_46_CODING_MODEL,
                "id":XAI_GROK_46_CODING_MODEL,
                "contextWindow":500_000
            },
            {"model":"grok-imagine-image"},
            {
                "_meta":{"modelId":XAI_GROK_BUILD_MODEL,"totalContextTokens":4096}
            }
        ]
    }))?;
    let entries = catalog.admitted_coding_entries()?;
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].model().as_str(), XAI_GROK_PRIMARY_CODING_MODEL);
    assert_eq!(
        entries[0].diagnostics().reported_context_length().get(),
        8192
    );
    assert_eq!(entries[1].model().as_str(), XAI_GROK_46_CODING_MODEL);
    assert_eq!(
        entries[1].diagnostics().reported_context_length().get(),
        500_000
    );
    let missing: CatalogResponse = serde_json::from_value(json!({
        "data":[{"model":XAI_GROK_BUILD_MODEL,"contextWindow":4096}]
    }))?;
    assert!(matches!(
        missing.admitted_coding_entries(),
        Err(XaiNativeAuthError::CatalogUnavailable)
    ));
    for data in [
        json!([]),
        json!([{"id":XAI_GROK_BUILD_MODEL,"model":XAI_GROK_PRIMARY_CODING_MODEL}]),
        json!([{"model":XAI_GROK_PRIMARY_CODING_MODEL,"contextWindow":0}]),
        json!([{"model":XAI_GROK_PRIMARY_CODING_MODEL,"contextWindow":4_294_967_296_u64}]),
        json!([
            {"model":XAI_GROK_PRIMARY_CODING_MODEL,"contextWindow":1},
            {"model":XAI_GROK_PRIMARY_CODING_MODEL,"contextWindow":1}
        ]),
        json!([{"model":17,"modelId":false,"id":null,"_meta":{}}]),
    ] {
        let invalid: CatalogResponse = serde_json::from_value(json!({"data":data}))?;
        assert!(matches!(
            invalid.admitted_coding_entries(),
            Err(XaiNativeAuthError::CatalogUnavailable)
        ));
    }
    Ok(())
}

#[test]
fn proxy_catalog_decoder_matches_pinned_field_precedence() -> TestResult {
    for (value, requested, effective, context) in [
        (
            json!({
                "id":"requested", "model":"from-model", "modelId":"from-model-id",
                "contextWindow":1, "context_window":2,
                "_meta":{"model":"from-meta-model","modelId":"from-meta-id",
                         "contextWindow":3,"totalContextTokens":4}
            }),
            "requested",
            "from-model",
            Some(1),
        ),
        (
            json!({
                "id":"requested", "model":false, "modelId":"from-model-id",
                "context_window":2, "_meta":{"contextWindow":3}
            }),
            "requested",
            "from-model-id",
            Some(2),
        ),
        (json!({"id":"from-id"}), "from-id", "from-id", None),
        (
            json!({"_meta":{"model":"from-meta-model","contextWindow":3}}),
            "from-meta-model",
            "from-meta-model",
            Some(3),
        ),
        (
            json!({"_meta":{"modelId":"from-meta-id","totalContextTokens":4}}),
            "from-meta-id",
            "from-meta-id",
            Some(4),
        ),
    ] {
        let decoded = ProxyCatalogModel::decode(&value)
            .ok_or("pinned proxy catalog row unexpectedly rejected")?;
        assert_eq!(decoded.requested, requested);
        assert_eq!(decoded.effective, effective);
        assert_eq!(decoded.context_window, context);
    }
    Ok(())
}

#[test]
fn catalog_subject_must_be_a_validated_auth_identity() {
    assert!(AccountBinding::new("résolver-subject").is_err());
    assert!(AccountBinding::new("resolver-subject\r\nforged: true").is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn catalog_rejects_alias_selection_and_admits_only_exact_conservative_tuple() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let manager = manager(&temp, &auth)?;
    install_stored(
        &manager,
        "catalog-subject",
        "catalog-access",
        "catalog-refresh",
        system_unix_seconds()? + 3_600,
    )?;
    auth.responses
        .send(response(&json!({
            "object":"list",
            "data":[{
                "id":"provider-internal-model",
                "aliases":[XAI_GROK_BUILD_MODEL],
                "object":"model",
                "owned_by":"xai"
            }]
        })))
        .await?;
    assert!(matches!(
        manager.refresh_catalog().await,
        Err(XaiNativeAuthError::CatalogUnavailable)
    ));
    let receipt = admit_catalog(&manager, &auth).await?;
    assert_eq!(receipt.account_binding().as_str(), "catalog-subject");
    assert!(receipt.observed_at() < receipt.expires_at());
    assert_eq!(receipt.entries().len(), 2);
    assert_eq!(
        required(
            receipt.entry(&ModelId::new(XAI_GROK_46_CODING_MODEL)?),
            "4.6 catalog entry",
        )?
        .diagnostics()
        .reported_context_length()
        .get(),
        4096
    );
    assert_eq!(
        required(
            receipt.entry(&ModelId::new(XAI_GROK_PRIMARY_CODING_MODEL)?),
            "primary catalog entry",
        )?
        .diagnostics()
        .reported_context_length()
        .get(),
        500_000
    );
    let inference = mock_peer().await?;
    let route =
        manager.test_subscription_route(crate::XaiOrigin::new(inference.origin.clone())?)?;
    for (model, profile) in [
        (
            XaiModel::new(ModelId::new("grok-build-latest")?, None),
            GrokCodingProfile::pinned_compatibility(),
        ),
        (
            XaiModel::new(ModelId::new(XAI_GROK_PRIMARY_CODING_MODEL)?, None),
            GrokCodingProfile::provisional(),
        ),
    ] {
        assert!(matches!(
            XaiResolvedModel::new(route.clone(), model, profile, now_timestamp()?,),
            Err(crate::XaiModelError::UnadmittedSubscriptionModel)
        ));
    }
    assert!(inference.requests.lock().await.is_empty());

    let exact = XaiResolvedModel::new(
        route,
        XaiModel::new(ModelId::new(XAI_GROK_PRIMARY_CODING_MODEL)?, None),
        GrokCodingProfile::pinned_compatibility(),
        now_timestamp()?,
    )?;
    let capabilities = exact.descriptor().capabilities();
    assert_eq!(capabilities.features().len(), 3);
    for capability in [
        plakous_model::Capability::Streaming,
        plakous_model::Capability::Tools,
        plakous_model::Capability::Reasoning,
    ] {
        let support = required(capabilities.get(capability), "admitted capability")?;
        assert_eq!(support.support(), plakous_model::Support::Native);
        assert_eq!(
            support.provenance().source(),
            plakous_model::FactSource::RouteEvidence
        );
        assert_eq!(
            support.provenance().expires_at(),
            Some(receipt.expires_at())
        );
    }
    assert!(
        capabilities
            .get(plakous_model::Capability::StructuredOutput)
            .is_none()
    );
    assert!(
        capabilities
            .get(plakous_model::Capability::Continuation)
            .is_none()
    );
    assert!(
        capabilities
            .get(plakous_model::Capability::Caching)
            .is_none(),
        "SuperGrok subscription route must not mint prompt-cache capability",
    );
    wait_for_requests(&auth, 2).await?;
    assert_eq!(auth.requests.lock().await.len(), 2);
    auth.shutdown().await?;
    inference.shutdown().await?;
    Ok(())
}

#[test]
fn enrolled_same_served_models_are_never_enrolled_labels() {
    for row in GROK_ENROLLMENTS {
        for served in row.same_served_models {
            assert_ne!(*served, row.model);
            assert!(!is_subscription_coding_model_id(served), "{served}");
        }
    }
    let declared = |model| {
        grok_enrollment(model)
            .map(|row| row.same_served_models)
            .unwrap_or_default()
    };
    assert_eq!(declared(XAI_GROK_47_CODING_MODEL), ["grok-4.7-build"]);
    for model in [
        XAI_GROK_47_FAST_CODING_MODEL,
        XAI_GROK_46_CODING_MODEL,
        XAI_GROK_PRIMARY_CODING_MODEL,
    ] {
        assert!(declared(model).is_empty(), "{model}");
    }
}

/// SuperGrok answers `grok-4.7` as `grok-4.7-build`: the resolved descriptor
/// declares that served id for usage anchoring, while the label itself stays
/// unadmitted and no other enrolled model gains a served-model variant.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grok_47_descriptor_declares_its_served_build_id_for_anchoring() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let manager = manager(&temp, &auth)?;
    install_stored(
        &manager,
        "served-model-subject",
        "served-model-access",
        "served-model-refresh",
        system_unix_seconds()? + 3_600,
    )?;
    auth.responses
        .send(response(&json!({
            "object":"list",
            "data":[
                {"id":XAI_GROK_47_CODING_MODEL,"object":"model","created":1,"owned_by":"xai","contextWindow":256_000},
                {"id":XAI_GROK_46_CODING_MODEL,"object":"model","created":1,"owned_by":"xai","contextWindow":500_000}
            ]
        })))
        .await?;
    manager.refresh_catalog().await?;
    let inference = mock_peer().await?;
    let route =
        manager.test_subscription_route(crate::XaiOrigin::new(inference.origin.clone())?)?;
    let resolve = |model: &str| -> TestResult<XaiResolvedModel> {
        Ok(XaiResolvedModel::new(
            route.clone(),
            XaiModel::new(ModelId::new(model)?, None),
            GrokCodingProfile::pinned_compatibility(),
            now_timestamp()?,
        )?)
    };
    let descriptor = resolve(XAI_GROK_47_CODING_MODEL)?.descriptor().clone();
    assert!(descriptor.context_contract().is_some());
    assert_eq!(
        descriptor
            .same_served_models()
            .iter()
            .map(ModelId::as_str)
            .collect::<Vec<_>>(),
        ["grok-4.7-build"]
    );
    assert!(descriptor.serves_same_model(&ModelId::new("grok-4.7-build")?));
    assert!(descriptor.serves_same_model(&ModelId::new(XAI_GROK_47_CODING_MODEL)?));
    for foreign in ["grok-4.7-latest", "grok-4.7-build-fast", "grok-4.6"] {
        assert!(
            !descriptor.serves_same_model(&ModelId::new(foreign)?),
            "{foreign}"
        );
    }
    assert_eq!(
        resolve(XAI_GROK_46_CODING_MODEL)?
            .descriptor()
            .same_served_models(),
        []
    );
    assert!(resolve("grok-4.7-build").is_err());
    assert!(inference.requests.lock().await.is_empty());
    auth.shutdown().await?;
    inference.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authenticated_catalog_receipt_authorizes_only_reference_client_envelope_input()
-> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let manager = manager(&temp, &auth)?;
    install_stored(
        &manager,
        "context-unavailable-subject",
        "context-unavailable-access",
        "context-unavailable-refresh",
        system_unix_seconds()? + 3_600,
    )?;
    let catalog = admit_catalog(&manager, &auth).await?;
    assert_eq!(
        catalog.entries()[0]
            .diagnostics()
            .reported_context_length()
            .get(),
        500_000
    );
    let inference = mock_peer().await?;
    let route =
        manager.test_subscription_route(crate::XaiOrigin::new(inference.origin.clone())?)?;
    let admitted_model = ModelId::new(XAI_GROK_PRIMARY_CODING_MODEL)?;
    let admitted_row = route
        .native_catalog_admission()
        .and_then(|admission| admission.entry(&admitted_model))
        .ok_or("native route discarded its exact authenticated catalog row")?;
    assert_eq!(
        admitted_row.diagnostics().reported_context_length().get(),
        500_000
    );
    let resolved = XaiResolvedModel::new(
        route,
        XaiModel::new(ModelId::new(XAI_GROK_PRIMARY_CODING_MODEL)?, None),
        GrokCodingProfile::pinned_compatibility(),
        now_timestamp()?,
    )?;
    assert!(resolved.descriptor().context_contract().is_some());
    let descriptor = resolved.descriptor().clone();
    let model = Arc::new(
        XaiLanguageModel::new(resolved, inference.client.clone())?
            .with_subscription_request_gate(subscription_gate(1)?)?,
    );
    let request = model_request("00000000-0000-4000-8000-000000000410")?;
    let prepared_at = request.metadata().created_at();
    let plan = descriptor.validate_request(request, ValidationMode::Strict)?;
    let authority = Arc::clone(&model)
        .revalidate_runtime_authority(prepared_at)
        .await?;
    let _prepared = Arc::clone(&model)
        .prepare_call(plan, ModelPreparationContext::new(authority, prepared_at))?;
    assert!(matches!(
        descriptor
            .context_contract()
            .map(|contract| contract.capacity().evidence_class()),
        Some(ContextEvidenceClass::ReferenceClientEnvelope { .. })
    ));
    assert!(inference.requests.lock().await.is_empty());
    assert_eq!(auth.requests.lock().await.len(), 1);
    auth.shutdown().await?;
    inference.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_account_relogin_epoch_invalidates_captured_route_before_inference() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let manager = manager(&temp, &auth)?;
    install_stored(
        &manager,
        "epoch-subject",
        "epoch-access-one",
        "epoch-refresh-one",
        system_unix_seconds()? + 3_600,
    )?;
    admit_catalog(&manager, &auth).await?;
    let inference = mock_peer().await?;
    let route =
        manager.test_subscription_route(crate::XaiOrigin::new(inference.origin.clone())?)?;
    let original_credential_authority = route
        .identity()
        .ok_or("original relogin route omitted identity")?
        .credential_authority_binding();
    let resolved = XaiResolvedModel::new(
        route,
        XaiModel::new(ModelId::new(XAI_GROK_PRIMARY_CODING_MODEL)?, None),
        GrokCodingProfile::pinned_compatibility(),
        now_timestamp()?,
    )?;
    let model = permitted_subscription_model(resolved, inference.client.clone())?;

    let baseline = required(manager.core.store.load()?, "baseline fence")?;
    let replacement = StoredTokens {
        version: STORED_TOKENS_VERSION,
        issuer: XAI_ISSUER.to_owned(),
        client_id: XAI_PUBLIC_CLIENT_ID.to_owned(),
        subject: "epoch-subject".to_owned(),
        scope: XAI_OAUTH_SCOPE.to_owned(),
        access_token: "epoch-access-two".to_owned(),
        refresh_token: "epoch-refresh-two".to_owned(),
        expires_at: system_unix_seconds()? + 3_600,
        selection_epoch: "selection-epoch-two-0123456789abcdefghijklm".to_owned(),
    };
    let record = credential_record(
        AccountBinding::new("epoch-subject")?,
        baseline.generation().next()?,
        &replacement,
    )?;
    manager
        .core
        .store
        .replace_selected_login(&baseline, record)?;

    assert!(
        prepare_model_call(
            Arc::clone(&model),
            model_request("00000000-0000-4000-8000-000000000451")?,
            None,
        )
        .await
        .is_err(),
        "captured route must reject the new selection epoch"
    );
    assert!(inference.requests.lock().await.is_empty());
    admit_catalog(&manager, &auth).await?;
    let replacement_route =
        manager.test_subscription_route(crate::XaiOrigin::new(inference.origin.clone())?)?;
    assert_eq!(
        replacement_route
            .identity()
            .ok_or("replacement relogin route omitted identity")?
            .credential_authority_binding(),
        original_credential_authority,
        "selection epoch must fence route use without redefining stable credential authority"
    );
    assert_eq!(auth.requests.lock().await.len(), 2);
    auth.shutdown().await?;
    inference.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peer_generation_renews_running_model_like_a_fresh_resolution() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let manager = manager(&temp, &auth)?;
    install_stored(
        &manager,
        "peer-resolve-subject",
        "peer-resolve-access-one",
        "peer-resolve-refresh-one",
        system_unix_seconds()? + 3_600,
    )?;
    let inference = mock_peer().await?;
    admit_catalog(&manager, &auth).await?;
    enqueue_admitted_catalog(&auth).await?;
    let (old, old_evidence) = manager
        .resolve_model_with_evidence(
            XAI_GROK_PRIMARY_CODING_MODEL,
            inference.client.clone(),
            StandardXaiHttpConfig::default(),
        )
        .await?;
    let rotated = rotate_selected_generation(
        &manager,
        "peer-resolve-subject",
        "peer-resolve-access-two",
        "peer-resolve-refresh-two",
    )?;
    // A peer refresh needs no claim here: the running model only re-resolves
    // the exact catalog for the new generation and swaps in a new receipt.
    enqueue_admitted_catalog(&auth).await?;
    let renewed = Arc::clone(&old)
        .revalidate_runtime_authority(now_timestamp()?)
        .await?;
    assert_eq!(renewed.credential_generation(), rotated.generation().get());
    enqueue_admitted_catalog(&auth).await?;
    let (fresh, fresh_evidence) = manager
        .resolve_model_with_evidence(
            XAI_GROK_PRIMARY_CODING_MODEL,
            inference.client.clone(),
            StandardXaiHttpConfig::default(),
        )
        .await?;
    let authority = fresh.revalidate_runtime_authority(now_timestamp()?).await?;
    assert_eq!(
        authority.credential_generation(),
        rotated.generation().get()
    );
    assert_eq!(authority.account_binding(), renewed.account_binding());
    assert_eq!(
        authority.credential_authority_binding(),
        renewed.credential_authority_binding()
    );
    assert_eq!(
        fresh_evidence.account_fingerprint(),
        old_evidence.account_fingerprint()
    );
    assert_eq!(
        fresh_evidence.credential_authority_binding(),
        old_evidence.credential_authority_binding()
    );
    let token_requests = auth
        .requests
        .lock()
        .await
        .iter()
        .filter(|request| String::from_utf8_lossy(request).starts_with("POST /oauth2/token"))
        .count();
    assert_eq!(token_requests, 0, "a peer refresh is not claimed again");
    assert!(inference.requests.lock().await.is_empty());
    auth.shutdown().await?;
    inference.abort_join().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generation_rotation_after_resolve_before_dispatch_rejects_exact_target() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let manager = manager(&temp, &auth)?;
    install_stored(
        &manager,
        "pre-dispatch-rotation-subject",
        "pre-dispatch-access-one",
        "pre-dispatch-refresh-one",
        system_unix_seconds()? + 3_600,
    )?;
    admit_catalog(&manager, &auth).await?;
    let qualified_fence = required(manager.core.store.load()?, "qualified fence")?;
    let qualified = manager.core.read_stored(&qualified_fence)?;

    let inference = mock_peer().await?;
    inference
        .responses
        .send(MockResponse {
            status: "200 OK",
            content_type: "text/event-stream",
            body: success_sse(),
            extra_headers: String::new(),
            disconnect_after_request: false,
            truncate_after: None,
            before_head: None,
            before_body: None,
        })
        .await?;
    let route =
        manager.test_subscription_route(crate::XaiOrigin::new(inference.origin.clone())?)?;
    let resolved = XaiResolvedModel::new(
        route,
        XaiModel::new(ModelId::new(XAI_GROK_PRIMARY_CODING_MODEL)?, None),
        GrokCodingProfile::pinned_compatibility(),
        now_timestamp()?,
    )?;
    let model = renewable_subscription_model(resolved, inference.client.clone())?;
    let prepared = prepare_model_call(
        Arc::clone(&model),
        model_request("00000000-0000-4000-8000-000000000472")?,
        None,
    )
    .await?;

    let rotated_fence = rotate_selected_generation(
        &manager,
        "pre-dispatch-rotation-subject",
        "pre-dispatch-access-two",
        "pre-dispatch-refresh-two",
    )?;
    assert_ne!(rotated_fence, qualified_fence);
    let rotated = manager.core.read_stored(&rotated_fence)?;
    assert_eq!(
        rotated.selection_epoch, qualified.selection_epoch,
        "a refresh-equivalent rotation must preserve the selection epoch"
    );

    // The prep->dispatch race: the agent's dispatch-time check renews the
    // receipt, but the prepared call stays bound to the generation it was
    // prepared with, so it fails closed: no dispatch and no replay.
    enqueue_admitted_catalog(&auth).await?;
    let dispatch_authority = Arc::clone(&model)
        .revalidate_runtime_authority(now_timestamp()?)
        .await?;
    assert_eq!(
        dispatch_authority.credential_generation(),
        rotated_fence.generation().get()
    );
    assert!(
        prepared
            .record()
            .revalidate_selected_authority(
                &dispatch_authority,
                model.descriptor(),
                now_timestamp()?
            )
            .is_err(),
        "a renewed generation must not authorize a call prepared on the old one"
    );
    let error = tokio::time::timeout(
        Duration::from_secs(5),
        complete_prepared_model_round(
            prepared,
            ModelCallContext::new(
                AttemptId::from_str("00000000-0000-4000-8000-000000000472")?,
                plakous_model::CancellationToken::new(),
                None,
            ),
        ),
    )
    .await?
    .err()
    .ok_or_else(|| std::io::Error::other("rotated qualified target dispatched"))?;
    assert_eq!(error.kind(), ModelErrorKind::Authentication);
    assert!(
        inference.requests.lock().await.is_empty(),
        "generation rotation after resolution must precede inference dispatch"
    );
    assert_eq!(auth.requests.lock().await.len(), 2);
    auth.shutdown().await?;
    inference.abort_join().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(
    clippy::too_many_lines,
    reason = "the linear two-round rotation schedule keeps the exact dispatch boundary visible"
)]
async fn generation_rotation_between_tool_rounds_renews_before_later_dispatch() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let manager = manager(&temp, &auth)?;
    install_stored(
        &manager,
        "later-round-rotation-subject",
        "later-round-access-one",
        "later-round-refresh-one",
        system_unix_seconds()? + 3_600,
    )?;
    admit_catalog(&manager, &auth).await?;
    let qualified_fence = required(manager.core.store.load()?, "qualified fence")?;
    let qualified = manager.core.read_stored(&qualified_fence)?;

    let inference = mock_peer().await?;
    enqueue_tool_loop_responses(&inference).await?;
    let route =
        manager.test_subscription_route(crate::XaiOrigin::new(inference.origin.clone())?)?;
    let resolved = XaiResolvedModel::new(
        route,
        XaiModel::new(ModelId::new(XAI_GROK_PRIMARY_CODING_MODEL)?, None),
        GrokCodingProfile::pinned_compatibility(),
        now_timestamp()?,
    )?;
    let model = renewable_subscription_model(resolved, inference.client.clone())?;

    let user = ConversationItem::new(
        EntryId::from_str("00000000-0000-4000-8000-000000000473")?,
        Actor::User,
        vec![ContentBlock::Text(TextBlock::new(
            "read the fixture",
            None,
        )?)],
        Provenance::UserInput,
    )?;
    let first = complete_model_round(
        Arc::clone(&model),
        tool_loop_request(vec![user.clone()], "00000000-0000-4000-8000-000000000474")?,
        "00000000-0000-4000-8000-000000000474",
    )
    .await?;
    let active_family_artifact = promote_responses_family_artifact(&first)?;
    let call = first
        .output()
        .iter()
        .flat_map(ConversationItem::blocks)
        .find_map(|block| match block {
            ContentBlock::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .ok_or_else(|| std::io::Error::other("first round did not request the tool"))?;
    wait_until_request_count(&inference, 1).await?;

    let rotated_fence = rotate_selected_generation(
        &manager,
        "later-round-rotation-subject",
        "later-round-access-two",
        "later-round-refresh-two",
    )?;
    assert_ne!(rotated_fence, qualified_fence);
    let rotated = manager.core.read_stored(&rotated_fence)?;
    assert_eq!(rotated.selection_epoch, qualified.selection_epoch);

    let tool_result = ToolResultBlock::new(
        call.call_id(),
        ToolExecutionOwner::Local,
        ToolResultKind::Success,
        vec![ToolResultPart::Text(TextBlock::new(
            "fixture result",
            None,
        )?)],
        call.correlation().clone(),
    )?;
    let result_item = ConversationItem::new(
        EntryId::from_str("00000000-0000-4000-8000-000000000475")?,
        Actor::Tool,
        vec![ContentBlock::ToolResult(tool_result)],
        Provenance::ToolExecution,
    )?;
    let history = tool_follow_up_view(user, first.output(), result_item)?;
    // Another process refreshed the shared store between rounds. The next
    // round renews the receipt from a fresh exact catalog and dispatches once
    // with the new grant, replaying the first round's encrypted reasoning.
    enqueue_admitted_catalog(&auth).await?;
    complete_model_round_with_active_family_artifact(
        Arc::clone(&model),
        tool_loop_request_from_view(history, "00000000-0000-4000-8000-000000000476")?,
        "00000000-0000-4000-8000-000000000476",
        active_family_artifact,
    )
    .await?;
    wait_for_requests(&inference, 2).await?;
    let requests = inference.requests.lock().await;
    assert_eq!(requests.len(), 2, "each round dispatches exactly once");
    let first_head = String::from_utf8_lossy(&requests[0]).to_ascii_lowercase();
    let second_head = String::from_utf8_lossy(&requests[1]).to_ascii_lowercase();
    assert!(first_head.contains("bearer later-round-access-one"));
    assert!(second_head.contains("bearer later-round-access-two"));
    let second = captured_request_body(&requests[1])?;
    drop(requests);
    assert!(
        second["input"]
            .as_array()
            .is_some_and(|input| input.iter().any(|item| item["type"] == "reasoning"))
    );
    assert_eq!(
        auth.requests.lock().await.len(),
        2,
        "one admission catalog and one renewal catalog; the peer refresh is not claimed again"
    );
    auth.shutdown().await?;
    inference.abort_join().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_lead_mid_turn_renews_receipt_once_and_dispatches_the_new_grant() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let manager = manager(&temp, &auth)?;
    install_stored(
        &manager,
        "lead-subject",
        "lead-access-one",
        "lead-refresh-one",
        system_unix_seconds()? + 3_600,
    )?;
    admit_catalog(&manager, &auth).await?;
    // The running model's own receipt names a grant inside the refresh lead.
    let baseline = required(manager.core.store.load()?, "baseline fence")?;
    let in_lead = stored_record(
        "lead-subject",
        "lead-access-two",
        "lead-refresh-two",
        system_unix_seconds()? + 120,
        baseline.generation().next()?,
    )?;
    let fence = manager
        .core
        .store
        .replace_selected_login(&baseline, in_lead)?;
    required(
        manager
            .core
            .catalog
            .lock()
            .map_err(|_| std::io::Error::other("catalog lock poisoned"))?
            .as_mut(),
        "admitted catalog",
    )?
    .fence = fence;
    let inference = mock_peer().await?;
    inference
        .responses
        .send(MockResponse {
            status: "200 OK",
            content_type: "text/event-stream",
            body: success_sse(),
            extra_headers: String::new(),
            disconnect_after_request: false,
            truncate_after: None,
            before_head: None,
            before_body: None,
        })
        .await?;
    let route =
        manager.test_subscription_route(crate::XaiOrigin::new(inference.origin.clone())?)?;
    let resolved = XaiResolvedModel::new(
        route,
        XaiModel::new(ModelId::new(XAI_GROK_PRIMARY_CODING_MODEL)?, None),
        GrokCodingProfile::pinned_compatibility(),
        now_timestamp()?,
    )?;
    let model = renewable_subscription_model(resolved, inference.client.clone())?;
    auth.responses
        .send(response(&token_response(
            "lead-access-three",
            "lead-refresh-three",
            &id_token("lead-subject", None)?,
        )))
        .await?;
    auth.responses.send(response(&jwks(TEST_KID))).await?;
    enqueue_admitted_catalog(&auth).await?;
    let checked_at = now_timestamp()?;
    let (first, second) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            Arc::clone(&model).revalidate_runtime_authority(checked_at),
            Arc::clone(&model).revalidate_runtime_authority(checked_at)
        )
    })
    .await?;
    // A claimed refresh writes its claim and its completion as generations.
    let refreshed = required(manager.core.store.load()?, "refreshed fence")?;
    assert_eq!(first?.credential_generation(), refreshed.generation().get());
    assert_eq!(
        second?.credential_generation(),
        refreshed.generation().get()
    );
    assert_eq!(refreshed.status(), CredentialStatus::Ready);
    complete_model_round(
        Arc::clone(&model),
        model_request("00000000-0000-4000-8000-000000000481")?,
        "00000000-0000-4000-8000-000000000481",
    )
    .await?;
    wait_for_requests(&inference, 1).await?;
    let requests = inference.requests.lock().await;
    assert_eq!(requests.len(), 1, "the operation dispatches exactly once");
    assert!(
        String::from_utf8_lossy(&requests[0])
            .to_ascii_lowercase()
            .contains("bearer lead-access-three")
    );
    drop(requests);
    let auth_requests = auth.requests.lock().await;
    assert_eq!(
        auth_requests
            .iter()
            .filter(|request| String::from_utf8_lossy(request).starts_with("POST /oauth2/token"))
            .count(),
        1,
        "concurrent revalidations share one claimed refresh"
    );
    assert_eq!(
        auth_requests.len(),
        4,
        "admission catalog, one refresh with its key set, one renewal catalog"
    );
    drop(auth_requests);
    auth.shutdown().await?;
    inference.abort_join().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn renewal_fails_closed_on_changed_model_semantics_or_account_without_dispatch() -> TestResult
{
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let manager = manager(&temp, &auth)?;
    install_stored(
        &manager,
        "changed-subject",
        "changed-access-one",
        "changed-refresh-one",
        system_unix_seconds()? + 3_600,
    )?;
    admit_catalog(&manager, &auth).await?;
    let inference = mock_peer().await?;
    let route =
        manager.test_subscription_route(crate::XaiOrigin::new(inference.origin.clone())?)?;
    let resolved = XaiResolvedModel::new(
        route,
        XaiModel::new(ModelId::new(XAI_GROK_PRIMARY_CODING_MODEL)?, None),
        GrokCodingProfile::pinned_compatibility(),
        now_timestamp()?,
    )?;
    // The release context contract, so the catalog's context window is a
    // semantic fact of the admitted descriptor.
    let model = Arc::new(XaiLanguageModel::new(resolved, inference.client.clone())?);

    rotate_selected_generation(
        &manager,
        "changed-subject",
        "changed-access-two",
        "changed-refresh-two",
    )?;
    // The renewal catalog reports another context window for the model.
    auth.responses
        .send(response(&json!({
            "object":"list",
            "data":[{
                "id":XAI_GROK_PRIMARY_CODING_MODEL,
                "object":"model",
                "created":1,
                "owned_by":"xai",
                "contextWindow":400_000
            }]
        })))
        .await?;
    assert!(matches!(
        Arc::clone(&model)
            .revalidate_runtime_authority(now_timestamp()?)
            .await,
        Err(RuntimeModelAuthorityError::ForeignAuthority)
    ));
    // Another account now holds the selected slot: refused before any wire.
    rotate_selected_generation(&manager, "other-subject", "other-access", "other-refresh")?;
    assert!(matches!(
        Arc::clone(&model)
            .revalidate_runtime_authority(now_timestamp()?)
            .await,
        Err(RuntimeModelAuthorityError::ForeignAuthority)
    ));
    assert!(
        prepare_model_call(
            Arc::clone(&model),
            model_request("00000000-0000-4000-8000-000000000482")?,
            None,
        )
        .await
        .is_err()
    );
    assert!(inference.requests.lock().await.is_empty());
    assert_eq!(
        auth.requests.lock().await.len(),
        2,
        "admission catalog and the one renewal catalog that changed semantics"
    );
    auth.shutdown().await?;
    inference.abort_join().await?;
    Ok(())
}

/// Moves the selected grant into the refresh lead at the next generation and
/// rebinds each manager's admitted catalog to it, as if the running models had
/// been admitted on that grant.
fn put_selected_grant_in_refresh_lead(
    managers: &[&XaiNativeAuthManager],
    subject: &str,
    access: &str,
    refresh: &str,
) -> TestResult<CredentialFence> {
    let store = &managers.first().ok_or("no manager")?.core.store;
    let baseline = required(store.load()?, "baseline fence")?;
    let in_lead = stored_record(
        subject,
        access,
        refresh,
        system_unix_seconds()? + 120,
        baseline.generation().next()?,
    )?;
    let fence = store.replace_selected_login(&baseline, in_lead)?;
    for manager in managers {
        required(
            manager
                .core
                .catalog
                .lock()
                .map_err(|_| std::io::Error::other("catalog lock poisoned"))?
                .as_mut(),
            "admitted catalog",
        )?
        .fence = fence.clone();
    }
    Ok(fence)
}

/// Moves the admitted catalog's observation `seconds` into the past, so a
/// caller can model a clock read taken that long before a renewal finishes.
fn backdate_admitted_catalog(
    manager: &XaiNativeAuthManager,
    seconds: u64,
) -> TestResult<Timestamp> {
    let mut catalog = manager
        .core
        .catalog
        .lock()
        .map_err(|_| std::io::Error::other("catalog lock poisoned"))?;
    let receipt = required(catalog.as_mut(), "admitted catalog")?;
    let observed = receipt.public.observed_at.seconds() - seconds;
    receipt.public.observed_at = Timestamp::new(observed, 0)?;
    receipt.public.expires_at = Timestamp::new(observed + CATALOG_TTL_SECONDS, 0)?;
    Ok(receipt.public.observed_at)
}

fn token_post_count(requests: &[Vec<u8>]) -> usize {
    requests
        .iter()
        .filter(|request| String::from_utf8_lossy(request).starts_with("POST /oauth2/token"))
        .count()
}

/// The driver reads `checked_at` before it awaits revalidation; a renewal's
/// refresh and catalog round trip then routinely ends in a later second. Here
/// the clock reads precede the catalog response by about five seconds, and a
/// waiter queued behind the renewal checked earlier still.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn renewal_answers_checks_read_before_a_slow_round_trip() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let manager = manager(&temp, &auth)?;
    install_stored(
        &manager,
        "latency-subject",
        "latency-access-one",
        "latency-refresh-one",
        system_unix_seconds()? + 3_600,
    )?;
    admit_catalog(&manager, &auth).await?;
    let admitted_at = backdate_admitted_catalog(&manager, 10)?;
    let inference = mock_peer().await?;
    let route =
        manager.test_subscription_route(crate::XaiOrigin::new(inference.origin.clone())?)?;
    let resolved = XaiResolvedModel::new(
        route,
        XaiModel::new(ModelId::new(XAI_GROK_PRIMARY_CODING_MODEL)?, None),
        GrokCodingProfile::pinned_compatibility(),
        now_timestamp()?,
    )?;
    let model = renewable_subscription_model(resolved, inference.client.clone())?;
    let rotated = rotate_selected_generation(
        &manager,
        "latency-subject",
        "latency-access-two",
        "latency-refresh-two",
    )?;
    let renewer_checked_at = Timestamp::new(admitted_at.seconds() + 6, 500_000_000)?;
    let waiter_checked_at = Timestamp::new(admitted_at.seconds() + 4, 250_000_000)?;
    let (catalog, release) = gated_response(&json!({
        "object":"list",
        "data":[
            {"id":XAI_GROK_46_CODING_MODEL,"object":"model","created":1,"owned_by":"xai","contextWindow":4096},
            {"id":XAI_GROK_PRIMARY_CODING_MODEL,"object":"model","created":1,"owned_by":"xai","contextWindow":500_000}
        ]
    }));
    auth.responses.send(catalog).await?;
    let (renewer, waiter, released) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            Arc::clone(&model).revalidate_runtime_authority(renewer_checked_at),
            Arc::clone(&model).revalidate_runtime_authority(waiter_checked_at),
            async {
                // Both revalidations are queued before the catalog answers.
                wait_until_request_count(&auth, 2).await?;
                release
                    .send(())
                    .map_err(|()| std::io::Error::other("catalog gate dropped"))?;
                TestResult::Ok(())
            }
        )
    })
    .await?;
    released?;
    let renewer = renewer?;
    let waiter = waiter?;
    assert_eq!(renewer.credential_generation(), rotated.generation().get());
    assert_eq!(waiter.credential_generation(), rotated.generation().get());
    assert_eq!(renewer.observed_at(), renewer_checked_at);
    assert_eq!(waiter.observed_at(), waiter_checked_at);
    let renewed_observed_at = required(
        manager
            .core
            .catalog
            .lock()
            .map_err(|_| std::io::Error::other("catalog lock poisoned"))?
            .clone(),
        "renewed catalog",
    )?
    .public
    .observed_at;
    assert_eq!(
        renewed_observed_at.seconds(),
        waiter_checked_at.seconds(),
        "the renewed receipt claims observation no later than the earliest waiting check"
    );
    assert!(renewed_observed_at > admitted_at);
    assert_eq!(auth.requests.lock().await.len(), 2);
    auth.shutdown().await?;
    inference.abort_join().await?;
    Ok(())
}

/// Past the admitted catalog window renewal cannot succeed, so it spends
/// neither a claimed refresh nor an authenticated catalog request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn past_admitted_window_renews_nothing_and_fetches_no_catalog() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let manager = manager(&temp, &auth)?;
    install_stored(
        &manager,
        "window-subject",
        "window-access-one",
        "window-refresh-one",
        system_unix_seconds()? + 3_600,
    )?;
    let inference = mock_peer().await?;
    admit_catalog(&manager, &auth).await?;
    enqueue_admitted_catalog(&auth).await?;
    let (model, _) = manager
        .resolve_model_with_evidence(
            XAI_GROK_PRIMARY_CODING_MODEL,
            inference.client.clone(),
            StandardXaiHttpConfig::default(),
        )
        .await?;
    let admitted_until = required(
        manager
            .core
            .catalog
            .lock()
            .map_err(|_| std::io::Error::other("catalog lock poisoned"))?
            .clone(),
        "admitted catalog",
    )?
    .public
    .expires_at;
    let before = auth.requests.lock().await.len();
    // Served only if the model wrongly fetches one.
    enqueue_admitted_catalog(&auth).await?;
    let past = Timestamp::new(admitted_until.seconds() + 1, 0)?;
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        Arc::clone(&model).revalidate_runtime_authority(past),
    )
    .await?;
    assert!(matches!(result, Err(RuntimeModelAuthorityError::Stale)));
    assert_eq!(auth.requests.lock().await.len(), before);
    // The decoy catalog stays queued; do not wait for it to be served.
    auth.abort_join().await?;
    inference.abort_join().await?;
    Ok(())
}

/// Two running models on separate managers over one store renew at once: the
/// store claim lets one refresh reach the issuer and the other adopts it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_managers_renew_concurrently_with_one_claimed_refresh() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let first_manager = manager(&temp, &auth)?;
    // A second process's manager over the same credential file.
    let second_manager = XaiNativeAuthManager::open_test_slot(
        temp.path().join("credentials").join("xai.json"),
        auth.client.clone(),
        &auth.origin,
    )?;
    install_stored(
        &first_manager,
        "pair-subject",
        "pair-access-one",
        "pair-refresh-one",
        system_unix_seconds()? + 3_600,
    )?;
    admit_catalog(&first_manager, &auth).await?;
    admit_catalog(&second_manager, &auth).await?;
    put_selected_grant_in_refresh_lead(
        &[&first_manager, &second_manager],
        "pair-subject",
        "pair-access-two",
        "pair-refresh-two",
    )?;
    let inference = mock_peer().await?;
    let mut models = Vec::new();
    for manager in [&first_manager, &second_manager] {
        let route =
            manager.test_subscription_route(crate::XaiOrigin::new(inference.origin.clone())?)?;
        let resolved = XaiResolvedModel::new(
            route,
            XaiModel::new(ModelId::new(XAI_GROK_PRIMARY_CODING_MODEL)?, None),
            GrokCodingProfile::pinned_compatibility(),
            now_timestamp()?,
        )?;
        models.push(renewable_subscription_model(
            resolved,
            inference.client.clone(),
        )?);
    }
    auth.responses
        .send(response(&token_response(
            "pair-access-three",
            "pair-refresh-three",
            &id_token("pair-subject", None)?,
        )))
        .await?;
    auth.responses.send(response(&jwks(TEST_KID))).await?;
    enqueue_admitted_catalog(&auth).await?;
    enqueue_admitted_catalog(&auth).await?;
    let checked_at = now_timestamp()?;
    let (first, second) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            Arc::clone(&models[0]).revalidate_runtime_authority(checked_at),
            Arc::clone(&models[1]).revalidate_runtime_authority(checked_at)
        )
    })
    .await?;
    let current = required(first_manager.core.store.load()?, "refreshed fence")?;
    assert_eq!(current.status(), CredentialStatus::Ready);
    assert_eq!(first?.credential_generation(), current.generation().get());
    assert_eq!(second?.credential_generation(), current.generation().get());
    let requests = auth.requests.lock().await;
    assert_eq!(
        token_post_count(&requests),
        1,
        "one refresh reaches the issuer"
    );
    assert_eq!(
        requests.len(),
        6,
        "two admission catalogs, one refresh with its key set, two renewal catalogs"
    );
    drop(requests);
    assert!(inference.requests.lock().await.is_empty());
    auth.shutdown().await?;
    inference.abort_join().await?;
    Ok(())
}

/// A renewal that starts while a dispatch's post-401 forced refresh holds the
/// claim waits for it and installs a receipt for the generation it produced;
/// the next dispatch carries that grant and nothing refreshes twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn renewal_racing_the_rejected_grant_refresh_adopts_its_generation() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let manager = manager(&temp, &auth)?;
    install_stored(
        &manager,
        "rejected-subject",
        "rejected-access-one",
        "rejected-refresh-one",
        system_unix_seconds()? + 3_600,
    )?;
    admit_catalog(&manager, &auth).await?;
    let inference = mock_peer().await?;
    inference
        .responses
        .send(MockResponse {
            status: "200 OK",
            content_type: "text/event-stream",
            body: success_sse(),
            extra_headers: String::new(),
            disconnect_after_request: false,
            truncate_after: None,
            before_head: None,
            before_body: None,
        })
        .await?;
    let route =
        manager.test_subscription_route(crate::XaiOrigin::new(inference.origin.clone())?)?;
    let forced = Arc::clone(required(route.credential_authority(), "route authority")?);
    let resolved = XaiResolvedModel::new(
        route,
        XaiModel::new(ModelId::new(XAI_GROK_PRIMARY_CODING_MODEL)?, None),
        GrokCodingProfile::pinned_compatibility(),
        now_timestamp()?,
    )?;
    let model = renewable_subscription_model(resolved, inference.client.clone())?;
    let (token, release_token) = gated_response(&token_response(
        "rejected-access-two",
        "rejected-refresh-two",
        &id_token("rejected-subject", None)?,
    ));
    auth.responses.send(token).await?;
    auth.responses.send(response(&jwks(TEST_KID))).await?;
    enqueue_admitted_catalog(&auth).await?;
    let rejected = tokio::spawn(async move {
        crate::XaiSubscriptionCredentialAuthority::refresh_after_rejection(&*forced).await
    });
    wait_until_request_count(&auth, 2).await?;
    let renewal = tokio::spawn(Arc::clone(&model).revalidate_runtime_authority(now_timestamp()?));
    release_token
        .send(())
        .map_err(|()| std::io::Error::other("token gate dropped"))?;
    tokio::time::timeout(Duration::from_secs(5), rejected).await???;
    let renewed = tokio::time::timeout(Duration::from_secs(5), renewal).await???;
    let current = required(manager.core.store.load()?, "forced fence")?;
    assert_eq!(renewed.credential_generation(), current.generation().get());
    complete_model_round(
        Arc::clone(&model),
        model_request("00000000-0000-4000-8000-000000000483")?,
        "00000000-0000-4000-8000-000000000483",
    )
    .await?;
    wait_for_requests(&inference, 1).await?;
    assert!(
        String::from_utf8_lossy(&inference.requests.lock().await[0])
            .to_ascii_lowercase()
            .contains("bearer rejected-access-two")
    );
    assert_eq!(token_post_count(&auth.requests.lock().await), 1);
    auth.shutdown().await?;
    inference.abort_join().await?;
    Ok(())
}

/// A renewal dropped while its claimed refresh is on the wire restores the
/// claim and installs nothing; the next revalidation renews from scratch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_renewal_restores_the_claim_and_installs_nothing() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let manager = manager(&temp, &auth)?;
    install_stored(
        &manager,
        "cancel-renew-subject",
        "cancel-renew-access-one",
        "cancel-renew-refresh-one",
        system_unix_seconds()? + 3_600,
    )?;
    admit_catalog(&manager, &auth).await?;
    let in_lead = put_selected_grant_in_refresh_lead(
        &[&manager],
        "cancel-renew-subject",
        "cancel-renew-access-two",
        "cancel-renew-refresh-two",
    )?;
    let inference = mock_peer().await?;
    let route =
        manager.test_subscription_route(crate::XaiOrigin::new(inference.origin.clone())?)?;
    let resolved = XaiResolvedModel::new(
        route,
        XaiModel::new(ModelId::new(XAI_GROK_PRIMARY_CODING_MODEL)?, None),
        GrokCodingProfile::pinned_compatibility(),
        now_timestamp()?,
    )?;
    let model = renewable_subscription_model(resolved, inference.client.clone())?;
    let (token, release_token) = gated_response(&token_response(
        "cancel-renew-access-three",
        "cancel-renew-refresh-three",
        &id_token("cancel-renew-subject", None)?,
    ));
    auth.responses.send(token).await?;
    let renewal = tokio::spawn(Arc::clone(&model).revalidate_runtime_authority(now_timestamp()?));
    wait_until_request_count(&auth, 2).await?;
    renewal.abort();
    let joined = tokio::time::timeout(Duration::from_secs(1), renewal).await?;
    assert!(joined.is_err_and(|error| error.is_cancelled()));
    drop(release_token);
    let restored = required(manager.core.store.load()?, "restored fence")?;
    assert_eq!(restored.status(), CredentialStatus::Ready);
    assert_eq!(
        manager.core.read_stored(&restored)?.access_token,
        "cancel-renew-access-two",
        "the abandoned refresh wrote no new grant"
    );
    assert_eq!(
        auth.requests.lock().await.len(),
        2,
        "no catalog was fetched for the abandoned renewal"
    );
    // A renewal installs only a receipt built from a fetched catalog; none was
    // fetched and the manager still holds the in-lead receipt.
    let cached = required(
        manager
            .core
            .catalog
            .lock()
            .map_err(|_| std::io::Error::other("catalog lock poisoned"))?
            .clone(),
        "admitted catalog",
    )?;
    assert_eq!(cached.fence.generation(), in_lead.generation());
    assert!(inference.requests.lock().await.is_empty());
    // The gated token response was abandoned with its caller.
    auth.abort_join().await?;
    inference.abort_join().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_profile_blocks_remote_revoke_but_local_logout_remains_available() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let peer = mock_peer().await?;
    let manager = manager(&temp, &peer)?;
    install_stored(
        &manager,
        "expiry-subject",
        "expiry-access",
        "expiry-refresh",
        system_unix_seconds()? + 3_600,
    )?;
    assert!(matches!(
        manager.core.revoke_at(COMPATIBILITY_EXPIRES_UNIX).await,
        Err(XaiNativeAuthError::CompatibilityInactive)
    ));
    assert!(peer.requests.lock().await.is_empty());
    assert_eq!(manager.logout()?.get(), 2);
    assert_eq!(
        required(manager.status()?, "local expiry tombstone")?.status(),
        CredentialStatus::Revoked
    );
    peer.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_explicit_slot_status_and_tombstone_ignore_expiry_and_kill_without_network()
-> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let path = private_store(&temp)?;
    let peer = mock_peer().await?;
    let manager =
        XaiNativeAuthManager::open_test_slot(path.clone(), peer.client.clone(), &peer.origin)?;
    install_stored(
        &manager,
        "offline-subject",
        "offline-access",
        "offline-refresh",
        system_unix_seconds()? + 3_600,
    )?;
    drop(manager);

    let offline =
        XaiOfflineSelectedSlot::open_explicit(path, CredentialSlotId::new("test-selected")?)?;
    assert_eq!(offline.status()?, XaiOfflineCredentialStatus::Ready);
    assert!(matches!(
        ensure_compatibility(COMPATIBILITY_EXPIRES_UNIX),
        Err(XaiNativeAuthError::CompatibilityInactive)
    ));
    assert!(matches!(
        ensure_compatibility_state(COMPATIBILITY_REVIEWED_UNIX, true),
        Err(XaiNativeAuthError::CompatibilityInactive)
    ));
    assert_eq!(offline.logout_local()?, XaiOfflineCredentialStatus::Revoked);
    assert_eq!(offline.status()?, XaiOfflineCredentialStatus::Revoked);
    assert!(!format!("{offline:?}").contains("offline-subject"));
    assert!(peer.requests.lock().await.is_empty());
    peer.shutdown().await?;

    let empty = tempdir()?;
    let empty_path = private_store(&empty)?;
    let empty_offline =
        XaiOfflineSelectedSlot::open_explicit(empty_path, CredentialSlotId::new("test-selected")?)?;
    assert_eq!(
        empty_offline.logout_local()?,
        XaiOfflineCredentialStatus::SignedOut
    );
    assert_eq!(
        empty_offline.status()?,
        XaiOfflineCredentialStatus::SignedOut
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_subscription_uses_direct_responses_and_retains_grant_to_head() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let manager = manager(&temp, &auth)?;
    install_stored(
        &manager,
        "inference-subject",
        "subscription-access",
        "refresh",
        system_unix_seconds()? + 3600,
    )?;
    admit_catalog(&manager, &auth).await?;
    let inference = mock_peer().await?;
    let body = success_sse();
    inference
        .responses
        .send(MockResponse {
            status: "200 OK",
            content_type: "text/event-stream",
            body,
            extra_headers: String::new(),
            disconnect_after_request: false,
            truncate_after: None,
            before_head: None,
            before_body: None,
        })
        .await?;
    let route =
        manager.test_subscription_route(crate::XaiOrigin::new(inference.origin.clone())?)?;
    let resolved = XaiResolvedModel::new(
        route,
        XaiModel::new(ModelId::new(XAI_GROK_PRIMARY_CODING_MODEL)?, None),
        GrokCodingProfile::pinned_compatibility(),
        now_timestamp()?,
    )?;
    let request_gate = subscription_gate(1)?;
    let model =
        subscription_model_with_gate(resolved, inference.client.clone(), request_gate.clone())?;
    let request = model_request("00000000-0000-4000-8000-000000000401")?;
    let prepared = prepare_model_call(Arc::clone(&model), request, None).await?;
    let mut stream = tokio::time::timeout(
        Duration::from_secs(5),
        prepared.start(ModelCallContext::new(
            AttemptId::from_str("00000000-0000-4000-8000-000000000401")?,
            plakous_model::CancellationToken::new(),
            None,
        )),
    )
    .await??;
    let completed = tokio::time::timeout(Duration::from_secs(5), async {
        let mut completed = false;
        while let Some(event) = stream.next().await {
            if matches!(event?, ModelEvent::ResponseCompleted(_)) {
                completed = true;
            }
        }
        Ok::<_, ModelError>(completed)
    })
    .await??;
    assert!(completed);
    wait_for_requests(&inference, 1).await?;
    let requests = inference.requests.lock().await;
    assert_eq!(requests.len(), 1, "one admitted turn has one TLS request");
    let request = &requests[0];
    let body_offset = request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| std::io::Error::other("captured request omitted HTTP head boundary"))?
        + 4;
    let wire = String::from_utf8_lossy(&request[..body_offset]).to_ascii_lowercase();
    assert!(wire.starts_with("post /v1/responses http/1.1"));
    assert_eq!(wire.matches("authorization:").count(), 1);
    assert!(wire.contains("authorization: bearer subscription-access"));
    assert!(wire.contains("accept: text/event-stream"));
    assert!(wire.contains("content-type: application/json"));
    assert!(wire.contains("user-agent: plak/"));
    assert!(wire.contains("x-xai-token-auth: xai-grok-cli"));
    assert!(wire.contains("x-authenticateresponse: authenticate-response"));
    assert!(wire.contains("x-grok-client-version: 1.0.46"));
    assert!(wire.contains("x-grok-client-identifier: plakous"));
    assert!(wire.contains("x-grok-client-mode: interactive"));
    assert!(wire.contains(&format!(
        "x-grok-conv-id: {}",
        test_cache_affinity()?.identity()
    )));
    assert!(!wire.contains("x-userid"));
    assert!(!wire.contains("x-email"));
    assert!(!wire.contains("xai_api_key"));
    assert!(!wire.contains("opencode/"));
    let body: serde_json::Value = serde_json::from_slice(&request[body_offset..])?;
    assert_eq!(
        body,
        json!({
            "model": XAI_GROK_PRIMARY_CODING_MODEL,
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{"type":"input_text","text":"native subscription"}]
            }],
            "max_output_tokens": 64,
            "store": false,
            "include": ["reasoning.encrypted_content"],
            "stream": true
        })
    );
    let inference_gate = request_gate
        .snapshot()?
        .class(SubscriptionRequestClass::Inference);
    assert_eq!(inference_gate.reserved(), 1);
    assert_eq!(inference_gate.dispatched(), 1);
    assert_eq!(
        auth.requests.lock().await.len(),
        1,
        "fresh preflight must not repeat authenticated catalog discovery"
    );
    drop(requests);
    auth.shutdown().await?;
    inference.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supergrok_not_started_refunds_inference_permit_and_input() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let manager = manager(&temp, &auth)?;
    install_stored(
        &manager,
        "inference-subject",
        "subscription-access",
        "refresh",
        system_unix_seconds()? + 3600,
    )?;
    admit_catalog(&manager, &auth).await?;
    let inference = mock_peer().await?;
    let body = success_sse();
    inference
        .responses
        .send(MockResponse {
            status: "200 OK",
            content_type: "text/event-stream",
            body,
            extra_headers: String::new(),
            disconnect_after_request: false,
            truncate_after: None,
            before_head: None,
            before_body: None,
        })
        .await?;
    let route =
        manager.test_subscription_route(crate::XaiOrigin::new(inference.origin.clone())?)?;
    let resolved = XaiResolvedModel::new(
        route,
        XaiModel::new(ModelId::new(XAI_GROK_PRIMARY_CODING_MODEL)?, None),
        GrokCodingProfile::pinned_compatibility(),
        now_timestamp()?,
    )?;
    let request_gate = subscription_gate(1)?;
    let model = subscription_model_with_gate(
        resolved,
        inference
            .client
            .clone()
            .with_faults(Some(plakous_http::FaultPlan::parse("fault=connect,nth=1")?)),
        request_gate.clone(),
    )?;
    let request = model_request("00000000-0000-4000-8000-000000000401")?;
    let prepared = prepare_model_call(Arc::clone(&model), request, None).await?;
    let mut stream = tokio::time::timeout(
        Duration::from_secs(5),
        prepared.start(ModelCallContext::new(
            AttemptId::from_str("00000000-0000-4000-8000-000000000401")?,
            plakous_model::CancellationToken::new(),
            None,
        )),
    )
    .await??;
    let completed = tokio::time::timeout(Duration::from_secs(5), async {
        let mut completed = false;
        while let Some(event) = stream.next().await {
            if matches!(event?, ModelEvent::ResponseCompleted(_)) {
                completed = true;
            }
        }
        Ok::<_, ModelError>(completed)
    })
    .await??;
    assert!(completed);
    wait_for_requests(&inference, 1).await?;
    assert_eq!(
        inference.requests.lock().await.len(),
        1,
        "one paid dispatch"
    );
    let inference_gate = request_gate
        .snapshot()?
        .class(SubscriptionRequestClass::Inference);
    assert_eq!(inference_gate.reserved(), 1);
    assert_eq!(inference_gate.dispatched(), 1);
    assert!(inference_gate.input_token_upper_bound() > 0);
    assert_eq!(
        auth.requests.lock().await.len(),
        1,
        "fresh preflight must not repeat authenticated catalog discovery"
    );
    auth.shutdown().await?;
    inference.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn final_native_grant_rechecks_exact_catalog_expiry() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let manager = manager(&temp, &auth)?;
    install_stored(
        &manager,
        "grant-expiry-subject",
        "subscription-access",
        "refresh",
        system_unix_seconds()? + 3_600,
    )?;
    admit_catalog(&manager, &auth).await?;
    let fence = manager
        .core
        .store
        .load()?
        .ok_or(XaiNativeAuthError::NotLoggedIn)?;
    let stored = manager.core.read_stored(&fence)?;
    let mut receipt =
        manager
            .core
            .active_catalog_receipt(&fence, &stored, system_unix_seconds()?)?;
    let now = system_unix_seconds()?;
    manager
        .core
        .catalog
        .lock()
        .map_err(|_| XaiNativeAuthError::CatalogUnavailable)?
        .as_mut()
        .ok_or(XaiNativeAuthError::CatalogUnavailable)?
        .public
        .expires_at = Timestamp::new(now, 0)?;

    receipt.public.expires_at = Timestamp::new(now, 0)?;
    let Err(error) = manager.core.acquire_grant(&receipt) else {
        return Err("expired catalog unexpectedly released a native grant".into());
    };
    assert_eq!(error, crate::XaiAuthError::BindingMismatch);
    assert_eq!(auth.requests.lock().await.len(), 1);
    auth.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_subscription_grant_gap_rejects_receipt_replacement_and_expiry_before_wire()
-> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    for (offset, mutation) in [
        CatalogGapMutation::FreshSameFenceReplacement,
        CatalogGapMutation::Expire,
    ]
    .into_iter()
    .enumerate()
    {
        let temp = tempdir()?;
        let auth = mock_peer().await?;
        let manager = manager(&temp, &auth)?;
        install_stored(
            &manager,
            "catalog-gap-subject",
            "catalog-gap-access",
            "catalog-gap-refresh",
            system_unix_seconds()? + 3_600,
        )?;
        admit_catalog(&manager, &auth).await?;
        let captured = manager
            .core
            .catalog
            .lock()
            .map_err(|_| XaiNativeAuthError::CatalogUnavailable)?
            .clone()
            .ok_or(XaiNativeAuthError::CatalogUnavailable)?;
        let inference = mock_peer().await?;
        let route =
            manager.test_subscription_route(crate::XaiOrigin::new(inference.origin.clone())?)?;
        let resolved = XaiResolvedModel::new(
            route,
            XaiModel::new(ModelId::new(XAI_GROK_PRIMARY_CODING_MODEL)?, None),
            GrokCodingProfile::pinned_compatibility(),
            now_timestamp()?,
        )?;
        let inner = subscription_gate(1)?;
        let gate: Arc<dyn SubscriptionRequestGate> = Arc::new(CatalogGapGate {
            inner: Arc::clone(&inner),
            core: Arc::clone(&manager.core),
            mutation,
        });
        let model = subscription_model_with_gate(resolved, inference.client.clone(), gate)?;
        let attempt = format!("00000000-0000-4000-8000-00000000049{}", offset + 1);
        let erased: Arc<dyn LanguageModel> = model.clone();
        let error = plakous_test_support::run_model_operation(
            erased,
            model.descriptor(),
            model_request(&attempt)?,
            ModelCallContext::new(
                AttemptId::from_str(&attempt)?,
                plakous_model::CancellationToken::new(),
                None,
            ),
        )
        .await
        .err()
        .ok_or("mutated native catalog receipt unexpectedly reached wire")?;

        assert_eq!(error.kind(), ModelErrorKind::Authentication);
        let inference_gate = inner.snapshot()?.class(SubscriptionRequestClass::Inference);
        assert_eq!(inference_gate.reserved(), 1);
        assert_eq!(inference_gate.dispatched(), 1);
        let current = manager
            .core
            .catalog
            .lock()
            .map_err(|_| XaiNativeAuthError::CatalogUnavailable)?
            .clone()
            .ok_or(XaiNativeAuthError::CatalogUnavailable)?;
        assert_eq!(current.fence, captured.fence);
        assert_eq!(current.selection_epoch, captured.selection_epoch);
        assert_eq!(current.authority_id, captured.authority_id);
        assert!(current != captured);
        assert!(
            inference.requests.lock().await.is_empty(),
            "post-grant catalog mutation crossed the inference wire"
        );
        assert_eq!(auth.requests.lock().await.len(), 1);
        auth.shutdown().await?;
        inference.abort_join().await?;
    }
    Ok(())
}

async fn assert_subscription_gate_fails_before_inference_network(
    request_gate: Option<Arc<dyn SubscriptionRequestGate>>,
    attempt_id: &str,
) -> TestResult {
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let manager = manager(&temp, &auth)?;
    install_stored(
        &manager,
        "gate-boundary-subject",
        "gate-boundary-access",
        "gate-boundary-refresh",
        system_unix_seconds()? + 3_600,
    )?;
    admit_catalog(&manager, &auth).await?;
    let inference = mock_peer().await?;
    let route =
        manager.test_subscription_route(crate::XaiOrigin::new(inference.origin.clone())?)?;
    let resolved = XaiResolvedModel::new(
        route,
        XaiModel::new(ModelId::new(XAI_GROK_PRIMARY_CODING_MODEL)?, None),
        GrokCodingProfile::pinned_compatibility(),
        now_timestamp()?,
    )?;
    let model = match request_gate {
        Some(gate) => subscription_model_with_gate(resolved, inference.client.clone(), gate)?,
        None => Arc::new(XaiLanguageModel::new(
            resolved.with_test_context_contract(now_timestamp()?)?,
            inference.client.clone(),
        )?),
    };
    let prepared = prepare_model_call(Arc::clone(&model), model_request(attempt_id)?, None).await?;
    let error = complete_prepared_model_round(
        prepared,
        ModelCallContext::new(
            AttemptId::from_str(attempt_id)?,
            plakous_model::CancellationToken::new(),
            None,
        ),
    )
    .await
    .err()
    .ok_or("subscription inference unexpectedly completed")?;
    assert_eq!(error.kind(), ModelErrorKind::ResourceLimit);
    assert!(
        inference.requests.lock().await.is_empty(),
        "subscription gate failure must precede inference network dispatch"
    );
    assert_eq!(
        auth.requests.lock().await.len(),
        1,
        "fresh refresh must not repeat authenticated catalog discovery"
    );
    auth.shutdown().await?;
    inference.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_and_denied_subscription_gates_fail_after_refresh_without_inference_network()
-> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    assert_subscription_gate_fails_before_inference_network(
        None,
        "00000000-0000-4000-8000-000000000405",
    )
    .await?;

    let denied = subscription_gate(0)?;
    assert_subscription_gate_fails_before_inference_network(
        Some(denied.clone()),
        "00000000-0000-4000-8000-000000000406",
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_refresh_does_not_consume_inference_class() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let mut manager = manager(&temp, &auth)?;
    install_stored(
        &manager,
        "failed-refresh-subject",
        "failed-refresh-access",
        "failed-refresh-token",
        system_unix_seconds()? + 3_600,
    )?;
    admit_catalog(&manager, &auth).await?;
    let request_gate = classified_subscription_gate(1, 0, 1)?;
    manager = manager.with_subscription_request_gate(request_gate.clone())?;
    let inference = mock_peer().await?;
    let route =
        manager.test_subscription_route(crate::XaiOrigin::new(inference.origin.clone())?)?;
    let resolved = XaiResolvedModel::new(
        route,
        XaiModel::new(ModelId::new(XAI_GROK_PRIMARY_CODING_MODEL)?, None),
        GrokCodingProfile::pinned_compatibility(),
        now_timestamp()?,
    )?;
    let model = Arc::new(XaiLanguageModel::new(
        resolved.with_test_context_contract(now_timestamp()?)?,
        inference.client.clone(),
    )?);
    let prepared = prepare_model_call(
        Arc::clone(&model),
        model_request("00000000-0000-4000-8000-000000000407")?,
        None,
    )
    .await?;
    manager.logout()?;
    let error = complete_prepared_model_round(
        prepared,
        ModelCallContext::new(
            AttemptId::from_str("00000000-0000-4000-8000-000000000407")?,
            plakous_model::CancellationToken::new(),
            None,
        ),
    )
    .await
    .err()
    .ok_or("logged-out SuperGrok inference unexpectedly completed")?;
    assert_eq!(error.kind(), ModelErrorKind::Authentication);
    let inference_gate = request_gate
        .snapshot()?
        .class(SubscriptionRequestClass::Inference);
    assert_eq!(inference_gate.reserved(), 0);
    assert_eq!(inference_gate.dispatched(), 0);
    assert_eq!(inference_gate.denied(), 0);
    assert!(
        inference.requests.lock().await.is_empty(),
        "failed refresh must not reach inference transport"
    );
    assert_eq!(
        auth.requests.lock().await.len(),
        1,
        "logout must fail refresh locally without a second OAuth request"
    );
    auth.shutdown().await?;
    inference.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_token_refresh_is_charged_before_inference_reservation() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let mut manager = manager(&temp, &auth)?;
    install_stored(
        &manager,
        "skew-refresh-subject",
        "skew-refresh-access",
        "skew-refresh-token",
        system_unix_seconds()? + 3_600,
    )?;
    admit_catalog(&manager, &auth).await?;
    let inner = classified_subscription_gate(1, 0, 1)?;
    let order = Arc::new(std::sync::Mutex::new(Vec::new()));
    let request_gate: Arc<dyn SubscriptionRequestGate> = Arc::new(ClassOrderGate {
        inner: Arc::clone(&inner),
        order: Arc::clone(&order),
    });
    manager = manager.with_subscription_request_gate(Arc::clone(&request_gate))?;
    let inference = mock_peer().await?;
    let route =
        manager.test_subscription_route(crate::XaiOrigin::new(inference.origin.clone())?)?;
    let resolved = XaiResolvedModel::new(
        route,
        XaiModel::new(ModelId::new(XAI_GROK_PRIMARY_CODING_MODEL)?, None),
        GrokCodingProfile::pinned_compatibility(),
        now_timestamp()?,
    )?;
    let model = Arc::new(XaiLanguageModel::new(
        resolved.with_test_context_contract(now_timestamp()?)?,
        inference.client.clone(),
    )?);
    let prepared = prepare_model_call(
        Arc::clone(&model),
        model_request("00000000-0000-4000-8000-000000000408")?,
        None,
    )
    .await?;
    expire_selected_access(
        &manager,
        "skew-refresh-subject",
        "skew-refresh-access",
        "skew-refresh-token",
    )?;
    auth.responses.send(disconnect_after_request()).await?;
    let error = complete_prepared_model_round(
        prepared,
        ModelCallContext::new(
            AttemptId::from_str("00000000-0000-4000-8000-000000000408")?,
            plakous_model::CancellationToken::new(),
            None,
        ),
    )
    .await
    .err()
    .ok_or("expired SuperGrok refresh unexpectedly completed inference")?;
    assert_eq!(error.kind(), ModelErrorKind::Authentication);
    assert_eq!(
        *order.lock().expect("class order"),
        [SubscriptionRequestClass::AuthRefresh]
    );
    let snapshot = inner.snapshot()?;
    let refresh = snapshot.class(SubscriptionRequestClass::AuthRefresh);
    let inference_class = snapshot.class(SubscriptionRequestClass::Inference);
    assert_eq!(refresh.reserved(), 1);
    assert_eq!(inference_class.reserved(), 0);
    assert_eq!(inference_class.dispatched(), 0);
    assert_eq!(inference_class.denied(), 0);
    assert!(
        inference.requests.lock().await.is_empty(),
        "failed token refresh must not reach inference transport"
    );
    assert_eq!(
        auth.requests.lock().await.len(),
        2,
        "catalog plus one failed refresh"
    );
    auth.shutdown().await?;
    inference.abort_join().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_ready_lease_is_held_through_head_and_released_before_stream_body() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let manager = Arc::new(manager(&temp, &auth)?);
    install_stored(
        &manager,
        "lease-subject",
        "lease-access",
        "lease-refresh",
        system_unix_seconds()? + 3_600,
    )?;
    admit_catalog(&manager, &auth).await?;
    let inference = mock_peer().await?;
    let (response, release_head, release_body) = gated_sse(success_sse());
    inference.responses.send(response).await?;
    let route =
        manager.test_subscription_route(crate::XaiOrigin::new(inference.origin.clone())?)?;
    let resolved = XaiResolvedModel::new(
        route,
        XaiModel::new(ModelId::new(XAI_GROK_PRIMARY_CODING_MODEL)?, None),
        GrokCodingProfile::pinned_compatibility(),
        now_timestamp()?,
    )?;
    let model = permitted_subscription_model(resolved, inference.client.clone())?;
    let prepared = prepare_model_call(
        Arc::clone(&model),
        model_request("00000000-0000-4000-8000-000000000461")?,
        None,
    )
    .await?;
    let mut stream = prepared
        .start(ModelCallContext::new(
            AttemptId::from_str("00000000-0000-4000-8000-000000000461")?,
            plakous_model::CancellationToken::new(),
            None,
        ))
        .await?;
    let drain = tokio::spawn(async move {
        let mut completed = false;
        while let Some(event) = stream.next().await {
            if matches!(event?, ModelEvent::ResponseCompleted(_)) {
                completed = true;
            }
        }
        Ok::<_, ModelError>(completed)
    });
    wait_until_request_count(&inference, 1).await?;

    let logout_manager = Arc::clone(&manager);
    let (logout_started, wait_logout_started) = oneshot::channel();
    let mut logout = tokio::task::spawn_blocking(move || {
        // Probe the same exclusive store lock logout needs, without a timed
        // absence assertion or a race against the blocking worker's scheduling.
        let writer = fs::File::open(logout_manager.core.store.lock_path())
            .expect("credential record lock exists");
        assert!(
            matches!(writer.try_lock(), Err(fs::TryLockError::WouldBlock)),
            "logout must remain blocked while the inference response head is gated"
        );
        assert!(logout_manager.core.store.try_load().is_ok());
        logout_started.send(()).expect("logout-start receiver lost");
        logout_manager.logout()
    });
    wait_logout_started.await?;
    release_head
        .send(())
        .map_err(|()| std::io::Error::other("head gate receiver was lost"))?;
    let generation = tokio::time::timeout(Duration::from_secs(2), &mut logout)
        .await
        .map_err(|_| std::io::Error::other("logout stayed blocked after response head"))???;
    assert_eq!(generation.get(), 2);
    release_body
        .send(())
        .map_err(|()| std::io::Error::other("body gate receiver was lost"))?;
    drain.abort();
    let joined = tokio::time::timeout(Duration::from_secs(1), drain).await?;
    if let Err(error) = joined
        && !error.is_cancelled()
    {
        return Err(error.into());
    }
    auth.shutdown().await?;
    inference.abort_join().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_subscription_completes_two_round_local_tool_protocol() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let manager = manager(&temp, &auth)?;
    install_stored(
        &manager,
        "tool-subject",
        "tool-access",
        "tool-refresh",
        system_unix_seconds()? + 3600,
    )?;
    admit_catalog(&manager, &auth).await?;
    let inference = mock_peer().await?;
    enqueue_tool_loop_responses(&inference).await?;
    let route =
        manager.test_subscription_route(crate::XaiOrigin::new(inference.origin.clone())?)?;
    let resolved = XaiResolvedModel::new(
        route,
        XaiModel::new(ModelId::new(XAI_GROK_46_CODING_MODEL)?, None),
        GrokCodingProfile::pinned_compatibility(),
        now_timestamp()?,
    )?;
    let model = permitted_subscription_model(resolved, inference.client.clone())?;
    let historical = historical_grok_45_output(model.descriptor())?;

    let user = ConversationItem::new(
        EntryId::from_str("00000000-0000-4000-8000-000000000421")?,
        Actor::User,
        vec![ContentBlock::Text(TextBlock::new(
            "read the fixture",
            None,
        )?)],
        Provenance::UserInput,
    )?;
    let first = complete_model_round(
        Arc::clone(&model),
        tool_loop_request(
            vec![historical.clone(), user.clone()],
            "00000000-0000-4000-8000-000000000422",
        )?,
        "00000000-0000-4000-8000-000000000422",
    )
    .await?;
    let active_family_artifact = promote_responses_family_artifact(&first)?;
    let call = first
        .output()
        .iter()
        .flat_map(ConversationItem::blocks)
        .find_map(|block| match block {
            ContentBlock::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .ok_or_else(|| std::io::Error::other("first round did not request the tool"))?;
    assert_eq!(call.exposed_name().as_str(), "fixture_tool");
    assert_eq!(call.raw_arguments(), r#"{"path":"fixture.txt"}"#);

    let tool_result = ToolResultBlock::new(
        call.call_id(),
        ToolExecutionOwner::Local,
        ToolResultKind::Success,
        vec![ToolResultPart::Text(TextBlock::new(
            "fixture result",
            None,
        )?)],
        call.correlation().clone(),
    )?;
    let result_item = ConversationItem::new(
        EntryId::from_str("00000000-0000-4000-8000-000000000423")?,
        Actor::Tool,
        vec![ContentBlock::ToolResult(tool_result)],
        Provenance::ToolExecution,
    )?;
    let mut exchange = first.output().to_vec();
    exchange.push(result_item);
    let history = ConversationView::from_shared(
        CanonicalHistory::new(vec![
            HistorySegment::new(HistorySegmentKind::ReasoningReplay, vec![historical])?,
            HistorySegment::new(HistorySegmentKind::RecentIntent, vec![user])?,
            HistorySegment::new(HistorySegmentKind::ToolExchange, exchange)?,
        ])?,
        WorkingProjection::default(),
    );
    let artifact_identity = active_family_artifact.integrity();
    let prepared = prepare_model_call(
        Arc::clone(&model),
        tool_loop_request_from_view(history, "00000000-0000-4000-8000-000000000424")?,
        Some(active_family_artifact),
    )
    .await
    .map_err(|error| std::io::Error::other(error.message().to_owned()))?;
    assert_eq!(
        prepared.record().projection().active_artifact(),
        Some(artifact_identity)
    );
    assert_eq!(
        prepared.record().projection().cache_use(),
        PreparedCacheUse::ActiveFamilyArtifactAndRuntimeAffinity {
            artifact: artifact_identity,
            affinity: test_cache_affinity()?.identity(),
        }
    );
    assert_eq!(
        inference.requests.lock().await.len(),
        1,
        "artifact-bound preparation must remain lazy"
    );
    let second = complete_prepared_model_round(
        prepared,
        ModelCallContext::new(
            AttemptId::from_str("00000000-0000-4000-8000-000000000424")?,
            plakous_model::CancellationToken::new(),
            None,
        ),
    )
    .await?;
    assert_responses_family_artifact_candidate(&second)?;
    assert!(second.output().iter().any(|item| {
        item.blocks()
            .iter()
            .any(|block| matches!(block, ContentBlock::Text(text) if text.text() == "native ok"))
    }));

    assert_tool_loop_wire(&inference).await?;
    assert_eq!(auth.requests.lock().await.len(), 1);
    auth.shutdown().await?;
    inference.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_second_turn_replays_exact_encrypted_reasoning() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let manager = manager(&temp, &auth)?;
    install_stored(
        &manager,
        "reasoning-subject",
        "reasoning-access",
        "reasoning-refresh",
        system_unix_seconds()? + 3_600,
    )?;
    admit_catalog(&manager, &auth).await?;
    let inference = mock_peer().await?;
    for body in [
        reasoning_with_encrypted_sse(XAI_GROK_PRIMARY_CODING_MODEL),
        success_sse(),
    ] {
        inference
            .responses
            .send(MockResponse {
                status: "200 OK",
                content_type: "text/event-stream",
                body,
                extra_headers: String::new(),
                disconnect_after_request: false,
                truncate_after: None,
                before_head: None,
                before_body: None,
            })
            .await?;
    }
    let route =
        manager.test_subscription_route(crate::XaiOrigin::new(inference.origin.clone())?)?;
    let resolved = XaiResolvedModel::new(
        route,
        XaiModel::new(ModelId::new(XAI_GROK_PRIMARY_CODING_MODEL)?, None),
        GrokCodingProfile::pinned_compatibility(),
        now_timestamp()?,
    )?;
    let model = permitted_subscription_model(resolved, inference.client.clone())?;
    let user = ConversationItem::new(
        EntryId::from_str("00000000-0000-4000-8000-000000000471")?,
        Actor::User,
        vec![ContentBlock::Text(TextBlock::new(
            "reason carefully",
            None,
        )?)],
        Provenance::UserInput,
    )?;
    let (selected, effective) = provider_configuration_ids()?;
    let first_request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![user.clone()])?,
        RequestMetadata::for_turn(
            RunId::from_str("00000000-0000-4000-8000-000000000472")?,
            TurnId::from_str("00000000-0000-4000-8000-000000000473")?,
            AttemptId::from_str("00000000-0000-4000-8000-000000000474")?,
            now_timestamp()?,
        ),
    )
    .limits(test_generation_limits()?)
    .reasoning(ReasoningPolicy::Enabled {
        effort: Some(ReasoningEffort::Medium),
        requirement: CapabilityRequirement::Required,
    })
    .resolved_harness_plan_identity(test_harness_plan_identity()?)
    .build()?;
    let first = complete_model_round(
        Arc::clone(&model),
        first_request,
        "00000000-0000-4000-8000-000000000474",
    )
    .await?;
    let active_family_artifact = promote_responses_family_artifact(&first)?;
    let follow_up = ConversationItem::new(
        EntryId::from_str("00000000-0000-4000-8000-000000000475")?,
        Actor::User,
        vec![ContentBlock::Text(TextBlock::new("continue", None)?)],
        Provenance::UserInput,
    )?;
    let history = reasoning_follow_up_view(user, first.output(), follow_up)?;
    let second_request = ModelRequest::builder(
        selected,
        effective,
        history,
        RequestMetadata::for_turn(
            RunId::from_str("00000000-0000-4000-8000-000000000476")?,
            TurnId::from_str("00000000-0000-4000-8000-000000000477")?,
            AttemptId::from_str("00000000-0000-4000-8000-000000000478")?,
            now_timestamp()?,
        ),
    )
    .limits(test_generation_limits()?)
    .reasoning(ReasoningPolicy::Enabled {
        effort: Some(ReasoningEffort::Medium),
        requirement: CapabilityRequirement::Required,
    })
    .resolved_harness_plan_identity(test_harness_plan_identity()?)
    .build()?;
    let second = complete_model_round_with_active_family_artifact(
        Arc::clone(&model),
        second_request,
        "00000000-0000-4000-8000-000000000478",
        active_family_artifact,
    )
    .await?;
    assert_responses_family_artifact_candidate(&second)?;
    wait_for_requests(&inference, 2).await?;
    let requests = inference.requests.lock().await;
    let second = captured_request_body(&requests[1])?;
    let input = second["input"]
        .as_array()
        .ok_or_else(|| std::io::Error::other("second request input was not an array"))?;
    assert!(input.iter().any(|item| {
        item["type"] == "reasoning" && item["encrypted_content"] == "encrypted-reasoning-native"
    }));
    assert!(input.iter().any(|item| {
        item["type"] == "message"
            && item["role"] == "assistant"
            && item.to_string().contains("Use the bounded decoder.")
    }));
    assert_eq!(requests.len(), 2);
    drop(requests);
    auth.shutdown().await?;
    inference.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grok_stream_reset_does_not_poison_the_next_request_on_the_same_model() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let manager = manager(&temp, &auth)?;
    install_stored(
        &manager,
        "reset-subject",
        "reset-access",
        "reset-refresh",
        system_unix_seconds()? + 3_600,
    )?;
    admit_catalog(&manager, &auth).await?;
    let inference = mock_peer().await?;
    let body = success_sse();
    let fourth_event_end = body
        .match_indices("\n\n")
        .nth(3)
        .ok_or_else(|| std::io::Error::other("fixture has fewer than four SSE events"))?
        .0
        + 2;
    inference.responses.send(sse_response(body.clone())).await?;
    inference
        .responses
        .send(MockResponse {
            truncate_after: Some(fourth_event_end),
            ..sse_response(body.clone())
        })
        .await?;
    // The cut stream is replayed for the turn; the last response serves the
    // following request.
    inference.responses.send(sse_response(body.clone())).await?;
    inference.responses.send(sse_response(body)).await?;

    let route =
        manager.test_subscription_route(crate::XaiOrigin::new(inference.origin.clone())?)?;
    let resolved = XaiResolvedModel::new(
        route,
        XaiModel::new(ModelId::new(XAI_GROK_PRIMARY_CODING_MODEL)?, None),
        GrokCodingProfile::pinned_compatibility(),
        now_timestamp()?,
    )?;
    let model = permitted_subscription_model(resolved, inference.client.clone())?;
    let first = "00000000-0000-4000-8000-000000000481";
    assert_native_ok(
        &complete_model_round(Arc::clone(&model), model_request(first)?, first).await?,
    );
    let broken = "00000000-0000-4000-8000-000000000482";
    assert_native_ok(
        &complete_model_round(Arc::clone(&model), model_request(broken)?, broken).await?,
    );
    let following = "00000000-0000-4000-8000-000000000483";
    assert_native_ok(
        &complete_model_round(Arc::clone(&model), model_request(following)?, following).await?,
    );
    wait_for_requests(&inference, 4).await?;
    assert_eq!(inference.requests.lock().await.len(), 4);
    auth.shutdown().await?;
    inference.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_missing_encrypted_reasoning_fails_without_second_inference_write() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let manager = manager(&temp, &auth)?;
    install_stored(
        &manager,
        "strict-reasoning-subject",
        "strict-reasoning-access",
        "strict-reasoning-refresh",
        system_unix_seconds()? + 3_600,
    )?;
    admit_catalog(&manager, &auth).await?;
    let inference = mock_peer().await?;
    inference
        .responses
        .send(MockResponse {
            status: "200 OK",
            content_type: "text/event-stream",
            body: reasoning_without_encrypted_sse(XAI_GROK_PRIMARY_CODING_MODEL),
            extra_headers: String::new(),
            disconnect_after_request: false,
            truncate_after: None,
            before_head: None,
            before_body: None,
        })
        .await?;
    let route =
        manager.test_subscription_route(crate::XaiOrigin::new(inference.origin.clone())?)?;
    let resolved = XaiResolvedModel::new(
        route,
        XaiModel::new(ModelId::new(XAI_GROK_PRIMARY_CODING_MODEL)?, None),
        GrokCodingProfile::pinned_compatibility(),
        now_timestamp()?,
    )?;
    let model = permitted_subscription_model(resolved, inference.client.clone())?;
    let request = model_request_with_options(
        "00000000-0000-4000-8000-000000000479",
        test_generation_limits()?,
        Vec::new(),
        ReasoningPolicy::Enabled {
            effort: Some(ReasoningEffort::Medium),
            requirement: CapabilityRequirement::Required,
        },
    )?;
    assert!(
        complete_model_round(model, request, "00000000-0000-4000-8000-000000000479",)
            .await
            .is_err()
    );
    wait_for_requests(&inference, 1).await?;
    assert_eq!(
        inference.requests.lock().await.len(),
        1,
        "invalid strict reasoning must not trigger a second inference write"
    );
    auth.shutdown().await?;
    inference.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_subscription_never_follows_cross_origin_redirects() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let manager = manager(&temp, &auth)?;
    install_stored(
        &manager,
        "redirect-subject",
        "redirect-access",
        "redirect-refresh",
        system_unix_seconds()? + 3600,
    )?;
    admit_catalog(&manager, &auth).await?;
    let inference = mock_peer().await?;
    let redirect_target = mock_peer().await?;
    inference
        .responses
        .send(redirect(&format!(
            "{}/credential-capture",
            redirect_target.origin
        )))
        .await?;
    let route =
        manager.test_subscription_route(crate::XaiOrigin::new(inference.origin.clone())?)?;
    let resolved = XaiResolvedModel::new(
        route,
        XaiModel::new(ModelId::new(XAI_GROK_PRIMARY_CODING_MODEL)?, None),
        GrokCodingProfile::pinned_compatibility(),
        now_timestamp()?,
    )?;
    let model = permitted_subscription_model(resolved, inference.client.clone())?;
    let request = model_request("00000000-0000-4000-8000-000000000431")?;
    let prepared = prepare_model_call(Arc::clone(&model), request, None).await?;
    let started = tokio::time::timeout(
        Duration::from_secs(5),
        prepared.start(ModelCallContext::new(
            AttemptId::from_str("00000000-0000-4000-8000-000000000431")?,
            plakous_model::CancellationToken::new(),
            None,
        )),
    )
    .await?;
    let rejected = match started {
        Err(_) => true,
        Ok(mut stream) => {
            tokio::time::timeout(Duration::from_secs(5), async move {
                while let Some(event) = stream.next().await {
                    if event.is_err() {
                        return true;
                    }
                }
                false
            })
            .await?
        }
    };
    assert!(rejected, "redirect response must fail the direct operation");
    wait_for_requests(&inference, 1).await?;
    let requests = inference.requests.lock().await;
    assert!(
        String::from_utf8_lossy(&requests[0])
            .to_ascii_lowercase()
            .contains("authorization: bearer redirect-access")
    );
    assert!(redirect_target.requests.lock().await.is_empty());
    assert_eq!(auth.requests.lock().await.len(), 1);
    drop(requests);
    auth.shutdown().await?;
    inference.shutdown().await?;
    redirect_target.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prepare_and_unpolled_start_have_no_credential_or_network_effect() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let manager = manager(&temp, &auth)?;
    install_stored(
        &manager,
        "admission-subject",
        "admission-access",
        "refresh",
        system_unix_seconds()? + 3600,
    )?;
    admit_catalog(&manager, &auth).await?;
    let inference = mock_peer().await?;
    let route =
        manager.test_subscription_route(crate::XaiOrigin::new(inference.origin.clone())?)?;
    let resolved = XaiResolvedModel::new(
        route,
        XaiModel::new(ModelId::new(XAI_GROK_PRIMARY_CODING_MODEL)?, None),
        GrokCodingProfile::pinned_compatibility(),
        now_timestamp()?,
    )?;
    let model = permitted_subscription_model(resolved, inference.client.clone())?;
    let output_maximum = NonZeroU32::new(64)
        .ok_or_else(|| std::io::Error::other("nonzero output maximum construction failed"))?;
    let request = model_request_with_limits(
        "00000000-0000-4000-8000-000000000411",
        GenerationLimits::new(Some(output_maximum), Vec::new())?,
    )?;
    let exact_request = request.clone();
    let prepared = prepare_model_call(Arc::clone(&model), request, None).await?;
    let descriptor = model.descriptor();
    let exact_wire = lower_responses_request(
        &exact_request,
        ResponsesRequestConfig::new(
            descriptor.protocol(),
            descriptor.model_family(),
            descriptor.requested_model(),
            crate::SUPERGROK_RESPONSES_DIALECT.request_policy(),
            None,
        ),
    )?
    .body_bytes()?;
    let exact_body: serde_json::Value = serde_json::from_slice(&exact_wire)?;
    assert!(exact_body.get("prompt_cache_key").is_none());
    assert!(
        descriptor
            .capabilities()
            .get(plakous_model::Capability::Caching)
            .is_none()
    );
    let expected_wire_digest = Digest::sha256(
        DigestDomain::Blob,
        1,
        &[b"responses-prepared-wire-body-v1", &exact_wire],
    )?;
    let projection = prepared.record().projection();
    assert_eq!(projection.protocol_family().as_str(), "responses-family");
    assert_eq!(projection.dialect(), descriptor.protocol());
    assert_eq!(projection.route(), descriptor.route());
    assert_eq!(projection.request(), exact_request.identity());
    assert_eq!(projection.wire_digest(), expected_wire_digest);
    assert_eq!(projection.wire_size(), u64::try_from(exact_wire.len())?);
    assert_eq!(
        projection.token_projection().wire_digest(),
        expected_wire_digest
    );
    assert_eq!(
        projection.token_projection().wire_size(),
        projection.wire_size()
    );
    assert_eq!(
        projection.cache_use(),
        PreparedCacheUse::RuntimeAffinity {
            affinity: test_cache_affinity()?.identity(),
        }
    );
    assert!(
        projection.output_limit().is_some(),
        "synthetic test-only context contract omitted its fixture output proof"
    );
    assert_eq!(auth.requests.lock().await.len(), 1);
    assert!(inference.requests.lock().await.is_empty());
    let unpolled = prepared.start(ModelCallContext::new(
        AttemptId::from_str("00000000-0000-4000-8000-000000000411")?,
        plakous_model::CancellationToken::new(),
        None,
    ));
    assert_eq!(auth.requests.lock().await.len(), 1);
    assert!(inference.requests.lock().await.is_empty());
    drop(unpolled);
    assert_eq!(auth.requests.lock().await.len(), 1);
    assert!(inference.requests.lock().await.is_empty());
    auth.shutdown().await?;
    inference.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prepared_call_dispatches_exact_wire_once_and_shared_host_accepts_xai_terminals()
-> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let manager = manager(&temp, &auth)?;
    install_stored(
        &manager,
        "prepared-wire-subject",
        "prepared-wire-access",
        "prepared-wire-refresh",
        system_unix_seconds()? + 3_600,
    )?;
    admit_catalog(&manager, &auth).await?;
    let inference = mock_peer().await?;
    for body in [success_sse(), success_sse(), success_sse()] {
        inference.responses.send(sse_response(body)).await?;
    }
    let route =
        manager.test_subscription_route(crate::XaiOrigin::new(inference.origin.clone())?)?;
    let resolved = XaiResolvedModel::new(
        route,
        XaiModel::new(ModelId::new(XAI_GROK_PRIMARY_CODING_MODEL)?, None),
        GrokCodingProfile::pinned_compatibility(),
        now_timestamp()?,
    )?;
    let model = permitted_subscription_model(resolved, inference.client.clone())?;

    let attempt = "00000000-0000-4000-8000-000000000412";
    let request = model_request(attempt)?;
    let descriptor = model.descriptor();
    let exact_wire = lower_responses_request(
        &request,
        ResponsesRequestConfig::new(
            descriptor.protocol(),
            descriptor.model_family(),
            descriptor.requested_model(),
            crate::SUPERGROK_RESPONSES_DIALECT.request_policy(),
            None,
        ),
    )?
    .body_bytes()?;
    let prepared = prepare_model_call(Arc::clone(&model), request, None).await?;
    let first = complete_prepared_model_round(
        prepared,
        ModelCallContext::new(
            AttemptId::from_str(attempt)?,
            plakous_model::CancellationToken::new(),
            None,
        ),
    )
    .await?;
    assert_native_ok(&first);
    wait_until_request_count(&inference, 1).await?;
    {
        let requests = inference.requests.lock().await;
        assert_eq!(requests.len(), 1);
        assert_eq!(captured_request_body_bytes(&requests[0])?, exact_wire);
    }

    for (attempt, delivery) in [
        (
            "00000000-0000-4000-8000-000000000413",
            ObservationDeliveryFixture::TerminalOnly,
        ),
        (
            "00000000-0000-4000-8000-000000000414",
            ObservationDeliveryFixture::Gapped,
        ),
    ] {
        let prepared =
            prepare_model_call(Arc::clone(&model), model_request(attempt)?, None).await?;
        let response = complete_prepared_model_round(
            filter_prepared_observations(prepared, delivery),
            ModelCallContext::new(
                AttemptId::from_str(attempt)?,
                plakous_model::CancellationToken::new(),
                None,
            ),
        )
        .await?;
        assert_native_ok(&response);
    }
    wait_for_requests(&inference, 3).await?;
    assert_eq!(inference.requests.lock().await.len(), 3);
    assert_eq!(auth.requests.lock().await.len(), 1);
    auth.shutdown().await?;
    inference.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_custom_reasoning_drop_and_foreign_attempt_have_zero_provider_effect() -> TestResult
{
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let manager = manager(&temp, &auth)?;
    install_stored(
        &manager,
        "zero-effect-subject",
        "zero-effect-access",
        "zero-effect-refresh",
        system_unix_seconds()? + 3_600,
    )?;
    admit_catalog(&manager, &auth).await?;
    let inference = mock_peer().await?;
    let route =
        manager.test_subscription_route(crate::XaiOrigin::new(inference.origin.clone())?)?;
    let resolved = XaiResolvedModel::new(
        route,
        XaiModel::new(ModelId::new(XAI_GROK_PRIMARY_CODING_MODEL)?, None),
        GrokCodingProfile::pinned_compatibility(),
        now_timestamp()?,
    )?;
    let model = permitted_subscription_model(resolved, inference.client.clone())?;

    let cache_request = model_request_with_cache(
        "00000000-0000-4000-8000-000000000420",
        CachePolicy::Enabled {
            requirement: CapabilityRequirement::Required,
        },
    )?;
    assert!(matches!(
        model
            .descriptor()
            .validate_request(cache_request, ValidationMode::Strict),
        Err(plakous_model::ValidationError::RequiredCapability {
            capability: plakous_model::Capability::Caching,
            support: plakous_model::Support::Unsupported,
        })
    ));

    let custom_error = prepare_model_call(
        Arc::clone(&model),
        model_request_with_options(
            "00000000-0000-4000-8000-000000000415",
            GenerationLimits::default(),
            vec![fixture_string_custom_tool()?],
            ReasoningPolicy::Disabled,
        )?,
        None,
    )
    .await
    .err()
    .ok_or_else(|| std::io::Error::other("custom tool unexpectedly prepared"))?;
    assert_eq!(custom_error.kind(), ModelErrorKind::InvalidRequest);

    let reasoning_error = prepare_model_call(
        Arc::clone(&model),
        model_request_with_options(
            "00000000-0000-4000-8000-000000000416",
            GenerationLimits::default(),
            Vec::new(),
            ReasoningPolicy::Enabled {
                effort: Some(ReasoningEffort::XHigh),
                requirement: CapabilityRequirement::Required,
            },
        )?,
        None,
    )
    .await
    .err()
    .ok_or_else(|| std::io::Error::other("xhigh reasoning unexpectedly prepared"))?;
    assert_eq!(reasoning_error.kind(), ModelErrorKind::InvalidRequest);

    let dropped = prepare_model_call(
        Arc::clone(&model),
        model_request("00000000-0000-4000-8000-000000000417")?,
        None,
    )
    .await?;
    drop(dropped);

    let foreign = prepare_model_call(
        Arc::clone(&model),
        model_request("00000000-0000-4000-8000-000000000418")?,
        None,
    )
    .await?;
    let foreign_error = foreign
        .start(ModelCallContext::new(
            AttemptId::from_str("00000000-0000-4000-8000-000000000419")?,
            plakous_model::CancellationToken::new(),
            None,
        ))
        .await
        .err()
        .ok_or_else(|| std::io::Error::other("foreign attempt unexpectedly started"))?;
    assert_eq!(foreign_error.kind(), ModelErrorKind::InvalidRequest);

    assert_eq!(auth.requests.lock().await.len(), 1);
    assert!(inference.requests.lock().await.is_empty());
    auth.shutdown().await?;
    inference.shutdown().await?;
    Ok(())
}

fn install_stored(
    manager: &XaiNativeAuthManager,
    subject: &str,
    access: &str,
    refresh: &str,
    expires_at: u64,
) -> Result<(), XaiNativeAuthError> {
    let record = stored_record(
        subject,
        access,
        refresh,
        expires_at,
        CredentialGeneration::new(1)?,
    )?;
    manager.core.store.initialize_selected(&record)?;
    Ok(())
}

fn stored_record(
    subject: &str,
    access: &str,
    refresh: &str,
    expires_at: u64,
    generation: CredentialGeneration,
) -> Result<CredentialRecord, XaiNativeAuthError> {
    let account = AccountBinding::new(subject)?;
    let stored = StoredTokens {
        version: STORED_TOKENS_VERSION,
        issuer: XAI_ISSUER.to_owned(),
        client_id: XAI_PUBLIC_CLIENT_ID.to_owned(),
        subject: subject.to_owned(),
        scope: XAI_OAUTH_SCOPE.to_owned(),
        access_token: access.to_owned(),
        refresh_token: refresh.to_owned(),
        expires_at,
        selection_epoch: "selection-epoch-one-0123456789abcdefghijklm".to_owned(),
    };
    credential_record(account, generation, &stored)
}

fn rotate_selected_generation(
    manager: &XaiNativeAuthManager,
    subject: &str,
    access: &str,
    refresh: &str,
) -> Result<CredentialFence, XaiNativeAuthError> {
    let baseline = manager
        .core
        .store
        .load()?
        .ok_or(XaiNativeAuthError::NotLoggedIn)?;
    let replacement = stored_record(
        subject,
        access,
        refresh,
        system_unix_seconds()? + 3_600,
        baseline.generation().next()?,
    )?;
    manager
        .core
        .store
        .replace_selected_login(&baseline, replacement)
        .map_err(Into::into)
}

fn expire_selected_access(
    manager: &XaiNativeAuthManager,
    subject: &str,
    access: &str,
    refresh: &str,
) -> Result<CredentialFence, XaiNativeAuthError> {
    let baseline = manager
        .core
        .store
        .load()?
        .ok_or(XaiNativeAuthError::NotLoggedIn)?;
    let replacement = stored_record(
        subject,
        access,
        refresh,
        system_unix_seconds()?.saturating_sub(1),
        baseline.generation().next()?,
    )?;
    manager
        .core
        .store
        .replace_selected_login(&baseline, replacement)
        .map_err(Into::into)
}

fn required<T>(value: Option<T>, message: &'static str) -> TestResult<T> {
    value.ok_or_else(|| std::io::Error::other(message).into())
}

async fn admit_catalog(
    manager: &XaiNativeAuthManager,
    peer: &MockPeer,
) -> TestResult<XaiNativeCatalogReceipt> {
    enqueue_admitted_catalog(peer).await?;
    Ok(manager.refresh_catalog().await?)
}

async fn enqueue_admitted_catalog(peer: &MockPeer) -> TestResult {
    peer.responses
        .send(response(&json!({
            "object":"list",
            "data":[
                {
                    "id":XAI_GROK_46_CODING_MODEL,
                    "object":"model",
                    "created":1,
                    "owned_by":"xai",
                    "contextWindow":4096
                },
                {
                    "id":XAI_GROK_PRIMARY_CODING_MODEL,
                    "object":"model",
                    "created":1,
                    "owned_by":"xai",
                    "contextWindow":500_000
                }
            ]
        })))
        .await?;
    Ok(())
}

async fn wait_for_requests(peer: &MockPeer, count: usize) -> TestResult {
    peer.responses.wait().await?;
    let observed = peer.requests.lock().await.len();
    if observed < count {
        return Err(std::io::Error::other(format!(
            "mock observed {observed} requests, expected at least {count}"
        ))
        .into());
    }
    Ok(())
}

async fn wait_until_request_count(peer: &MockPeer, count: usize) -> TestResult {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if peer.requests.lock().await.len() >= count {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|_| std::io::Error::other("mock did not observe the bounded request count"))?;
    Ok(())
}

#[derive(Clone, Copy)]
enum CatalogGapMutation {
    FreshSameFenceReplacement,
    Expire,
}

struct CatalogGapGate {
    inner: Arc<InMemorySubscriptionRequestGate>,
    core: Arc<ManagerCore>,
    mutation: CatalogGapMutation,
}

impl SubscriptionRequestGate for CatalogGapGate {
    fn acquire(
        &self,
        class: SubscriptionRequestClass,
    ) -> Result<Box<dyn SubscriptionRequestPermit>, SubscriptionLimitError> {
        self.inner.acquire(class)
    }

    fn acquire_inference(
        &self,
        input_token_upper_bound: u64,
    ) -> Result<Box<dyn SubscriptionRequestPermit>, SubscriptionLimitError> {
        let inner = self.inner.acquire_inference(input_token_upper_bound)?;
        Ok(Box::new(CatalogGapPermit {
            inner,
            core: Arc::clone(&self.core),
            mutation: self.mutation,
        }))
    }

    fn snapshot(&self) -> Result<SubscriptionLimitSnapshot, SubscriptionLimitError> {
        self.inner.snapshot()
    }
}

struct CatalogGapPermit {
    inner: Box<dyn SubscriptionRequestPermit>,
    core: Arc<ManagerCore>,
    mutation: CatalogGapMutation,
}

impl SubscriptionRequestPermit for CatalogGapPermit {
    fn refund_not_started(&mut self) -> Result<(), SubscriptionLimitError> {
        self.inner.refund_not_started()
    }

    fn class(&self) -> SubscriptionRequestClass {
        self.inner.class()
    }

    fn authorize_dispatch(&mut self) -> Result<(), SubscriptionLimitError> {
        self.inner.authorize_dispatch()?;
        let now = system_unix_seconds().map_err(|_| SubscriptionLimitError::Unavailable)?;
        let mut catalog = self
            .core
            .catalog
            .lock()
            .map_err(|_| SubscriptionLimitError::Unavailable)?;
        let receipt = catalog
            .as_mut()
            .ok_or(SubscriptionLimitError::Unavailable)?;
        match self.mutation {
            CatalogGapMutation::FreshSameFenceReplacement => {
                receipt.public.observed_at =
                    Timestamp::new(now, 0).map_err(|_| SubscriptionLimitError::Unavailable)?;
                let replacement_expiry = receipt.public.expires_at.seconds().saturating_sub(1);
                if replacement_expiry <= now {
                    return Err(SubscriptionLimitError::Unavailable);
                }
                receipt.public.expires_at = Timestamp::new(replacement_expiry, 0)
                    .map_err(|_| SubscriptionLimitError::Unavailable)?;
            }
            CatalogGapMutation::Expire => {
                receipt.public.expires_at =
                    Timestamp::new(now, 0).map_err(|_| SubscriptionLimitError::Unavailable)?;
            }
        }
        Ok(())
    }
}

fn classified_subscription_gate(
    auth_refresh: u32,
    catalog: u32,
    inference: u32,
) -> TestResult<Arc<InMemorySubscriptionRequestGate>> {
    let limits = SubscriptionRequestLimits::new(
        auth_refresh,
        catalog,
        inference,
        1_000_000,
        Some(Duration::from_secs(300)),
        0,
    )?;
    Ok(Arc::new(InMemorySubscriptionRequestGate::new(
        limits,
        Arc::new(SystemSubscriptionGateClock::default()),
    )))
}

fn subscription_gate(inference: u32) -> TestResult<Arc<InMemorySubscriptionRequestGate>> {
    classified_subscription_gate(0, 0, inference)
}

struct ClassOrderGate {
    inner: Arc<InMemorySubscriptionRequestGate>,
    order: Arc<std::sync::Mutex<Vec<SubscriptionRequestClass>>>,
}

impl SubscriptionRequestGate for ClassOrderGate {
    fn acquire(
        &self,
        class: SubscriptionRequestClass,
    ) -> Result<Box<dyn SubscriptionRequestPermit>, SubscriptionLimitError> {
        self.order
            .lock()
            .map_err(|_| SubscriptionLimitError::Unavailable)?
            .push(class);
        self.inner.acquire(class)
    }

    fn acquire_inference(
        &self,
        input_token_upper_bound: u64,
    ) -> Result<Box<dyn SubscriptionRequestPermit>, SubscriptionLimitError> {
        self.order
            .lock()
            .map_err(|_| SubscriptionLimitError::Unavailable)?
            .push(SubscriptionRequestClass::Inference);
        self.inner.acquire_inference(input_token_upper_bound)
    }

    fn snapshot(&self) -> Result<SubscriptionLimitSnapshot, SubscriptionLimitError> {
        self.inner.snapshot()
    }
}

fn subscription_model_with_gate(
    resolved: XaiResolvedModel,
    client: Client,
    gate: Arc<dyn SubscriptionRequestGate>,
) -> TestResult<Arc<XaiLanguageModel>> {
    let resolved = resolved.with_test_context_contract(now_timestamp()?)?;
    let model = XaiLanguageModel::new(resolved, client)?.with_subscription_request_gate(gate)?;
    Ok(Arc::new(model))
}

fn permitted_subscription_model(
    resolved: XaiResolvedModel,
    client: Client,
) -> TestResult<Arc<XaiLanguageModel>> {
    subscription_model_with_gate(resolved, client, subscription_gate(64)?)
}

/// A permitted model on the release context contract: the fixture contract
/// is not what a fresh resolution produces, so renewal would refuse it as a
/// semantic change.
fn renewable_subscription_model(
    resolved: XaiResolvedModel,
    client: Client,
) -> TestResult<Arc<XaiLanguageModel>> {
    let model = XaiLanguageModel::new(resolved, client)?
        .with_subscription_request_gate(subscription_gate(64)?)?;
    Ok(Arc::new(model))
}

fn model_request(attempt: &str) -> TestResult<ModelRequest> {
    model_request_with_limits(attempt, test_generation_limits()?)
}

fn model_request_with_cache(attempt: &str, cache: CachePolicy) -> TestResult<ModelRequest> {
    let item = ConversationItem::new(
        EntryId::from_str("00000000-0000-4000-8000-000000000402")?,
        Actor::User,
        vec![ContentBlock::Text(TextBlock::new(
            "native subscription",
            None,
        )?)],
        Provenance::UserInput,
    )?;
    let (selected, effective) = provider_configuration_ids()?;
    Ok(ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![item])?,
        RequestMetadata::for_turn(
            RunId::from_str("00000000-0000-4000-8000-000000000403")?,
            TurnId::from_str("00000000-0000-4000-8000-000000000404")?,
            AttemptId::from_str(attempt)?,
            now_timestamp()?,
        ),
    )
    .limits(test_generation_limits()?)
    .cache(cache)
    .resolved_harness_plan_identity(test_harness_plan_identity()?)
    .build()?)
}

fn test_generation_limits() -> TestResult<GenerationLimits> {
    let maximum = NonZeroU32::new(64)
        .ok_or_else(|| std::io::Error::other("test output maximum must be nonzero"))?;
    Ok(GenerationLimits::new(Some(maximum), Vec::new())?)
}

fn model_request_with_limits(attempt: &str, limits: GenerationLimits) -> TestResult<ModelRequest> {
    model_request_with_options(attempt, limits, Vec::new(), ReasoningPolicy::Disabled)
}

fn model_request_with_options(
    attempt: &str,
    limits: GenerationLimits,
    tools: Vec<ToolDefinition>,
    reasoning: ReasoningPolicy,
) -> TestResult<ModelRequest> {
    let item = ConversationItem::new(
        EntryId::from_str("00000000-0000-4000-8000-000000000402")?,
        Actor::User,
        vec![ContentBlock::Text(TextBlock::new(
            "native subscription",
            None,
        )?)],
        Provenance::UserInput,
    )?;
    let (selected, effective) = provider_configuration_ids()?;
    Ok(ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![item])?,
        RequestMetadata::for_turn(
            RunId::from_str("00000000-0000-4000-8000-000000000403")?,
            TurnId::from_str("00000000-0000-4000-8000-000000000404")?,
            AttemptId::from_str(attempt)?,
            now_timestamp()?,
        ),
    )
    .limits(limits)
    .tools(
        tools
            .into_iter()
            .map(|tool| {
                plakous_model::ConfiguredTool::new(
                    tool,
                    plakous_model::ToolExecutionDisposition::Local,
                    plakous_model::ToolCallerDisposition::DirectOnly,
                )
            })
            .collect(),
    )
    .reasoning(reasoning)
    .resolved_harness_plan_identity(test_harness_plan_identity()?)
    .build()?)
}

fn test_harness_plan_identity() -> TestResult<Digest> {
    Ok(Digest::sha256(
        DigestDomain::ResolvedHarnessPlan,
        1,
        &[b"xai-native-auth-test-harness-plan"],
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
            &[b"xai-native-auth-test-selected-configuration"],
        )?)?)?,
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"xai-native-auth-test-effective-configuration"],
        )?)?)?,
    ))
}

fn fixture_tool() -> TestResult<ToolDefinition> {
    Ok(ToolDefinition::new(
        ToolName::new("fixture_tool")?,
        "1.0.0",
        "Read one deterministic fixture",
        ToolInputContract::structured(json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"],
            "additionalProperties": false
        }))?,
        ToolOutputContract::portable_model_content(),
        ToolConcurrency::Serial,
        ToolEffect::ReadOnly,
    )?)
}

fn fixture_string_custom_tool() -> TestResult<ToolDefinition> {
    Ok(ToolDefinition::new(
        ToolName::new("fixture_free_form")?,
        "1.0.0",
        "Accept deterministic free-form text",
        ToolInputContract::utf8(65_536)?,
        ToolOutputContract::portable_model_content(),
        ToolConcurrency::Serial,
        ToolEffect::ReadOnly,
    )?)
}

fn tool_loop_request(items: Vec<ConversationItem>, attempt: &str) -> TestResult<ModelRequest> {
    tool_loop_request_from_view(ConversationView::from_items(items)?, attempt)
}

fn tool_loop_request_from_view(input: ConversationView, attempt: &str) -> TestResult<ModelRequest> {
    let (selected, effective) = provider_configuration_ids()?;
    Ok(ModelRequest::builder(
        selected,
        effective,
        input,
        RequestMetadata::for_turn(
            RunId::from_str("00000000-0000-4000-8000-000000000425")?,
            TurnId::from_str("00000000-0000-4000-8000-000000000426")?,
            AttemptId::from_str(attempt)?,
            now_timestamp()?,
        ),
    )
    .limits(test_generation_limits()?)
    .tools(vec![plakous_model::ConfiguredTool::new(
        fixture_tool()?,
        plakous_model::ToolExecutionDisposition::Local,
        plakous_model::ToolCallerDisposition::DirectOnly,
    )])
    .resolved_harness_plan_identity(test_harness_plan_identity()?)
    .build()?)
}

fn tool_follow_up_view(
    user: ConversationItem,
    output: &[ConversationItem],
    result: ConversationItem,
) -> TestResult<ConversationView> {
    let mut exchange = output.to_vec();
    exchange.push(result);
    Ok(ConversationView::from_shared(
        CanonicalHistory::new(vec![
            HistorySegment::new(HistorySegmentKind::RecentIntent, vec![user])?,
            HistorySegment::new(HistorySegmentKind::ToolExchange, exchange)?,
        ])?,
        WorkingProjection::default(),
    ))
}

fn reasoning_follow_up_view(
    user: ConversationItem,
    output: &[ConversationItem],
    follow_up: ConversationItem,
) -> TestResult<ConversationView> {
    Ok(ConversationView::from_shared(
        CanonicalHistory::new(vec![
            HistorySegment::new(HistorySegmentKind::RecentIntent, vec![user])?,
            HistorySegment::new(HistorySegmentKind::ReasoningReplay, output.to_vec())?,
            HistorySegment::new(HistorySegmentKind::RecentIntent, vec![follow_up])?,
        ])?,
        WorkingProjection::default(),
    ))
}

async fn complete_model_round(
    model: Arc<XaiLanguageModel>,
    request: ModelRequest,
    attempt: &str,
) -> TestResult<ModelResponse> {
    let prepared = prepare_model_call(Arc::clone(&model), request, None).await?;
    Ok(complete_prepared_model_round(
        prepared,
        ModelCallContext::new(
            AttemptId::from_str(attempt)?,
            plakous_model::CancellationToken::new(),
            None,
        ),
    )
    .await?)
}

async fn complete_model_round_with_active_family_artifact(
    model: Arc<XaiLanguageModel>,
    request: ModelRequest,
    attempt: &str,
    artifact: ActiveFamilyArtifact,
) -> TestResult<ModelResponse> {
    let prepared = prepare_model_call(Arc::clone(&model), request, Some(artifact)).await?;
    Ok(complete_prepared_model_round(
        prepared,
        ModelCallContext::new(
            AttemptId::from_str(attempt)?,
            plakous_model::CancellationToken::new(),
            None,
        ),
    )
    .await?)
}

async fn prepare_model_call(
    model: Arc<XaiLanguageModel>,
    request: ModelRequest,
    artifact: Option<ActiveFamilyArtifact>,
) -> Result<PreparedModelCall, ModelError> {
    let prepared_at = request.metadata().created_at();
    let plan = model
        .descriptor()
        .validate_request(request, ValidationMode::Strict)
        .map_err(ModelError::from)?;
    let authority = Arc::clone(&model)
        .revalidate_runtime_authority(prepared_at)
        .await
        .map_err(plakous_model::ModelPreparationError::from)
        .map_err(ModelError::from)?;
    let mut context = ModelPreparationContext::new(authority, prepared_at).with_cache_affinity(
        test_cache_affinity()
            .map_err(|error| ModelError::new(ModelErrorKind::InvalidRequest, error.to_string()))?,
    );
    if let Some(artifact) = artifact {
        context = context.with_active_family_artifact(artifact);
    }
    Arc::clone(&model)
        .prepare_call(plan, context)
        .map_err(ModelError::from)
}

async fn complete_prepared_model_round(
    prepared: PreparedModelCall,
    context: ModelCallContext,
) -> Result<ModelResponse, ModelError> {
    let plan = prepared.record().projection().plan();
    let mut stream = tokio::time::timeout(Duration::from_secs(5), prepared.start(context))
        .await
        .map_err(|_| {
            ModelError::new(
                ModelErrorKind::Timeout,
                "prepared model start exceeded the test deadline",
            )
        })??;
    let completed = tokio::time::timeout(Duration::from_secs(5), async {
        let mut completed = None;
        while let Some(event) = stream.next().await {
            if completed.is_some() {
                return Err(ModelError::new(
                    ModelErrorKind::InvalidEvent,
                    "event followed terminal aggregate",
                ));
            }
            if let ModelEvent::ResponseCompleted(response) = event? {
                response.validate_for_commit()?;
                if response.plan_digest() != plan {
                    return Err(ModelError::new(
                        ModelErrorKind::InvalidEvent,
                        "terminal aggregate used another prepared plan",
                    ));
                }
                completed = Some(response);
            }
        }
        completed.ok_or_else(|| {
            ModelError::new(
                ModelErrorKind::IncompleteResponse,
                "response completed event was missing",
            )
        })
    })
    .await
    .map_err(|_| {
        ModelError::new(
            ModelErrorKind::Timeout,
            "prepared model stream exceeded the test deadline",
        )
    })??;
    Ok(completed)
}

#[derive(Clone, Copy)]
enum ObservationDeliveryFixture {
    TerminalOnly,
    Gapped,
}

fn filter_prepared_observations(
    prepared: PreparedModelCall,
    delivery: ObservationDeliveryFixture,
) -> PreparedModelCall {
    prepared.map_start(move |start| {
        Box::pin(async move {
            let inner = start.await?;
            Ok(Box::pin(FilteredObservationStream {
                inner,
                delivery,
                observed: 0,
            }) as ModelStream)
        })
    })
}

struct FilteredObservationStream {
    inner: ModelStream,
    delivery: ObservationDeliveryFixture,
    observed: usize,
}

impl futures_core::Stream for FilteredObservationStream {
    type Item = Result<ModelEvent, ModelError>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match self.inner.as_mut().poll_next(context) {
                Poll::Ready(Some(Ok(event @ ModelEvent::ResponseCompleted(_)))) => {
                    return Poll::Ready(Some(Ok(event)));
                }
                Poll::Ready(Some(Ok(event))) => {
                    self.observed = self.observed.saturating_add(1);
                    if matches!(self.delivery, ObservationDeliveryFixture::Gapped)
                        && self.observed % 2 == 1
                    {
                        return Poll::Ready(Some(Ok(event)));
                    }
                }
                Poll::Ready(Some(Err(error))) => return Poll::Ready(Some(Err(error))),
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

fn assert_responses_family_artifact_candidate(
    response: &ModelResponse,
) -> TestResult<&SealedFamilyArtifactCandidate> {
    let candidate = required(
        response.family_artifact_candidate(),
        "response omitted its family artifact candidate",
    )?;
    assert_eq!(candidate.family(), "responses");
    assert_eq!(
        candidate.schema_revision(),
        RESPONSES_ARTIFACT_SCHEMA_REVISION
    );
    assert_eq!(
        candidate.codec_revision(),
        RESPONSES_ARTIFACT_CODEC_REVISION
    );
    assert!(
        !serde_json::to_value(response)?
            .as_object()
            .is_some_and(|object| object.contains_key("replay"))
    );
    let canonical = serde_json::to_vec(response.output())?;
    assert!(
        !canonical
            .windows(b"responses.artifact".len())
            .any(|part| part == b"responses.artifact")
    );
    Ok(candidate)
}

fn promote_responses_family_artifact(response: &ModelResponse) -> TestResult<ActiveFamilyArtifact> {
    let candidate = assert_responses_family_artifact_candidate(response)?;
    let active = ActiveFamilyArtifact::new(
        candidate.family(),
        candidate.schema_revision(),
        candidate.codec_revision(),
        candidate.binding(),
        candidate.bytes().to_vec(),
    )?;
    assert_eq!(active.family(), candidate.family());
    assert_eq!(active.schema_revision(), candidate.schema_revision());
    assert_eq!(active.codec_revision(), candidate.codec_revision());
    assert_eq!(active.binding(), candidate.binding());
    assert_eq!(active.bytes(), candidate.bytes());
    Ok(active)
}

fn historical_grok_45_output(descriptor: &ResolvedModelDescriptor) -> TestResult<ConversationItem> {
    let replay = ReplayEnvelope::new(
        descriptor.protocol().clone(),
        descriptor.model_family().cloned(),
        ReplayScope::ExactModel(ModelId::new(XAI_GROK_PRIMARY_CODING_MODEL)?),
        ReplayKindId::new(
            crate::SUPERGROK_RESPONSES_DIALECT
                .output_item_replay()
                .name(),
        )?,
        crate::SUPERGROK_RESPONSES_DIALECT
            .output_item_replay()
            .version(),
        DataClassification::Sensitive,
        serde_json::value::RawValue::from_string(
            json!({
                "id":"rs_historical",
                "type":"reasoning",
                "status":"completed",
                "summary":[],
                "encrypted_content":"historical-grok-4.5"
            })
            .to_string(),
        )?,
    )?;
    Ok(ConversationItem::new(
        EntryId::from_str("00000000-0000-4000-8000-000000000420")?,
        Actor::Assistant,
        vec![ContentBlock::Opaque(OpaqueBlock::new(
            replay,
            OpaqueFallback::Omit,
        )?)],
        Provenance::ModelOutput,
    )?)
}

fn reasoning_function_call_sse() -> String {
    let reasoning = json!({
        "id":"rs_tool", "type":"reasoning", "status":"completed",
        "encrypted_content":"encrypted-tool-reasoning",
        "summary":[{"type":"summary_text","text":"I should inspect the fixture."}]
    });
    let call = json!({
        "id":"fc_native", "type":"function_call", "status":"completed",
        "call_id":"call_native", "name":"fixture_tool",
        "arguments":"{\"path\":\"fixture.txt\"}"
    });
    [
        json!({"type":"response.created","sequence_number":1,"response":{"id":"resp_tool","status":"in_progress","model":"grok-4.6-build","output":[]}}),
        json!({"type":"response.output_item.added","sequence_number":2,"output_index":0,"item":{"id":"rs_tool","type":"reasoning","status":"in_progress","summary":[]}}),
        json!({"type":"response.reasoning_summary_text.delta","sequence_number":3,"output_index":0,"summary_index":0,"delta":"I should inspect the fixture."}),
        json!({"type":"response.output_item.done","sequence_number":4,"output_index":0,"item":reasoning.clone()}),
        json!({"type":"response.output_item.added","sequence_number":5,"output_index":1,"item":{"id":"fc_native","type":"function_call","status":"in_progress","call_id":"call_native","name":"fixture_tool","arguments":""}}),
        json!({"type":"response.function_call_arguments.delta","sequence_number":6,"output_index":1,"item_id":"fc_native","delta":"{\"path\":\"fixture.txt\"}"}),
        json!({"type":"response.function_call_arguments.done","sequence_number":7,"output_index":1,"item_id":"fc_native","arguments":"{\"path\":\"fixture.txt\"}"}),
        json!({"type":"response.output_item.done","sequence_number":8,"output_index":1,"item":call.clone()}),
        json!({"type":"response.completed","sequence_number":9,"response":{"id":"resp_tool","object":"response","status":"completed","model":"grok-4.6-build","output":[reasoning,call],"usage":{"input_tokens":5,"output_tokens":4,"total_tokens":9},"incomplete_details":null,"error":null}}),
    ]
    .into_iter()
    .fold(String::new(), |mut stream, event| {
        stream.push_str("data: ");
        stream.push_str(&event.to_string());
        stream.push_str("\n\n");
        stream
    })
}

async fn enqueue_tool_loop_responses(inference: &MockPeer) -> TestResult {
    for body in [
        reasoning_function_call_sse(),
        success_sse().replace("grok-4.5", "grok-4.6-build"),
    ] {
        inference
            .responses
            .send(MockResponse {
                status: "200 OK",
                content_type: "text/event-stream",
                body,
                extra_headers: String::new(),
                disconnect_after_request: false,
                truncate_after: None,
                before_head: None,
                before_body: None,
            })
            .await?;
    }
    Ok(())
}

async fn assert_tool_loop_wire(inference: &MockPeer) -> TestResult {
    wait_for_requests(inference, 2).await?;
    let requests = inference.requests.lock().await;
    assert_eq!(requests.len(), 2, "two rounds must make exactly two writes");
    let first = captured_request_body(&requests[0])?;
    let second = captured_request_body(&requests[1])?;
    let affinity = test_cache_affinity()?.identity().to_string();
    for raw in [&requests[0], &requests[1]] {
        let head =
            std::str::from_utf8(&raw[..raw.len() - captured_request_body_bytes(raw)?.len()])?;
        assert!(
            head.to_ascii_lowercase()
                .contains(&format!("x-grok-conv-id: {affinity}"))
        );
    }
    for body in [&first, &second] {
        assert_eq!(body["model"], XAI_GROK_46_CODING_MODEL);
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert!(body.get("parallel_tool_calls").is_none());
        assert!(body.get("prompt_cache_key").is_none());
        assert!(body.get("previous_response_id").is_none());
    }
    assert_eq!(first["tools"][0]["name"], "fixture_tool");
    assert!(first["tools"][0].get("strict").is_none());
    let input = second["input"]
        .as_array()
        .ok_or_else(|| std::io::Error::other("second request input was not an array"))?;
    assert!(input.iter().any(|item| {
        item["type"] == "reasoning"
            && item["encrypted_content"] == "encrypted-tool-reasoning"
            && item.get("status").is_none()
    }));
    assert!(
        !input
            .iter()
            .any(|item| item.to_string().contains("historical-grok-4.5"))
    );
    assert!(input.iter().any(|item| {
        item["type"] == "function_call"
            && item["call_id"] == "call_native"
            && item["name"] == "fixture_tool"
            && item.get("id").is_none()
            && item.get("status").is_none()
    }));
    assert!(input.iter().any(|item| {
        item["type"] == "function_call_output"
            && item["call_id"] == "call_native"
            && item["output"].to_string().contains("fixture result")
            && item.get("id").is_none()
            && item.get("status").is_none()
    }));
    Ok(())
}

fn captured_request_body(request: &[u8]) -> TestResult<serde_json::Value> {
    Ok(serde_json::from_slice(captured_request_body_bytes(
        request,
    )?)?)
}

fn captured_request_body_bytes(request: &[u8]) -> TestResult<&[u8]> {
    let body_offset = request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| std::io::Error::other("captured request omitted HTTP head boundary"))?
        + 4;
    Ok(&request[body_offset..])
}

fn sse_response(body: String) -> MockResponse {
    MockResponse {
        status: "200 OK",
        content_type: "text/event-stream",
        body,
        extra_headers: String::new(),
        disconnect_after_request: false,
        truncate_after: None,
        before_head: None,
        before_body: None,
    }
}

fn assert_native_ok(response: &ModelResponse) {
    assert!(response.output().iter().any(|item| {
        item.blocks()
            .iter()
            .any(|block| matches!(block, ContentBlock::Text(text) if text.text() == "native ok"))
    }));
}

fn success_sse() -> String {
    [
        r#"data: {"type":"response.created","sequence_number":1,"response":{"id":"resp_native","status":"in_progress","model":"grok-4.5","output":[]}}\n\n"#,
        r#"data: {"type":"response.output_item.added","sequence_number":2,"output_index":0,"item":{"id":"msg_native","type":"message","status":"in_progress","role":"assistant","content":[]}}\n\n"#,
        r#"data: {"type":"response.content_part.added","sequence_number":3,"output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}\n\n"#,
        r#"data: {"type":"response.output_text.delta","sequence_number":4,"output_index":0,"content_index":0,"delta":"native ok"}\n\n"#,
        r#"data: {"type":"response.content_part.done","sequence_number":5,"output_index":0,"content_index":0,"part":{"type":"output_text","text":"native ok","annotations":[]}}\n\n"#,
        r#"data: {"type":"response.output_item.done","sequence_number":6,"output_index":0,"item":{"id":"msg_native","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":"native ok","annotations":[]}]}}\n\n"#,
        r#"data: {"type":"response.completed","sequence_number":7,"response":{"id":"resp_native","object":"response","status":"completed","model":"grok-4.5","output":[{"id":"msg_native","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":"native ok","annotations":[]}]}],"usage":{"input_tokens":4,"output_tokens":2,"total_tokens":6},"incomplete_details":null,"error":null}}\n\n"#,
    ].concat().replace("\\n", "\n")
}

fn reasoning_with_encrypted_sse(model: &str) -> String {
    let reasoning = json!({
        "id":"rs_native", "type":"reasoning", "status":"completed",
        "encrypted_content":"encrypted-reasoning-native",
        "summary":[{"type":"summary_text","text":"I inspected carefully"}]
    });
    let message = json!({
        "id":"msg_reasoned", "type":"message", "status":"completed", "role":"assistant",
        "content":[{"type":"output_text","text":"Use the bounded decoder.","annotations":[]}]
    });
    [
        json!({"type":"response.created","sequence_number":1,"response":{"id":"resp_reasoned","status":"in_progress","model":model,"output":[]}}),
        json!({"type":"response.output_item.added","sequence_number":2,"output_index":0,"item":{"id":"rs_native","type":"reasoning","status":"in_progress","summary":[]}}),
        json!({"type":"response.reasoning_summary_text.delta","sequence_number":3,"output_index":0,"summary_index":0,"delta":"I inspected carefully"}),
        json!({"type":"response.output_item.done","sequence_number":4,"output_index":0,"item":reasoning.clone()}),
        json!({"type":"response.output_item.added","sequence_number":5,"output_index":1,"item":{"id":"msg_reasoned","type":"message","status":"in_progress","role":"assistant","content":[]}}),
        json!({"type":"response.content_part.added","sequence_number":6,"output_index":1,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}),
        json!({"type":"response.output_text.delta","sequence_number":7,"output_index":1,"content_index":0,"delta":"Use the bounded decoder."}),
        json!({"type":"response.content_part.done","sequence_number":8,"output_index":1,"content_index":0,"part":{"type":"output_text","text":"Use the bounded decoder.","annotations":[]}}),
        json!({"type":"response.output_item.done","sequence_number":9,"output_index":1,"item":message.clone()}),
        json!({"type":"response.completed","sequence_number":10,"response":{"id":"resp_reasoned","object":"response","status":"completed","model":model,"output":[reasoning,message],"usage":{"input_tokens":6,"output_tokens":4,"total_tokens":10},"incomplete_details":null,"error":null}}),
    ]
    .into_iter()
    .fold(String::new(), |mut stream, event| {
        stream.push_str("data: ");
        stream.push_str(&event.to_string());
        stream.push_str("\n\n");
        stream
    })
}

fn reasoning_without_encrypted_sse(model: &str) -> String {
    reasoning_with_encrypted_sse(model)
        .replace("\"encrypted_content\":\"encrypted-reasoning-native\",", "")
        .replace(",\"encrypted_content\":\"encrypted-reasoning-native\"", "")
}

mod wrap_up;

/// A waiter queued behind a renewal that fails (here a transient catalog 503)
/// renews itself next. Its `checked_at` must still bound that renewal's stamp
/// although the failed renewer cleared the floor.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_waiter_behind_a_failed_renewal_keeps_its_floor() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let manager = manager(&temp, &auth)?;
    install_stored(
        &manager,
        "probe-floor-subject",
        "probe-floor-access-one",
        "probe-floor-refresh-one",
        system_unix_seconds()? + 3_600,
    )?;
    admit_catalog(&manager, &auth).await?;
    let admitted_at = backdate_admitted_catalog(&manager, 10)?;
    let inference = mock_peer().await?;
    let route =
        manager.test_subscription_route(crate::XaiOrigin::new(inference.origin.clone())?)?;
    let resolved = XaiResolvedModel::new(
        route,
        XaiModel::new(ModelId::new(XAI_GROK_PRIMARY_CODING_MODEL)?, None),
        GrokCodingProfile::pinned_compatibility(),
        now_timestamp()?,
    )?;
    let model = renewable_subscription_model(resolved, inference.client.clone())?;
    let rotated = rotate_selected_generation(
        &manager,
        "probe-floor-subject",
        "probe-floor-access-two",
        "probe-floor-refresh-two",
    )?;
    let renewer_checked_at = Timestamp::new(admitted_at.seconds() + 6, 500_000_000)?;
    let waiter_checked_at = Timestamp::new(admitted_at.seconds() + 4, 250_000_000)?;
    // The renewer's catalog fails transiently; the waiter's succeeds.
    auth.responses
        .send(raw_response(
            "503 Service Unavailable",
            "application/json",
            "{}",
        ))
        .await?;
    enqueue_admitted_catalog(&auth).await?;
    let (renewer, waiter) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            Arc::clone(&model).revalidate_runtime_authority(renewer_checked_at),
            Arc::clone(&model).revalidate_runtime_authority(waiter_checked_at),
        )
    })
    .await?;
    assert!(
        renewer.is_err(),
        "the renewer's catalog failed: {renewer:?}"
    );
    assert_eq!(
        auth.requests.lock().await.len(),
        3,
        "admission catalog, failed renewal catalog, waiter's renewal catalog"
    );
    let stamped = required(
        manager
            .core
            .catalog
            .lock()
            .map_err(|_| std::io::Error::other("catalog lock poisoned"))?
            .clone(),
        "renewed catalog",
    )?
    .public
    .observed_at;
    assert!(
        waiter.is_ok(),
        "the waiter's own renewal succeeded on the wire but answered {waiter:?}; \
         receipt stamped {} vs waiter checked_at {}",
        stamped.seconds(),
        waiter_checked_at.seconds()
    );
    assert_eq!(waiter?.credential_generation(), rotated.generation().get());
    auth.shutdown().await?;
    inference.abort_join().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retired_hex_epoch_record_needs_a_new_login_without_network() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let peer = mock_peer().await?;
    let manager = manager(&temp, &peer)?;
    let retired = StoredTokens {
        version: 2,
        issuer: XAI_ISSUER.to_owned(),
        client_id: XAI_PUBLIC_CLIENT_ID.to_owned(),
        subject: "retired-subject".to_owned(),
        scope: XAI_OAUTH_SCOPE.to_owned(),
        access_token: "retired-access".to_owned(),
        refresh_token: "retired-refresh".to_owned(),
        expires_at: system_unix_seconds()? + 1,
        selection_epoch: "ab".repeat(32),
    };
    manager.core.store.initialize_selected(&credential_record(
        AccountBinding::new("retired-subject")?,
        CredentialGeneration::new(1)?,
        &retired,
    )?)?;
    assert!(matches!(
        manager.status(),
        Err(XaiNativeAuthError::StoredCredentialFormatRetired)
    ));
    assert!(matches!(
        manager.subscription_usage().await,
        SubscriptionUsageRead::Unavailable {
            reason: SubscriptionUsageUnavailable::CredentialState(
                SubscriptionUsageCredentialState::LoginRequired
            ),
            ..
        }
    ));
    assert!(matches!(
        manager.refresh_if_needed().await,
        Err(XaiNativeAuthError::StoredCredentialFormatRetired)
    ));
    let restored = required(manager.core.store.load()?, "restored retired fence")?;
    assert_eq!(restored.status(), CredentialStatus::Ready);
    assert!(matches!(
        manager.refresh_catalog().await,
        Err(XaiNativeAuthError::StoredCredentialFormatRetired)
    ));
    drop(manager);

    // Credential-free offline status reads the record exactly as the manager
    // refuses it, and offline local logout is its repair.
    let path = temp.path().join("credentials").join("xai.json");
    let offline = XaiOfflineSelectedSlot::open_explicit(
        path.clone(),
        CredentialSlotId::new("test-selected")?,
    )?;
    assert!(matches!(
        offline.status(),
        Err(XaiNativeAuthError::StoredCredentialFormatRetired)
    ));
    assert_eq!(offline.logout_local()?, XaiOfflineCredentialStatus::Revoked);
    assert_eq!(offline.status()?, XaiOfflineCredentialStatus::Revoked);
    let reopened = XaiNativeAuthManager::open_test_slot(path, peer.client.clone(), &peer.origin)?;
    let reopened = required(reopened.status()?, "retired tombstone")?;
    assert_eq!(reopened.status, CredentialStatus::Revoked);
    assert!(peer.requests.lock().await.is_empty());
    peer.shutdown().await?;
    Ok(())
}

mod retrieval;
