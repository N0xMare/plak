//! Session continuity under gateway custody (spec 01 §7.1): a login bound
//! under direct custody reopens through an in-process gateway over the same
//! store with an equal binding, and its exchanges (and a key-holding route's)
//! reach the upstream byte for byte as they do directly.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use plakous_model::{
    Actor, AttemptId, BranchId, CancellationToken, ContentBlock, ConversationItem,
    ConversationView, EntryId, GenerationLimits, ModelCacheAffinity, ModelCallContext,
    ModelOperationRoot, ModelPreparationContext, ModelRequest, Provenance, RequestMetadata, RunId,
    SessionId, SessionLineage, TextBlock, TurnId, ValidationMode,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use uuid::Uuid;

use crate::gateway::GatewayCustody;
use crate::gateway::pins::openai::{CODEX_CREDENTIAL_HEADERS, CODEX_UPSTREAM_PINS};
use crate::gateway::server::{InProcessGateway, LocalCustody, new_instance};
use crate::subscriptions::custody::SubscriptionCustodian;
use crate::subscriptions::state::tests::child_catalog::{MODEL, seed, selection};
use crate::subscriptions::{
    NativeProvider, NativeSubscriptionError, NativeSubscriptions, ResolvedProvider,
    SubscriptionPaths, SubscriptionRequestLimits,
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

const CHATGPT: &str = "https://chatgpt.com/";

/// A loopback TLS upstream recording each whole request: the Codex and
/// SuperGrok catalog and usage reads answer their fixtures, every other
/// request a final `400`.
struct Upstream {
    client: plakous_http::Client,
    origin: plakous_http::Origin,
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
}

/// How a fixture upstream answers one whole request: status line,
/// content type and body.
type Respond = Arc<dyn Fn(&[u8]) -> (&'static str, &'static str, Vec<u8>) + Send + Sync>;

async fn upstream() -> TestResult<Upstream> {
    upstream_with(Arc::new(|request: &[u8]| {
        let (status, body) = fixture_reply(request);
        (status, "application/json", body)
    }))
    .await
}

fn fixture_reply(request: &[u8]) -> (&'static str, Vec<u8>) {
    if request.starts_with(b"GET /backend-api/codex/models?") {
        ("200 OK", catalog_body())
    } else if request.starts_with(b"GET /backend-api/wham/usage ") {
        ("200 OK", usage_body())
    } else if request.starts_with(b"GET /v1/models ") {
        ("200 OK", grok_catalog_body())
    } else if request.starts_with(b"GET /v1/billing?format=credits ") {
        ("200 OK", grok_usage_body())
    } else {
        (
            "400 Bad Request",
            br#"{"error":{"message":"fixture refusal","type":"invalid_request_error"}}"#.to_vec(),
        )
    }
}

/// A loopback TLS upstream answering each whole request with `respond`.
async fn upstream_with(respond: Respond) -> TestResult<Upstream> {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()])?;
    let certificate = cert.der().clone();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
        signing_key.serialize_der(),
    ));
    let server = rustls::ServerConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(vec![certificate.clone()], key)?;
    let mut roots = rustls::RootCertStore::empty();
    roots.add(certificate.clone())?;
    let tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let identity = plakous_http::TlsConfigIdentity::from_parts(1, &[certificate.as_ref()])?;
    let client = plakous_http::Client::with_tls_config_and_identity(
        plakous_http::Limits::default(),
        tls,
        identity,
    )?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let origin = plakous_http::Origin::parse(&format!("https://{}/", listener.local_addr()?))?;
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            let recorded = Arc::clone(&recorded);
            let respond = Arc::clone(&respond);
            tokio::spawn(async move {
                let Ok(mut stream) = acceptor.accept(stream).await else {
                    return;
                };
                let mut request = Vec::new();
                let mut buffer = [0_u8; 8192];
                loop {
                    if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                        let length = head
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length: "))
                            .and_then(|value| value.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                    match stream.read(&mut buffer).await {
                        Ok(0) | Err(_) => return,
                        Ok(read) => request.extend_from_slice(&buffer[..read]),
                    }
                }
                let (status, content_type, body) = respond(&request);
                recorded
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(request);
                let head = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(&body).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    Ok(Upstream {
        client,
        origin,
        requests,
    })
}

fn catalog_body() -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({"models": [{
        "slug": MODEL, "visibility": "list", "priority": 1,
        "supported_in_api": true, "tool_mode": "direct",
        "supported_reasoning_levels": [{"effort": "low"}],
        "shell_type": "shell_command", "input_modalities": ["text"],
        "context_window": 272_000, "max_context_window": 1_000_000,
        "effective_context_window_percent": 95, "auto_compact_token_limit": 240_000
    }]}))
    .unwrap_or_default()
}

fn usage_body() -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({"rate_limit":{"primary_window":{
        "used_percent":12,"limit_window_seconds":18_000,"reset_at":4_000_000_000_u64
    }}}))
    .unwrap_or_default()
}

fn grok_catalog_body() -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({"object": "list", "data": [{
        "id": GROK_MODEL, "object": "model", "created": 1, "owned_by": "xai",
        "contextWindow": 256_000
    }]}))
    .unwrap_or_default()
}

fn grok_usage_body() -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({"config": {
        "creditUsagePercent": 12.25,
        "currentPeriod": {
            "type": "USAGE_PERIOD_TYPE_WEEKLY",
            "start": "2020-01-01T00:00:00Z",
            "end": "2099-01-01T00:00:00Z"
        }
    }}))
    .unwrap_or_default()
}

/// The SuperGrok model the fixture catalog admits.
const GROK_MODEL: &str = "grok-4.5";

impl Upstream {
    fn take(&self) -> Vec<Vec<u8>> {
        std::mem::take(
            &mut *self
                .requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

fn limits() -> TestResult<SubscriptionRequestLimits> {
    Ok(SubscriptionRequestLimits::new(
        8,
        3,
        4,
        1_000_000,
        Some(Duration::from_secs(300)),
        0,
    )?)
}

/// The non-secret session binding a host freezes for a resolved provider.
fn binding(provider: &ResolvedProvider) -> TestResult<impl PartialEq + std::fmt::Debug> {
    let authority = provider.initial_authority();
    Ok((
        provider.configuration().selected_identity(),
        provider.configuration().effective_identity(),
        authority.account_binding(),
        authority.credential_authority_binding(),
        authority.credential_generation(),
        provider.credential_selection_binding()?,
        provider.adapter_revision().to_owned(),
    ))
}

/// One model round through `provider`'s own model, which the fixture
/// refuses after recording its request.
async fn dispatch_once(provider: &ResolvedProvider) -> TestResult {
    let configured = provider.configured_model();
    let model = provider.model();
    let authority = provider.initial_authority().clone();
    let observed = authority.observed_at();
    let turn = TurnId::try_from(Uuid::from_u128(2))?;
    let attempt = AttemptId::try_from(Uuid::from_u128(3))?;
    let request = ModelRequest::builder(
        configured.configuration().selected_identity(),
        configured.configuration().effective_identity(),
        ConversationView::from_items(vec![ConversationItem::new(
            EntryId::try_from(Uuid::from_u128(6))?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("fixture prompt", None)?)],
            Provenance::UserInput,
        )?])?,
        RequestMetadata::for_turn(
            RunId::try_from(Uuid::from_u128(1))?,
            turn,
            attempt,
            observed,
        ),
    )
    .limits(GenerationLimits::new(None, Vec::new())?)
    .resolved_harness_plan_identity(plakous_model::Digest::sha256(
        plakous_model::DigestDomain::ResolvedHarnessPlan,
        1,
        &[b"fixture-harness-plan"],
    )?)
    .build()?;
    let plan = provider
        .model
        .descriptor()
        .validate_request(request, ValidationMode::Strict)
        .map_err(|error| format!("validate: {error}"))?;
    let _root = Arc::clone(&model).begin_operation_root(ModelOperationRoot::Turn(turn))?;
    let call = Arc::clone(&model)
        .prepare_call(
            plan,
            ModelPreparationContext::new(authority, observed).with_cache_affinity(
                ModelCacheAffinity::from_runtime_entropy(
                    SessionId::try_from(Uuid::from_u128(4))?,
                    BranchId::try_from(Uuid::from_u128(5))?,
                    1,
                    [7; 32],
                )?,
            ),
        )
        .map_err(|error| format!("prepare: {error}"))?;
    if let Ok(mut stream) = call
        .start(ModelCallContext::new(
            attempt,
            CancellationToken::new(),
            None,
        ))
        .await
    {
        use futures_util::StreamExt as _;
        while stream.next().await.is_some() {}
    }
    Ok(())
}

/// Subscriptions over `paths` on `provider`'s profile test clock.
fn at_review(
    paths: &SubscriptionPaths,
    provider: NativeProvider,
) -> TestResult<NativeSubscriptions> {
    let now = NativeSubscriptions::notice(provider)?.reviewed_at() + 60;
    Ok(NativeSubscriptions::new(paths.clone()).with_clock(Arc::new(
        plakous_model::FixedClock::new(plakous_model::Timestamp::new(now, 0)?),
    )))
}

/// The model's own runtime revalidation at its catalog observation, on the
/// profile's clock: the custodied facts are read and every synchronous
/// check passes.
async fn revalidate(provider: &ResolvedProvider) -> TestResult {
    let observed = provider.initial_authority().observed_at();
    provider
        .model()
        .revalidate_runtime_authority(observed)
        .await?;
    Ok(())
}

#[cfg(feature = "anthropic")]
mod anthropic_api;
#[cfg(feature = "claude")]
mod claude;
#[cfg(feature = "grok")]
mod grok;
#[cfg(feature = "openrouter")]
mod openrouter;
#[cfg(feature = "xai")]
mod xai_api;

/// Custody through an in-process gateway that serves the Codex logins of
/// `paths` and dials the production origin at `upstream`.
fn gateway_custody(
    paths: &SubscriptionPaths,
    upstream: &Upstream,
) -> TestResult<crate::custody::Custody> {
    let custodian = SubscriptionCustodian::new(
        NativeSubscriptions::new(paths.clone()),
        NativeProvider::Codex,
    );
    let custody = LocalCustody::new(new_instance().ok_or("gateway instance")?).with_kind(
        "codex",
        Arc::new(custodian),
        CODEX_UPSTREAM_PINS,
    );
    let server = InProcessGateway::server(custody, upstream.client.clone())
        .with_dial_override("https://chatgpt.com", upstream.origin.clone());
    let gateway = GatewayCustody::connect(InProcessGateway::endpoint_for(&server), None, |kind| {
        (kind == "codex").then_some(CODEX_CREDENTIAL_HEADERS)
    })?;
    Ok(gateway.custody())
}

#[tokio::test]
async fn a_codex_session_reopens_through_a_gateway_with_an_equal_binding_and_identical_bytes()
-> TestResult {
    let root = tempfile::tempdir()?;
    let paths = SubscriptionPaths::from_xdg_state_home(root.path())?;
    seed(&paths, super::super::unix_now()?)?;
    let upstream = upstream().await?;

    // Bound under direct custody: the route keeps its production origin and
    // its client dials the fixture, exactly as the gateway's upstream does.
    let mut direct = NativeSubscriptions::new(paths.clone());
    let dialing = plakous_http::Client::new_http1(plakous_http::Limits::default())?
        .with_test_dial_override(
            plakous_http::Origin::parse(CHATGPT)?,
            upstream.origin.clone(),
            upstream.client.clone(),
        );
    direct.codex_test_transport = Some((dialing, plakous_openai::EndpointOrigin::parse(CHATGPT)?));
    let bound = direct
        .resolve_turn_scoped_provider(selection()?, limits()?, SessionLineage::Root)
        .await?;
    dispatch_once(&bound).await?;
    let usage_direct = direct.subscription_usage(NativeProvider::Codex).await;
    let direct_requests = upstream.take();

    // Reopened through a gateway over the same store.
    let gateway =
        NativeSubscriptions::new(paths.clone()).with_custody(&gateway_custody(&paths, &upstream)?);
    let status = gateway.status(NativeProvider::Codex).await?;
    assert_eq!(status, direct.status(NativeProvider::Codex).await?);
    let reopened = gateway
        .resolve_turn_scoped_provider(selection()?, limits()?, SessionLineage::Root)
        .await?;
    assert_eq!(binding(&reopened)?, binding(&bound)?);
    dispatch_once(&reopened).await?;
    let usage_gateway = gateway.subscription_usage(NativeProvider::Codex).await;
    let gateway_requests = upstream.take();

    // Catalog GET, Responses POST and usage GET, in order, byte for byte
    // with the same header order.
    assert_eq!(direct_requests.len(), 3, "{direct_requests:?}");
    assert_eq!(gateway_requests.len(), direct_requests.len());
    for (direct, relayed) in direct_requests.iter().zip(&gateway_requests) {
        assert_eq!(
            String::from_utf8_lossy(relayed),
            String::from_utf8_lossy(direct)
        );
    }
    assert!(
        direct_requests[1].starts_with(b"POST /backend-api/codex/responses "),
        "{}",
        String::from_utf8_lossy(&direct_requests[1])
    );
    for usage in [usage_direct, usage_gateway] {
        assert!(
            matches!(usage, plakous_model::SubscriptionUsageRead::Snapshot(_)),
            "{usage:?}"
        );
    }

    // The gateway host never opened or wrote the store, and its store
    // operations refuse.
    assert_eq!(
        gateway.named_accounts(NativeProvider::Codex).await.err(),
        Some(NativeSubscriptionError::GatewayCustody)
    );
    assert_eq!(
        gateway.logout_local(NativeProvider::Codex).await.err(),
        Some(NativeSubscriptionError::GatewayCustody)
    );
    Ok(())
}

#[tokio::test]
async fn a_gateway_host_never_creates_the_subscription_store() -> TestResult {
    let root = tempfile::tempdir()?;
    let gateway_paths = SubscriptionPaths::from_xdg_state_home(root.path().join("gateway"))?;
    seed(&gateway_paths, super::super::unix_now()?)?;
    let upstream = upstream().await?;
    let host_root = root.path().join("host");
    let custody = gateway_custody(&gateway_paths, &upstream)?;
    // The host's own paths name an absent root; it is never created.
    let host = NativeSubscriptions::new(SubscriptionPaths::from_xdg_state_home(&host_root)?)
        .with_custody(&custody);
    let gateway = NativeSubscriptions::new(gateway_paths).with_custody(&custody);
    assert_eq!(
        host.status(NativeProvider::Codex).await?,
        gateway.status(NativeProvider::Codex).await?
    );
    host.resolve_turn_scoped_provider(selection()?, limits()?, SessionLineage::Root)
        .await?;
    assert!(!host_root.exists());
    Ok(())
}

#[tokio::test]
async fn a_dispatch_and_a_rejection_during_a_claimed_refresh_answer_the_settled_login() -> TestResult
{
    use plakous_auth::custody::{
        CredentialCustodian as _, CustodySlot, CustodyStatus, RefreshRequest,
    };

    let root = tempfile::tempdir()?;
    let paths = SubscriptionPaths::from_xdg_state_home(root.path())?;
    seed(&paths, super::super::unix_now()?)?;
    let custodian = Arc::new(SubscriptionCustodian::new(
        NativeSubscriptions::new(paths.clone()),
        NativeProvider::Codex,
    ));
    let slot = CustodySlot::new("codex")?;
    let facts = custodian.facts(&slot).await?;
    let expected = facts.expected_fence().ok_or("no fence")?;

    // Another holder claims a refresh of that generation and holds it.
    let store = crate::subscriptions::state::tests::child_catalog::codex_store(&paths)?;
    let fence = store.load()?.ok_or("no record")?;
    let payload = {
        let lease = store.acquire_ready(&fence)?;
        lease.payload().expose_secret().to_vec()
    };
    let claim = store.begin_refresh(&fence)?.start();
    assert_eq!(
        custodian.facts(&slot).await?.status,
        CustodyStatus::Refreshing
    );

    // The dispatch-time refresh and a 401's refresh arrive during the claim.
    let dispatch = {
        let (custodian, slot) = (Arc::clone(&custodian), slot.clone());
        tokio::spawn(async move {
            custodian
                .refresh(&slot, RefreshRequest::IfDue { expected })
                .await
        })
    };
    let rejection = {
        let (custodian, slot) = (Arc::clone(&custodian), slot.clone());
        let selection_epoch_digest = facts.selection_epoch_digest;
        tokio::spawn(async move {
            custodian
                .refresh(
                    &slot,
                    RefreshRequest::AfterRejection {
                        rejected: expected,
                        selection_epoch_digest,
                    },
                )
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!dispatch.is_finished(), "the dispatch waits for the claim");
    let settled = claim.fence().generation().next()?;
    store.complete_refresh(
        claim,
        plakous_auth::CredentialRecord::new(
            fence.profile_id().clone(),
            fence.account_binding().clone(),
            settled,
            plakous_auth::CredentialStatus::Ready,
            plakous_auth::SecretBytes::new(payload),
        )?,
    )?;
    for answer in [dispatch.await??, rejection.await??] {
        assert_eq!(answer.status, CustodyStatus::Ready);
        assert_eq!(answer.generation, Some(settled));
        assert_eq!(answer.account_binding, Some(expected.account_binding));
    }
    Ok(())
}

#[tokio::test]
async fn a_quarantined_login_reports_its_status_and_refuses_fields_not_ready() -> TestResult {
    use plakous_auth::custody::{
        CredentialCustodian as _, CustodyError, CustodySlot, CustodyStatus,
    };
    use plakous_gateway_protocol::facts::CredentialStatus;
    use plakous_gateway_protocol::refusal::GatewayRefusal;

    let root = tempfile::tempdir()?;
    let paths = SubscriptionPaths::from_xdg_state_home(root.path())?;
    seed(&paths, super::super::unix_now()?)?;
    let custodian = SubscriptionCustodian::new(
        NativeSubscriptions::new(paths.clone()),
        NativeProvider::Codex,
    );
    let slot = CustodySlot::new("codex")?;
    let expected = custodian
        .facts(&slot)
        .await?
        .expected_fence()
        .ok_or("no fence")?;

    // A refresh claimed and then abandoned leaves the login quarantined.
    let store = crate::subscriptions::state::tests::child_catalog::codex_store(&paths)?;
    drop(
        store
            .begin_refresh(&store.load()?.ok_or("no record")?)?
            .start(),
    );

    let facts = custodian.facts(&slot).await?;
    assert_eq!(facts.status, CustodyStatus::Quarantined);
    assert!(facts.profile.is_some());
    let refusal = custodian.fields(&slot, &expected).await.err();
    let Some(CustodyError::NotReady { facts }) = &refusal else {
        return Err(format!("a quarantined login supplied fields: {refusal:?}").into());
    };
    assert_eq!(facts.status, CustodyStatus::Quarantined);
    let instance = new_instance().ok_or("instance")?;
    assert_eq!(
        plakous_auth::custody::gateway::gateway_refusal(
            refusal.as_ref().ok_or("refusal")?,
            &instance
        ),
        GatewayRefusal::CredentialNotReady {
            status: CredentialStatus::Quarantined
        }
    );
    Ok(())
}
