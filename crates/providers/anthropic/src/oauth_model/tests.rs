#![allow(
    clippy::too_many_lines,
    reason = "Exercise Claude OAuth preparation and same-account relogin across complete replay and context-denial scenarios."
)]
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use plakous_auth::subscription_limit::{
    InMemorySubscriptionRequestGate, SubscriptionRequestClass, SubscriptionRequestGate,
    SubscriptionRequestLimits, SystemSubscriptionGateClock,
};
use plakous_auth::{CredentialSlotId, CredentialStatus};
use plakous_http::Limits;
use plakous_model::{
    Actor, AttemptId, ContentBlock, ConversationItem, ConversationView,
    EffectiveProviderConfigurationId, EntryId, ModelRequest, Provenance, RequestMetadata, RunId,
    SelectedProviderConfigurationId, TextBlock, TurnId, ValidationMode,
};
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_rustls::{TlsAcceptor, server::TlsStream};
use uuid::Uuid;

use super::*;
use crate::oauth::{
    CLAUDE_OAUTH_INFERENCE_SCOPE, CLAUDE_OAUTH_KILL_SWITCH_REVISION, CLAUDE_OAUTH_NOTICE_REVISION,
    CLAUDE_OAUTH_REVIEWED_AT_UNIX, CLAUDE_OAUTH_SOURCE_REVISION,
};
use crate::{AnthropicMessagesTargetBinding, ClaudeCompatibilityAcceptance};

pub(super) const TEST_GRANT_BINDING: &str = "12345678-1234-4234-8234-123456789abc";

#[test]
fn claude_oauth_default_dispatch_budget_is_the_native_route_budget() {
    assert_eq!(DEFAULT_TIMEOUT_SECS, 300);
    let deadline = AnthropicHttpDeadline::new(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
        .expect("native dispatch budget");
    assert_eq!(deadline.duration(), Duration::from_secs(300));
}

fn test_context_authority() -> Digest {
    RuntimeModelAuthority::authority_class_for(
        digest_binding(
            b"claude-oauth-provider-account",
            TEST_GRANT_BINDING.as_bytes(),
        )
        .expect("account binding"),
        digest_binding(
            b"claude-oauth-provider-account-credential",
            TEST_GRANT_BINDING.as_bytes(),
        )
        .expect("credential binding"),
    )
    .expect("context authority")
}

pub(super) fn private_store(temp: &tempfile::TempDir) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let directory = temp.path().join("credentials");
    std::fs::create_dir(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    directory.join("claude.json")
}

pub(super) fn manager(path: PathBuf) -> ClaudeNativeAuthManager {
    manager_with_client(path, Client::new_http1(Limits::default()).unwrap())
}

pub(super) fn manager_with_client(path: PathBuf, client: Client) -> ClaudeNativeAuthManager {
    let now = Timestamp::new(CLAUDE_OAUTH_REVIEWED_AT_UNIX, 0).unwrap();
    let acceptance = ClaudeCompatibilityAcceptance::accept_exact(
        CLAUDE_OAUTH_COMPATIBILITY_PROFILE_ID,
        CLAUDE_OAUTH_SOURCE_REVISION,
        CLAUDE_OAUTH_NOTICE_REVISION,
        CLAUDE_OAUTH_KILL_SWITCH_REVISION,
        now,
    )
    .unwrap();
    let slot = CredentialSlotId::new("primary").unwrap();
    ClaudeNativeAuthManager::open_selected_slot(
        path,
        slot,
        client,
        acceptance,
        crate::ClaudeClientDeviceId::new("ab".repeat(32)).unwrap(),
    )
    .unwrap()
}

async fn read_local_request(stream: &mut TlsStream<TcpStream>) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 2048];
    let head_end = loop {
        let count = stream.read(&mut buffer).await.unwrap();
        assert!(count > 0, "request head ended early");
        request.extend_from_slice(&buffer[..count]);
        if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let head = String::from_utf8_lossy(&request[..head_end]);
    let content_length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    while request.len() < head_end + content_length {
        let count = stream.read(&mut buffer).await.unwrap();
        assert!(count > 0, "request body ended early");
        request.extend_from_slice(&buffer[..count]);
    }
    request
}

pub(super) async fn local_peer(
    responses: Vec<Vec<u8>>,
) -> (Client, Origin, JoinHandle<Vec<Vec<u8>>>) {
    local_peer_with_first_response_gate(responses, None).await
}

pub(super) async fn local_peer_with_first_response_gate(
    responses: Vec<Vec<u8>>,
    gate: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
) -> (Client, Origin, JoinHandle<Vec<Vec<u8>>>) {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["localhost".to_owned(), "127.0.0.1".to_owned()]).unwrap();
    let certificate = cert.der().clone();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
    let mut server = ServerConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![certificate.clone()], private_key)
        .unwrap();
    server.alpn_protocols = vec![b"http/1.1".to_vec()];
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(certificate.to_vec()))
        .unwrap();
    let client_config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let client = Client::with_tls_config_and_identity(
        Limits::default(),
        client_config,
        plakous_http::TlsConfigIdentity::from_parts(
            1,
            &[b"claude-oauth-model-local-v1", certificate.as_ref()],
        )
        .unwrap(),
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = Origin::parse(&format!("https://{}", listener.local_addr().unwrap())).unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(server));
    let peer = tokio::spawn(async move {
        let mut requests = Vec::new();
        for (index, response) in responses.into_iter().enumerate() {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = acceptor.accept(stream).await.unwrap();
            requests.push(read_local_request(&mut stream).await);
            if index == 0
                && let Some((observed, release)) = &gate
            {
                observed.notify_one();
                release.notified().await;
            }
            if response.is_empty() {
                // Read the request, then drop TLS without `close_notify`.
                drop(stream);
                continue;
            }
            stream.write_all(&response).await.unwrap();
            let _ = stream.shutdown().await;
        }
        requests
    });
    (client, origin, peer)
}

pub(super) fn inference_gate(inference: u32) -> Arc<InMemorySubscriptionRequestGate> {
    request_gate(0, inference)
}

pub(super) fn request_gate(refresh: u32, inference: u32) -> Arc<InMemorySubscriptionRequestGate> {
    let limits = SubscriptionRequestLimits::new(
        1,
        refresh,
        inference,
        1_000_000,
        Some(Duration::from_secs(300)),
        0,
    )
    .unwrap();
    Arc::new(InMemorySubscriptionRequestGate::new(
        limits,
        Arc::new(SystemSubscriptionGateClock::default()),
    ))
}

pub(super) fn local_response(status: u16, content_type: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} Local\r\ncontent-type: {content_type}\r\nrequest-id: req_oauth_local\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    ).into_bytes()
}

pub(super) const LOCAL_SUCCESS_SSE: &str = concat!(
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

async fn start_local_model(
    manager: ClaudeNativeAuthManager,
    client: Client,
    origin: Origin,
    workflow: bool,
) -> Result<Vec<plakous_model::ModelEvent>, Box<dyn std::error::Error>> {
    let (_, authority, descriptor) = manager
        .resolve_model("claude-sonnet-5", client.clone(), SessionLineage::Root)
        .await?;
    let now = authority.observed_at();
    let ((token, _, _), identity) = manager.identity_credentials()?;
    let route = AnthropicPlatformRoute::claude_oauth(
        ApiKey::new(token)?,
        AnthropicAccountIdentity::new(identity.account_uuid().as_str())?,
    )?;
    let resolved = oauth_resolved_target(
        route,
        "claude-sonnet-5",
        authority.authority_class_binding()?,
        now,
        pin_expires_at(now)?,
    )?
    .with_session_lineage(SessionLineage::Root);
    assert_eq!(
        resolved.route().origin(),
        crate::oauth::CLAUDE_OAUTH_MESSAGES_ORIGIN
    );
    let mut model = ClaudeOAuthLanguageModel::new(
        resolved,
        client,
        authority.clone(),
        manager.clone(),
        manager.request_identity()?,
    );
    model.test_dial_origin = Some(origin);
    let request = if workflow {
        workflow_request(now)
    } else {
        request(now)
    };
    let plan = descriptor.validate_request(request, ValidationMode::Strict)?;
    let prepared = Arc::new(model).prepare_call(
        plan,
        ModelPreparationContext::new(authority, now)
            .with_cache_affinity(test_affinity(99, 0))
            .with_turn_position(plakous_model::TurnPosition::new(7, 9)),
    )?;
    let mut stream = prepared
        .start(ModelCallContext::new(
            AttemptId::try_from(Uuid::from_u128(4))?,
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

fn configuration_ids() -> (
    SelectedProviderConfigurationId,
    EffectiveProviderConfigurationId,
) {
    let selected = Digest::sha256(
        DigestDomain::SelectedProviderConfiguration,
        5,
        &[b"claude-oauth-context-test-selected"],
    )
    .expect("selected identity");
    let effective = Digest::sha256(
        DigestDomain::EffectiveProviderConfiguration,
        5,
        &[b"claude-oauth-context-test-effective"],
    )
    .expect("effective identity");
    (
        serde_json::from_value(serde_json::to_value(selected).expect("selected JSON"))
            .expect("selected configuration ID"),
        serde_json::from_value(serde_json::to_value(effective).expect("effective JSON"))
            .expect("effective configuration ID"),
    )
}

fn test_affinity(session: u128, epoch: u64) -> plakous_model::ModelCacheAffinity {
    plakous_model::ModelCacheAffinity::from_runtime_entropy(
        plakous_model::SessionId::try_from(Uuid::from_u128(session)).unwrap(),
        plakous_model::BranchId::try_from(Uuid::from_u128(100)).unwrap(),
        epoch,
        [1; 32],
    )
    .unwrap()
}

fn request(created_at: Timestamp) -> ModelRequest {
    request_with_metadata(RequestMetadata::for_turn(
        RunId::try_from(Uuid::from_u128(2)).expect("run ID"),
        TurnId::try_from(Uuid::from_u128(3)).expect("turn ID"),
        AttemptId::try_from(Uuid::from_u128(4)).expect("attempt ID"),
        created_at,
    ))
}

fn workflow_request(created_at: Timestamp) -> ModelRequest {
    request_with_metadata(RequestMetadata::for_session_workflow(
        RunId::try_from(Uuid::from_u128(2)).expect("run ID"),
        plakous_model::SessionWorkflowRootId::new([5; 32]).expect("workflow ID"),
        AttemptId::try_from(Uuid::from_u128(4)).expect("attempt ID"),
        created_at,
    ))
}

fn request_with_metadata(metadata: RequestMetadata) -> ModelRequest {
    request_with_text(metadata, "count the complete lowered body")
}

fn request_with_text(metadata: RequestMetadata, text: &str) -> ModelRequest {
    let (selected, effective) = configuration_ids();
    ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(vec![
            ConversationItem::new(
                EntryId::try_from(Uuid::from_u128(1)).expect("entry ID"),
                Actor::User,
                text.as_bytes()
                    .chunks(250_000)
                    .map(|chunk| {
                        ContentBlock::Text(
                            TextBlock::new(std::str::from_utf8(chunk).unwrap(), None)
                                .expect("text"),
                        )
                    })
                    .collect(),
                Provenance::UserInput,
            )
            .expect("conversation item"),
        ])
        .expect("test conversation items form one valid segment"),
        metadata,
    )
    .build()
    .expect("request")
}

#[tokio::test]
async fn same_account_relogin_replays_persisted_thinking_without_reusing_stale_plans() {
    use plakous_model::{
        CapabilityRequirement, FixedClock, HistorySegment, HistorySegmentKind, ReasoningEffort,
        ReasoningPolicy, SystemIdGenerator,
    };

    let now = Timestamp::new(CLAUDE_OAUTH_REVIEWED_AT_UNIX, 0).unwrap();
    let later = Timestamp::new(CLAUDE_OAUTH_REVIEWED_AT_UNIX + 60, 0).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let manager = manager(private_store(&temp));
    manager
        .persist_test_grant(
            "first-access",
            "first-refresh",
            3600,
            CLAUDE_OAUTH_INFERENCE_SCOPE,
        )
        .unwrap();
    let (before, _, _) = manager
        .resolve_oauth_model(
            "claude-opus-5",
            Client::new_http1(Limits::default()).unwrap(),
            SessionLineage::Root,
        )
        .await
        .unwrap();
    manager
        .persist_test_grant(
            "second-access",
            "second-refresh",
            3600,
            CLAUDE_OAUTH_INFERENCE_SCOPE,
        )
        .unwrap();
    let (after, _, _) = manager
        .resolve_oauth_model(
            "claude-opus-5",
            Client::new_http1(Limits::default()).unwrap(),
            SessionLineage::Root,
        )
        .await
        .unwrap();
    assert_eq!(
        before
            .resolved
            .descriptor()
            .semantic_binding_digest()
            .unwrap(),
        after
            .resolved
            .descriptor()
            .semantic_binding_digest()
            .unwrap()
    );
    let route = AnthropicPlatformRoute::claude_oauth(
        ApiKey::new("oauth-access-token-fixture").unwrap(),
        AnthropicAccountIdentity::new(after.identity.account_uuid().as_str()).unwrap(),
    )
    .unwrap();
    let resolve = |time| {
        oauth_resolved_target(
            route.clone(),
            "claude-opus-5",
            test_context_authority(),
            time,
            pin_expires_at(time).unwrap(),
        )
        .unwrap()
    };
    let original = resolve(now);
    let refreshed = resolve(later);
    assert_ne!(
        original.descriptor().binding_digest().unwrap(),
        refreshed.descriptor().binding_digest().unwrap()
    );
    assert_eq!(
        original.descriptor().semantic_binding_digest().unwrap(),
        refreshed.descriptor().semantic_binding_digest().unwrap()
    );
    assert_ne!(
        AnthropicMessagesTargetBinding::from_resolved(&original).unwrap(),
        AnthropicMessagesTargetBinding::from_resolved(&refreshed).unwrap()
    );
    let build = |items| {
        let (selected, effective) = configuration_ids();
        ModelRequest::builder(
            selected,
            effective,
            ConversationView::from_items(items)
                .expect("test conversation items form one valid segment"),
            request(now).metadata(),
        )
        .reasoning(ReasoningPolicy::Enabled {
            effort: Some(ReasoningEffort::Medium),
            requirement: CapabilityRequirement::Required,
        })
        .build()
        .unwrap()
    };
    let user = ConversationItem::new(
        EntryId::try_from(Uuid::from_u128(101)).unwrap(),
        Actor::User,
        vec![ContentBlock::Text(
            TextBlock::new("continue fixture", None).unwrap(),
        )],
        Provenance::UserInput,
    )
    .unwrap();
    let first_plan = original
        .descriptor()
        .validate_request(build(vec![user.clone()]), ValidationMode::Strict)
        .unwrap();
    let prepared = MessagesRequestTranslator::new(original.clone())
        .with_oauth_identity(crate::ClaudeOAuthRequestIdentity::new(
            crate::ClaudeAccountUuid::new("12345678-1234-4234-8234-123456789abc").unwrap(),
            crate::ClaudeClientDeviceId::new("ab".repeat(32)).unwrap(),
        ))
        .with_session_id(Some(test_affinity(99, 0).session()))
        .translate(&first_plan, ToolOwnership::default())
        .unwrap();
    assert!(
        MessagesRequestTranslator::new(refreshed.clone())
            .with_oauth_identity(crate::ClaudeOAuthRequestIdentity::new(
                crate::ClaudeAccountUuid::new("12345678-1234-4234-8234-123456789abc").unwrap(),
                crate::ClaudeClientDeviceId::new("ab".repeat(32)).unwrap()
            ))
            .with_session_id(Some(test_affinity(99, 0).session()))
            .translate(&first_plan, ToolOwnership::default())
            .is_err()
    );

    for (kind, thinking) in [
        ("thinking", ""),
        ("thinking", "visible fixture"),
        ("redacted_thinking", ""),
    ] {
        let mut decoder = crate::AnthropicMessagesStreamDecoder::new(
            prepared.clone(),
            Arc::new(FixedClock::new(now)),
            Arc::new(SystemIdGenerator),
        )
        .unwrap();
        let mut events = vec![serde_json::json!({
            "type":"message_start", "message": {"id":"msg_fixture", "type":"message", "role":"assistant", "model":"claude-opus-5", "content":[], "stop_reason":null, "stop_sequence":null, "usage":{"input_tokens":1,"output_tokens":0}}
        })];
        let block = if kind == "thinking" {
            serde_json::json!({"type":"thinking", "thinking":""})
        } else {
            serde_json::json!({"type":"redacted_thinking", "data":"opaque-redacted-fixture"})
        };
        events.push(
            serde_json::json!({"type":"content_block_start", "index":0, "content_block":block}),
        );
        if kind == "thinking" {
            if !thinking.is_empty() {
                events.push(serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":thinking}}));
            }
            events.push(serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"opaque-signature-fixture"}}));
        }
        events.extend([
            serde_json::json!({"type":"content_block_stop","index":0}),
            serde_json::json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":"done"}}),
            serde_json::json!({"type":"content_block_stop","index":1}),
            serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":4}}),
            serde_json::json!({"type":"message_stop"}),
        ]);
        for event in events {
            let wire = format!(
                "event: {}\ndata: {}\n\n",
                event["type"].as_str().unwrap(),
                event
            );
            decoder.push(wire.as_bytes()).unwrap();
        }
        decoder.finish_eof().unwrap();
        let (response, _, _) = decoder.into_terminal_projection().unwrap().into_parts();
        let segment = HistorySegment::new(
            HistorySegmentKind::ReasoningReplay,
            response.output().to_vec(),
        )
        .unwrap();
        let persisted = serde_json::to_vec(&segment).unwrap();
        let reopened: HistorySegment = serde_json::from_slice(&persisted).unwrap();
        assert_eq!(reopened.canonical_bytes(), segment.canonical_bytes());
        let mut items = vec![user.clone()];
        items.extend(reopened.items().iter().cloned());
        items.push(
            ConversationItem::new(
                EntryId::try_from(Uuid::from_u128(102)).unwrap(),
                Actor::User,
                vec![ContentBlock::Text(
                    TextBlock::new("next fixture", None).unwrap(),
                )],
                Provenance::UserInput,
            )
            .unwrap(),
        );
        let mut requests = Vec::new();
        for target in [original.clone(), refreshed.clone()] {
            let plan = target
                .descriptor()
                .validate_request(build(items.clone()), ValidationMode::Strict)
                .unwrap();
            let prepared = MessagesRequestTranslator::new(target)
                .with_oauth_identity(crate::ClaudeOAuthRequestIdentity::new(
                    crate::ClaudeAccountUuid::new("12345678-1234-4234-8234-123456789abc").unwrap(),
                    crate::ClaudeClientDeviceId::new("ab".repeat(32)).unwrap(),
                ))
                .with_session_id(Some(test_affinity(99, 0).session()))
                .translate(&plan, ToolOwnership::default())
                .unwrap();
            assert_eq!(prepared.replay_diagnostics(), [] as [String; 0]);
            requests.push(serde_json::from_slice::<serde_json::Value>(prepared.body()).unwrap());
        }
        assert_eq!(requests[0]["messages"], requests[1]["messages"]);
        let replayed = &requests[1]["messages"][1]["content"];
        assert_eq!(replayed[0]["type"], kind);
        if kind == "thinking" {
            assert_eq!(replayed[0]["thinking"], thinking);
            assert_eq!(replayed[0]["signature"], "opaque-signature-fixture");
        } else {
            assert_eq!(replayed[0]["data"], "opaque-redacted-fixture");
        }
        assert_eq!(replayed[1]["type"], "text");
    }
}

/// A resumed session must never hand Anthropic the model's own reasoning as
/// assistant text.
///
/// Live Claude Opus 5 returned `stop_reason=refusal`,
/// `category=reasoning_extraction` on the resume position only, four times
/// across two builds, with a benign prompt. A reasoning block that reaches
/// lowering without its replay envelope is the shape that does it: the
/// Messages contract replays a thinking block exactly with its signature
/// and a redacted block opaque, and never as text.
#[test]
fn a_resumed_reasoning_block_without_its_signature_is_never_replayed_as_text() {
    use plakous_model::{CapabilityRequirement, ReasoningEffort, ReasoningPolicy};

    let now = Timestamp::new(CLAUDE_OAUTH_REVIEWED_AT_UNIX, 0).unwrap();
    let route = AnthropicPlatformRoute::claude_oauth(
        ApiKey::new("oauth-access-token-fixture").unwrap(),
        AnthropicAccountIdentity::new(TEST_GRANT_BINDING).unwrap(),
    )
    .unwrap();
    let original = oauth_resolved_target(
        route,
        "claude-opus-5",
        test_context_authority(),
        now,
        pin_expires_at(now).unwrap(),
    )
    .unwrap();
    let build = |items| {
        let (selected, effective) = configuration_ids();
        ModelRequest::builder(
            selected,
            effective,
            ConversationView::from_items(items)
                .expect("test conversation items form one valid segment"),
            request(now).metadata(),
        )
        .reasoning(ReasoningPolicy::Enabled {
            effort: Some(ReasoningEffort::Medium),
            requirement: CapabilityRequirement::Required,
        })
        .build()
        .unwrap()
    };
    let user = ConversationItem::new(
        EntryId::try_from(Uuid::from_u128(201)).unwrap(),
        Actor::User,
        vec![ContentBlock::Text(
            TextBlock::new("copy the seed", None).unwrap(),
        )],
        Provenance::UserInput,
    )
    .unwrap();
    // Exactly what a restored turn looks like when the signature did not
    // survive persistence: the thinking text is there, the envelope is not.
    let assistant = ConversationItem::new(
        EntryId::try_from(Uuid::from_u128(202)).unwrap(),
        Actor::Assistant,
        vec![
            ContentBlock::Reasoning(
                plakous_model::ReasoningBlock::new(
                    plakous_model::ReasoningVisibility::Visible,
                    Some("the user wants the seed copied, so I will read it".to_owned()),
                    None,
                )
                .unwrap(),
            ),
            ContentBlock::Text(TextBlock::new("done", None).unwrap()),
        ],
        Provenance::ModelOutput,
    )
    .unwrap();
    let followup = ConversationItem::new(
        EntryId::try_from(Uuid::from_u128(203)).unwrap(),
        Actor::User,
        vec![ContentBlock::Text(
            TextBlock::new("now do it again", None).unwrap(),
        )],
        Provenance::UserInput,
    )
    .unwrap();
    let plan = original
        .descriptor()
        .validate_request(
            build(vec![user, assistant, followup]),
            ValidationMode::Strict,
        )
        .unwrap();
    // The exact translator dispatch builds, so the replay discipline cannot
    // be dropped from one caller and kept in the other.
    let prepared = oauth_request_translator(
        original,
        crate::ClaudeOAuthRequestIdentity::new(
            crate::ClaudeAccountUuid::new("12345678-1234-4234-8234-123456789abc").unwrap(),
            crate::ClaudeClientDeviceId::new("ab".repeat(32)).unwrap(),
        ),
        Some(test_affinity(99, 0).session()),
    )
    .translate(&plan, ToolOwnership::default())
    .unwrap();
    let body: serde_json::Value = serde_json::from_slice(prepared.body()).unwrap();
    let replayed = &body["messages"][1]["content"];
    let blocks = replayed.as_array().expect("assistant content is an array");
    // The reasoning is gone, not converted. Only the assistant's visible
    // text survives.
    assert_eq!(blocks.len(), 1, "{blocks:?}");
    assert_eq!(blocks[0]["type"], "text");
    assert_eq!(blocks[0]["text"], "done");
    let wire = String::from_utf8(prepared.body().to_vec()).unwrap();
    assert!(
        !wire.contains("the user wants the seed copied"),
        "the model's reasoning reached the wire as text"
    );
}

#[test]
fn enrolled_oauth_descriptor_uses_route_evidence_and_pin_expiry() {
    let now = Timestamp::new(CLAUDE_OAUTH_REVIEWED_AT_UNIX, 0).expect("reviewed at");
    let expires_at = pin_expires_at(now).expect("pin expiry");
    let route = AnthropicPlatformRoute::claude_oauth(
        ApiKey::new("oauth-access-token-fixture").expect("key"),
        AnthropicAccountIdentity::new(TEST_GRANT_BINDING).expect("account"),
    )
    .expect("route");
    let resolved = oauth_resolved_target(
        route,
        "claude-sonnet-5",
        test_context_authority(),
        now,
        expires_at,
    )
    .expect("enrolled sonnet-5 descriptor");
    let descriptor = resolved.descriptor();
    assert_eq!(descriptor.requested_model().as_str(), "claude-sonnet-5");
    assert_eq!(
        descriptor.profile().as_str(),
        CLAUDE_OAUTH_COMPATIBILITY_PROFILE_ID
    );
    for capability in [
        Capability::Streaming,
        Capability::Tools,
        Capability::ParallelTools,
        Capability::Reasoning,
        Capability::Caching,
    ] {
        let fact = descriptor
            .capabilities()
            .get(capability)
            .unwrap_or_else(|| panic!("missing {capability:?}"));
        assert_eq!(fact.support(), Support::Native);
        assert_eq!(fact.provenance().source(), FactSource::RouteEvidence);
        assert_eq!(fact.provenance().observed_at(), now);
        assert_eq!(fact.provenance().expires_at(), Some(expires_at));
    }
    let contract = descriptor.context_contract().expect("context contract");
    assert_eq!(contract.projection_revision(), "messages.projection.v3");
    assert!(matches!(
        contract.capacity().evidence_class(),
        plakous_model::ContextEvidenceClass::ReferenceClientEnvelope {
            catalog_receipt,
            reference_revision,
            ..
        } if reference_revision == REFERENCE_REVISION
            && *catalog_receipt == reference_client_catalog_receipt(
                test_context_authority(),
                "claude-sonnet-5",
                1_000_000,
                1_000_000,
            ).expect("catalog receipt")
    ));
    assert_eq!(
        contract.capacity().non_guarantee_disposition(),
        Some(plakous_model::ContextNonGuaranteeDisposition::ReferenceClientOperatingEnvelope)
    );
    assert_eq!(
        contract.capacity().rule(),
        CapacityRule::ProviderReservedInput {
            reported_context_window: Some(1_000_000),
            maximum_projected_input: 1_000_000,
            auto_compact_input: None,
        }
    );
    assert_eq!(
        contract.capacity().authority_binding(),
        Some(test_context_authority())
    );
    let measurement = contract.measurements().first().expect("measurement");
    assert_eq!(
        measurement.authority_binding(),
        Some(test_context_authority())
    );
    assert!(matches!(
        measurement.kind(),
        CountCertificateKind::ReferenceClientEnvelopeUpperBound { catalog_receipt }
            if catalog_receipt == reference_client_catalog_receipt(
                test_context_authority(),
                "claude-sonnet-5",
                1_000_000,
                1_000_000,
            ).expect("catalog receipt")
    ));
    assert_eq!(resolved.model().thinking(), ThinkingSupport::Adaptive);
    assert_eq!(
        resolved.dialect().beta_header().as_deref(),
        Some(crate::claude_oauth_beta_header("claude-sonnet-5"))
    );
    assert!(now < expires_at);
    assert_eq!(
        contract.capacity().freshness().expires_at(),
        Some(expires_at)
    );
}

#[test]
fn reference_receipt_changes_with_authority_model_and_context_row() {
    let authority = test_context_authority();
    let baseline =
        reference_client_catalog_receipt(authority, "claude-sonnet-5", 1_000_000, 1_000_000)
            .expect("baseline receipt");
    let other_authority = RuntimeModelAuthority::authority_class_for(
        digest_binding(b"claude-oauth-account", b"other").expect("account"),
        digest_binding(b"claude-oauth-credential", b"other").expect("credential"),
    )
    .expect("other authority");
    assert_ne!(
        baseline,
        reference_client_catalog_receipt(other_authority, "claude-sonnet-5", 1_000_000, 1_000_000,)
            .expect("authority receipt")
    );
    assert_ne!(
        baseline,
        reference_client_catalog_receipt(authority, "claude-opus-5", 1_000_000, 1_000_000,)
            .expect("model receipt")
    );
    assert_ne!(
        baseline,
        reference_client_catalog_receipt(authority, "claude-sonnet-5", 200_000, 200_000,)
            .expect("context receipt")
    );
}

#[test]
fn oauth_complete_wire_boundary_preserves_typed_context_denial() {
    let temp = tempfile::tempdir().unwrap();
    // Preparation needs identity and authority, but no stored grant or dispatch.
    let credentials = manager(private_store(&temp));
    let now = Timestamp::new(CLAUDE_OAUTH_REVIEWED_AT_UNIX, 0).unwrap();
    let expiry = pin_expires_at(now).unwrap();
    let route = AnthropicPlatformRoute::claude_oauth(
        ApiKey::new("offline-unused").unwrap(),
        AnthropicAccountIdentity::new(TEST_GRANT_BINDING).unwrap(),
    )
    .unwrap();
    let resolved = oauth_resolved_target(
        route,
        "claude-sonnet-5",
        test_context_authority(),
        now,
        expiry,
    )
    .unwrap();
    let descriptor = resolved.descriptor().clone();
    let authority = RuntimeModelAuthority::new(
        &descriptor,
        digest_binding(
            b"claude-oauth-provider-account",
            TEST_GRANT_BINDING.as_bytes(),
        )
        .unwrap(),
        digest_binding(
            b"claude-oauth-provider-account-credential",
            TEST_GRANT_BINDING.as_bytes(),
        )
        .unwrap(),
        1,
        now,
        Some(expiry),
    )
    .unwrap();
    let identity = crate::ClaudeOAuthRequestIdentity::new(
        crate::ClaudeAccountUuid::new("12345678-1234-4234-8234-123456789abc").unwrap(),
        crate::ClaudeClientDeviceId::new("ab".repeat(32)).unwrap(),
    );
    let model = Arc::new(ClaudeOAuthLanguageModel::new(
        resolved,
        Client::new_http1(Limits::default()).unwrap(),
        authority.clone(),
        credentials,
        identity,
    ));
    let context =
        ModelPreparationContext::new(authority, now).with_cache_affinity(test_affinity(99, 0));
    let plan_for = |text: &str| {
        descriptor
            .validate_request(
                request_with_text(request(now).metadata(), text),
                ValidationMode::Strict,
            )
            .unwrap()
    };
    // Keep four text blocks at every boundary, below the per-string bound.
    let baseline_text = "x".repeat(750_001);
    let baseline = model
        .clone()
        .prepare_call(plan_for(&baseline_text), context.clone())
        .unwrap();
    let overhead = baseline.record().projection().wire_size() - 750_001;
    let limit = 1_000_000;
    assert_eq!(
        descriptor.context_contract().unwrap().capacity().rule(),
        CapacityRule::ProviderReservedInput {
            reported_context_window: Some(limit),
            maximum_projected_input: limit,
            auto_compact_input: None,
        }
    );
    for wire_size in [limit - 1, limit, limit + 1] {
        let text = "x".repeat(usize::try_from(wire_size - overhead).unwrap());
        let plan = plan_for(&text);
        let input_identity = plan.request().model_input_identity();
        let lowered = oauth_request_translator(
            model.resolved.clone(),
            model.identity.clone(),
            Some(test_affinity(99, 0).session()),
        )
        .translate(&plan, ToolOwnership::default())
        .unwrap();
        assert_eq!(u64::try_from(lowered.body().len()).unwrap(), wire_size);
        let body: serde_json::Value = serde_json::from_slice(lowered.body()).unwrap();
        assert_eq!(
            body["max_tokens"],
            crate::claude_oauth_request_max_tokens("claude-sonnet-5").expect("request default")
        );
        let result = model.clone().prepare_call(plan.clone(), context.clone());
        assert_eq!(result.unwrap().record().projection().wire_size(), wire_size);
        if wire_size <= limit {
            continue;
        }
        let error = model
            .clone()
            .prepare_call(plan, context.clone().with_required_input_bound())
            .expect_err("the paid fold requires a conservative input bound");
        assert!(matches!(
            error,
            ModelPreparationError::ContextAdmissionDenied { .. }
        ));
        let status = error.context_status().unwrap();
        assert_eq!(status.model_input_identity(), input_identity);
        assert_eq!(
            status.measurement().unwrap().bound().upper_bound(),
            wire_size
        );
        let status_identity = status.identity();
        let projected = ModelError::from(error);
        assert_eq!(projected.kind(), ModelErrorKind::ContextLimit);
        let failure = projected.context_capacity().unwrap();
        assert_eq!(failure.status_identity(), status_identity);
        assert_eq!(failure.model_input_identity(), input_identity);
        assert!(!failure.is_compactable());
    }
}

#[test]
fn unknown_reference_context_row_fails_closed() {
    let now = Timestamp::new(CLAUDE_OAUTH_REVIEWED_AT_UNIX, 0).expect("reviewed at");
    let expires_at = pin_expires_at(now).expect("pin expiry");
    let route = AnthropicPlatformRoute::claude_oauth(
        ApiKey::new("oauth-unknown-access-token-fixture").expect("key"),
        AnthropicAccountIdentity::new(TEST_GRANT_BINDING).expect("account"),
    )
    .expect("route");
    assert!(
        oauth_resolved_target(
            route,
            "claude-future-neighbor",
            test_context_authority(),
            now,
            expires_at,
        )
        .is_err()
    );
    assert!(crate::oauth::claude_enrollment("claude-future-neighbor").is_none());
}

#[tokio::test]
async fn prepared_call_binds_complete_wire_byte_observation_to_runtime_authority() {
    let temp = tempfile::tempdir().expect("tempdir");
    let manager = manager(private_store(&temp));
    manager
        .persist_test_grant(
            "access-token-value",
            "refresh-token-lineage-value",
            3600,
            CLAUDE_OAUTH_INFERENCE_SCOPE,
        )
        .expect("credential");
    let client = Client::new_http1(Limits::default()).expect("client");
    let (model, authority, descriptor) = manager
        .resolve_model("claude-sonnet-5", client, SessionLineage::Root)
        .await
        .expect("resolved model");
    let prepared_at = authority.observed_at();
    let plan = descriptor
        .validate_request(request(prepared_at), ValidationMode::Strict)
        .expect("validated plan");
    let prepared = model
        .prepare_call(
            plan,
            ModelPreparationContext::new(authority.clone(), prepared_at)
                .with_cache_affinity(test_affinity(99, 0)),
        )
        .expect("prepared call");
    let projection = prepared.record().projection();
    assert_eq!(projection.projection_revision(), "messages.projection.v3");
    assert_eq!(
        projection.authority_class(),
        authority
            .authority_class_binding()
            .expect("authority class")
    );
    let contract = descriptor.context_contract().expect("context contract");
    assert_eq!(
        contract.capacity().authority_binding(),
        Some(projection.authority_class())
    );
    let certificate = contract.measurements().first().expect("measurement");
    assert_eq!(
        certificate.authority_binding(),
        Some(projection.authority_class())
    );
    let observation = projection
        .token_projection()
        .observation()
        .expect("count observation");
    assert_eq!(observation.certificate_id(), certificate.certificate_id());
    assert_eq!(observation.certificate_revision(), certificate.revision());
    assert_eq!(observation.tokens(), projection.wire_size());
    assert!(projection.wire_size() > 0);
}

async fn refreshed_session_models(
    manager: ClaudeNativeAuthManager,
) -> (Arc<ClaudeOAuthLanguageModel>, Arc<ClaudeOAuthLanguageModel>) {
    let (_, authority, descriptor) = manager
        .resolve_model(
            "claude-sonnet-5",
            Client::new_http1(Limits::default()).unwrap(),
            SessionLineage::Root,
        )
        .await
        .unwrap();
    let now = authority.observed_at();
    let ((token, _, _), identity) = manager.identity_credentials().unwrap();
    let route = AnthropicPlatformRoute::claude_oauth(
        ApiKey::new(token).unwrap(),
        AnthropicAccountIdentity::new(identity.account_uuid().as_str()).unwrap(),
    )
    .unwrap();
    let resolved = oauth_resolved_target(
        route,
        "claude-sonnet-5",
        authority.authority_class_binding().unwrap(),
        now,
        pin_expires_at(now).unwrap(),
    )
    .unwrap();
    let model = Arc::new(ClaudeOAuthLanguageModel::new(
        resolved,
        Client::new_http1(Limits::default()).unwrap(),
        authority.clone(),
        manager.clone(),
        manager.request_identity().unwrap(),
    ));
    let later = Timestamp::new(now.seconds() + 1, 0).unwrap();
    let refreshed = oauth_resolved_target(
        model.resolved.route().clone(),
        "claude-sonnet-5",
        authority.authority_class_binding().unwrap(),
        later,
        pin_expires_at(later).unwrap(),
    )
    .unwrap();
    assert_ne!(
        descriptor.binding_digest().unwrap(),
        refreshed.descriptor().binding_digest().unwrap()
    );
    let refreshed_authority = RuntimeModelAuthority::new(
        refreshed.descriptor(),
        authority.account_binding(),
        authority.credential_authority_binding(),
        authority.credential_generation(),
        later,
        authority.valid_until(),
    )
    .unwrap();
    let refreshed_model = Arc::new(ClaudeOAuthLanguageModel::new(
        refreshed,
        model.client.clone(),
        refreshed_authority,
        manager.clone(),
        manager.request_identity().unwrap(),
    ));
    (model, refreshed_model)
}

#[tokio::test]
async fn revalidation_accepts_new_grant_for_same_provider_uuid() {
    let temp = tempfile::tempdir().unwrap();
    let manager = manager(private_store(&temp));
    manager
        .persist_test_grant(
            "first-access",
            "first-refresh",
            3600,
            CLAUDE_OAUTH_INFERENCE_SCOPE,
        )
        .unwrap();
    let (model, _) = refreshed_session_models(manager.clone()).await;
    manager
        .persist_test_grant(
            "second-access",
            "second-refresh",
            3600,
            CLAUDE_OAUTH_INFERENCE_SCOPE,
        )
        .unwrap();
    let now = model.authority.observed_at();
    let before = model.authority.clone();
    let after = model.revalidate_runtime_authority(now).await.unwrap();
    assert!(after.credential_generation() > before.credential_generation());
    assert_eq!(after.account_binding(), before.account_binding());
    assert_eq!(
        after.credential_authority_binding(),
        before.credential_authority_binding()
    );
    assert!(after.observed_at() >= before.observed_at());
}

#[tokio::test]
async fn revalidation_refuses_new_grant_for_different_provider_uuid() {
    let temp = tempfile::tempdir().unwrap();
    let manager = manager(private_store(&temp));
    manager
        .persist_test_grant(
            "first-access",
            "first-refresh",
            3600,
            CLAUDE_OAUTH_INFERENCE_SCOPE,
        )
        .unwrap();
    let (model, _) = refreshed_session_models(manager.clone()).await;
    manager
        .persist_test_grant_for_account(
            "second-access",
            "second-refresh",
            3600,
            CLAUDE_OAUTH_INFERENCE_SCOPE,
            "87654321-4321-4321-8321-cba987654321",
        )
        .unwrap();
    let now = model.authority.observed_at();
    assert_eq!(
        model.revalidate_runtime_authority(now).await,
        Err(RuntimeModelAuthorityError::AccountChanged)
    );
}

#[tokio::test]
async fn revalidation_accepts_older_checked_at_without_regressing_admission_observation() {
    let temp = tempfile::tempdir().unwrap();
    let manager = manager(private_store(&temp));
    manager
        .persist_test_grant(
            "first-access",
            "first-refresh",
            3600,
            CLAUDE_OAUTH_INFERENCE_SCOPE,
        )
        .unwrap();
    let (model, _) = refreshed_session_models(manager.clone()).await;
    manager
        .persist_test_grant(
            "second-access",
            "second-refresh",
            3600,
            CLAUDE_OAUTH_INFERENCE_SCOPE,
        )
        .unwrap();
    let then = model.authority.observed_at();
    let later = Timestamp::new(then.seconds() + 1, then.nanoseconds()).unwrap();
    let newer = Arc::clone(&model)
        .revalidate_runtime_authority(later)
        .await
        .unwrap();
    let earlier = Arc::clone(&model)
        .revalidate_runtime_authority(then)
        .await
        .unwrap();
    assert_eq!(
        earlier.credential_generation(),
        newer.credential_generation()
    );
    assert_eq!(earlier.observed_at(), newer.observed_at());
    earlier.validate(model.resolved.descriptor(), then).unwrap();
    let plan = model
        .resolved
        .descriptor()
        .validate_request(request(then), ValidationMode::Strict)
        .unwrap();
    model
        .prepare_call(
            plan,
            ModelPreparationContext::new(earlier, then).with_cache_affinity(test_affinity(99, 0)),
        )
        .unwrap();
}

#[tokio::test]
async fn preparation_uses_persisted_session_across_refresh_and_cache_rotation() {
    let temp = tempfile::tempdir().unwrap();
    let manager = manager(private_store(&temp));
    manager
        .persist_test_grant(
            "access-token-value",
            "refresh-token-value",
            3600,
            CLAUDE_OAUTH_INFERENCE_SCOPE,
        )
        .unwrap();
    let (model, refreshed_model) = refreshed_session_models(manager).await;
    let now = model.authority.observed_at();
    let source = plakous_agent::internal::RuntimeContextState::genesis(
        test_affinity(99, 0).session(),
        test_affinity(99, 0).branch(),
        [1; 32],
    )
    .unwrap();
    let restored_state =
        plakous_agent::internal::RuntimeContextState::decode(&source.encode().unwrap())
            .unwrap()
            .retained_for_exact_resume();
    let fork = source
        .ordinary_fork(
            plakous_model::CheckpointId::try_from(Uuid::from_u128(102)).unwrap(),
            test_affinity(101, 0).session(),
            plakous_model::BranchId::try_from(Uuid::from_u128(103)).unwrap(),
            [2; 32],
        )
        .unwrap();
    let original_affinity = source.affinity().clone();
    let restored = restored_state.affinity().clone();
    let rotated = test_affinity(99, 1);
    assert_ne!(original_affinity.identity(), rotated.identity());
    let forked = fork.affinity().clone();
    let mut digests = Vec::new();
    for (candidate, affinity) in [
        (model.clone(), original_affinity),
        (refreshed_model.clone(), restored),
        (refreshed_model.clone(), rotated),
        (refreshed_model.clone(), forked),
    ] {
        let plan = candidate
            .resolved
            .descriptor()
            .validate_request(request(now), ValidationMode::Strict)
            .unwrap();
        let lowered = MessagesRequestTranslator::new(candidate.resolved.clone())
            .with_oauth_identity(crate::ClaudeOAuthRequestIdentity::new(
                crate::ClaudeAccountUuid::new("12345678-1234-4234-8234-123456789abc").unwrap(),
                crate::ClaudeClientDeviceId::new("ab".repeat(32)).unwrap(),
            ))
            .with_session_id(Some(affinity.session()))
            .translate(&plan, ToolOwnership::default())
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(lowered.body()).unwrap();
        assert_eq!(
            body["max_tokens"],
            crate::claude_oauth_request_max_tokens("claude-sonnet-5").expect("request default")
        );
        let metadata: serde_json::Value =
            serde_json::from_str(body["metadata"]["user_id"].as_str().unwrap()).unwrap();
        assert_eq!(metadata["session_id"], affinity.session().to_string());
        let expected =
            Digest::sha256(DigestDomain::Blob, 1, &[WIRE_DOMAIN, lowered.body()]).unwrap();
        let context = ModelPreparationContext::new(
            candidate.authority.clone(),
            candidate.authority.observed_at(),
        )
        .with_cache_affinity(affinity);
        let prepared = candidate.prepare_call(plan, context).unwrap();
        assert_eq!(prepared.record().projection().wire_digest(), expected);
        digests.push(expected);
    }
    assert_eq!(digests[0], digests[1]);
    assert_eq!(digests[0], digests[2]);
    assert_ne!(digests[0], digests[3]);
}

#[tokio::test]
async fn oauth_preparation_requires_explicit_logical_session() {
    let temp = tempfile::tempdir().unwrap();
    let manager = manager(private_store(&temp));
    manager
        .persist_test_grant(
            "access-token-value",
            "refresh-token-value",
            3600,
            CLAUDE_OAUTH_INFERENCE_SCOPE,
        )
        .unwrap();
    let (model, _) = refreshed_session_models(manager).await;
    let now = model.authority.observed_at();
    let plan = model
        .resolved
        .descriptor()
        .validate_request(request(now), ValidationMode::Strict)
        .unwrap();
    for _ in 0..2 {
        let error = model
            .clone()
            .prepare_call(
                plan.clone(),
                ModelPreparationContext::new(model.authority.clone(), now),
            )
            .expect_err("missing session must reject preparation");
        assert!(matches!(
            &error,
            ModelPreparationError::Rejected {
                kind: ModelPreparationErrorKind::Projection,
                ..
            }
        ));
        assert!(
            error
                .to_string()
                .contains("runtime logical session identity")
        );
    }
}

#[test]
fn haiku_5_5_is_enrolled_on_oauth_with_the_shared_window() {
    let now = Timestamp::new(CLAUDE_OAUTH_REVIEWED_AT_UNIX, 0).expect("reviewed at");
    let expires_at = pin_expires_at(now).expect("pin expiry");
    let route = AnthropicPlatformRoute::claude_oauth(
        ApiKey::new("oauth-haiku-access-token-fixture").expect("key"),
        AnthropicAccountIdentity::new(TEST_GRANT_BINDING).expect("account"),
    )
    .expect("route");
    let resolved = oauth_resolved_target(
        route,
        "claude-haiku-5-5",
        test_context_authority(),
        now,
        expires_at,
    )
    .expect("enrolled haiku 5.5");
    assert_eq!(resolved.model().remote_id().as_str(), "claude-haiku-5-5");
    assert_eq!(resolved.model().family().as_str(), "claude-5-haiku");
    assert_eq!(resolved.model().maximum_output_tokens(), 128_000);
    assert_eq!(resolved.model().thinking(), ThinkingSupport::Adaptive);
    assert!(resolved.dialect().is_claude_oauth());
    assert_eq!(
        resolved
            .descriptor()
            .context_contract()
            .expect("context")
            .capacity()
            .rule(),
        CapacityRule::ProviderReservedInput {
            reported_context_window: Some(1_000_000),
            maximum_projected_input: 1_000_000,
            auto_compact_input: None,
        }
    );
    assert_eq!(
        resolved.dialect().beta_header().as_deref(),
        Some(crate::claude_oauth_beta_header("claude-haiku-5-5"))
    );
}

#[test]
fn haiku_4_5_is_not_enrolled() {
    let now = Timestamp::new(CLAUDE_OAUTH_REVIEWED_AT_UNIX, 0).expect("reviewed at");
    let expires_at = pin_expires_at(now).expect("pin expiry");
    let route = AnthropicPlatformRoute::claude_oauth(
        ApiKey::new("oauth-haiku-access-token-fixture").expect("key"),
        AnthropicAccountIdentity::new(TEST_GRANT_BINDING).expect("account"),
    )
    .expect("route");
    assert!(
        oauth_resolved_target(
            route,
            "claude-haiku-4-5",
            test_context_authority(),
            now,
            expires_at,
        )
        .is_err()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn refresh_then_reserve_charges_inference_only_after_success() {
    let temp = tempfile::tempdir().unwrap();
    let gate = inference_gate(1);
    let manager = manager(private_store(&temp)).with_subscription_request_gate(gate.clone());
    let status = manager
        .persist_test_grant(
            "access-token-value",
            "refresh-token-value",
            3600,
            CLAUDE_OAUTH_INFERENCE_SCOPE,
        )
        .unwrap();
    assert_eq!(status.status(), CredentialStatus::Ready);
    let permit = refresh_then_reserve_inference(&manager, 32)
        .await
        .expect("fresh Claude credential must reserve inference");
    drop(permit);
    let inference = gate
        .snapshot()
        .unwrap()
        .class(SubscriptionRequestClass::Inference);
    assert_eq!(inference.reserved(), 1);
    assert_eq!(inference.dispatched(), 0);
    assert_eq!(
        gate.snapshot()
            .unwrap()
            .class(SubscriptionRequestClass::AuthRefresh)
            .reserved(),
        0,
        "fresh access must not charge AuthRefresh"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_second_physical_dispatch_requires_a_second_inference_permit() {
    let temp = tempfile::tempdir().unwrap();
    let gate = inference_gate(2);
    let manager = manager(private_store(&temp)).with_subscription_request_gate(gate.clone());
    manager
        .persist_test_grant(
            "access-token-value",
            "refresh-token-value",
            3600,
            CLAUDE_OAUTH_INFERENCE_SCOPE,
        )
        .unwrap();
    for _ in 0..2 {
        let mut permit = refresh_then_reserve_inference(&manager, 32)
            .await
            .expect("fresh permit for each physical dispatch");
        permit
            .authorize_dispatch()
            .expect("one permit authorizes once");
        drop(permit);
    }
    let inference = gate
        .snapshot()
        .unwrap()
        .class(SubscriptionRequestClass::Inference);
    assert_eq!(inference.reserved(), 2);
    assert_eq!(inference.dispatched(), 2);
    assert!(refresh_then_reserve_inference(&manager, 32).await.is_err());
}

#[tokio::test]
async fn production_unknown_sent_beta_is_single_dispatch_permanent_configuration_drift() {
    let temp = tempfile::tempdir().unwrap();
    let refusal = local_response(
        400,
        "application/json",
        r#"{"type":"error","error":{"type":"invalid_request_error","message":"Unexpected value(s) `effort-2025-11-24` for the `anthropic-beta` header. Please consult our documentation at docs.claude.com or try again without the header."}}"#,
    );
    let (client, origin, peer) = local_peer(vec![refusal]).await;
    let gate = inference_gate(2);
    let manager = manager_with_client(private_store(&temp), client.clone())
        .with_subscription_request_gate(gate.clone());
    manager
        .persist_test_grant(
            "offline-access",
            "offline-refresh",
            3600,
            CLAUDE_OAUTH_INFERENCE_SCOPE,
        )
        .unwrap();
    let failure = timeout(
        Duration::from_secs(5),
        start_local_model(manager, client, origin, false),
    )
    .await
    .unwrap()
    .unwrap_err();
    let model = failure.downcast_ref::<ModelError>().expect("model failure");
    assert_eq!(model.kind(), ModelErrorKind::InvalidConfiguration);
    assert_eq!(
        model.context().expect("failure context").retryability(),
        plakous_model::ModelRetryability::Permanent
    );
    assert!(model.to_string().contains("install a newer build"));
    assert_eq!(
        timeout(Duration::from_secs(5), peer)
            .await
            .unwrap()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        gate.snapshot()
            .unwrap()
            .class(SubscriptionRequestClass::Inference)
            .dispatched(),
        1
    );
    assert_eq!(
        gate.snapshot()
            .unwrap()
            .class(SubscriptionRequestClass::AuthRefresh)
            .reserved(),
        0
    );
}

#[tokio::test]
async fn a_side_request_rejecting_a_sent_beta_is_final_with_the_newer_build_remedy() {
    let temp = tempfile::tempdir().unwrap();
    let refusal = local_response(
        400,
        "application/json",
        r#"{"type":"error","error":{"type":"invalid_request_error","message":"Unexpected value(s) `effort-2025-11-24` for the `anthropic-beta` header. Please consult our documentation at docs.claude.com or try again without the header."}}"#,
    );
    let (client, origin, peer) = local_peer(vec![refusal]).await;
    let gate = inference_gate(2);
    let manager = manager_with_client(private_store(&temp), client.clone())
        .with_subscription_request_gate(gate.clone());
    manager
        .persist_test_grant(
            "offline-access",
            "offline-refresh",
            3600,
            CLAUDE_OAUTH_INFERENCE_SCOPE,
        )
        .unwrap();
    let session_id = plakous_model::SessionId::try_from(Uuid::from_u128(7)).unwrap();
    let (_, _, _, retrieval) = manager
        .resolve_model_with_retrieval("claude-sonnet-5", client, SessionLineage::Root, session_id)
        .await
        .unwrap();
    let query = plakous_messages::MessagesRetrievalQuery::WebSearch {
        query: "Rust release".to_owned(),
        allowed_domains: Vec::new(),
        blocked_domains: Vec::new(),
        max_uses: 1,
    };
    let error = timeout(
        Duration::from_secs(5),
        retrieval
            .with_dial_origin(origin)
            .retrieve(query, &plakous_model::CancellationToken::new()),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(
        error.kind(),
        plakous_messages::MessagesRetrievalErrorKind::Rejected
    );
    assert_eq!(error.status(), Some(400));
    assert_eq!(error.physical_requests(), 1);
    assert_eq!(
        error.message(),
        plakous_auth::CompatibilityDrift::UnknownBeta.message("Claude")
    );
    let requests = timeout(Duration::from_secs(5), peer)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(requests.len(), 1);
    assert!(
        String::from_utf8_lossy(&requests[0])
            .to_ascii_lowercase()
            .contains("effort-2025-11-24")
    );
}

#[tokio::test]
async fn production_oauth_turn_retries_local_429_with_fresh_inference_permit() {
    let temp = tempfile::tempdir().unwrap();
    let refusal = local_response(
        429,
        "application/json",
        r#"{"type":"error","error":{"type":"rate_limit_error","message":"fixture"}}"#,
    );
    let success = local_response(200, "text/event-stream", LOCAL_SUCCESS_SSE);
    let (client, origin, peer) = local_peer(vec![refusal, success]).await;
    let gate = inference_gate(2);
    let manager = manager_with_client(private_store(&temp), client.clone())
        .with_subscription_request_gate(gate.clone());
    manager
        .persist_test_grant(
            "offline-access-one",
            "offline-refresh-one",
            3600,
            CLAUDE_OAUTH_INFERENCE_SCOPE,
        )
        .unwrap();
    let events = timeout(
        Duration::from_secs(5),
        start_local_model(manager, client, origin, false),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, plakous_model::ModelEvent::ResponseCompleted(_)))
    );
    let requests = timeout(Duration::from_secs(5), peer)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|wire| wire.starts_with(b"POST /v1/messages?beta=true "))
    );
    let inference = gate
        .snapshot()
        .unwrap()
        .class(SubscriptionRequestClass::Inference);
    assert_eq!(inference.reserved(), 2);
    assert_eq!(inference.dispatched(), 2);
    assert_eq!(
        gate.snapshot()
            .unwrap()
            .class(SubscriptionRequestClass::AuthRefresh)
            .reserved(),
        0
    );
}

#[tokio::test]
async fn production_oauth_turn_refreshes_after_local_401_with_new_permit() {
    let temp = tempfile::tempdir().unwrap();
    let rejected = local_response(
        401,
        "application/json",
        r#"{"type":"error","error":{"type":"authentication_error","message":"fixture"}}"#,
    );
    let token = local_response(
        200,
        "application/json",
        r#"{"access_token":"offline-access-two","refresh_token":"offline-refresh-two","expires_in":3600,"scope":"user:inference","account":{"uuid":"12345678-1234-4234-8234-123456789abc"}}"#,
    );
    let success = local_response(200, "text/event-stream", LOCAL_SUCCESS_SSE);
    let (client, origin, peer) = local_peer(vec![rejected, token, success]).await;
    let gate = request_gate(1, 2);
    let manager = manager_with_client(private_store(&temp), client.clone())
        .with_test_token_origin(&origin)
        .unwrap()
        .with_subscription_request_gate(gate.clone());
    manager
        .persist_test_grant(
            "offline-access-one",
            "offline-refresh-one",
            3600,
            CLAUDE_OAUTH_INFERENCE_SCOPE,
        )
        .unwrap();
    let events = timeout(
        Duration::from_secs(5),
        start_local_model(manager, client, origin, false),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, plakous_model::ModelEvent::ResponseCompleted(_)))
    );
    let requests = timeout(Duration::from_secs(5), peer)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests[0].starts_with(b"POST /v1/messages?beta=true "));
    assert!(requests[1].starts_with(b"POST /v1/oauth/token "));
    assert!(requests[2].starts_with(b"POST /v1/messages?beta=true "));
    let inference = gate
        .snapshot()
        .unwrap()
        .class(SubscriptionRequestClass::Inference);
    assert_eq!(inference.reserved(), 2);
    assert_eq!(inference.dispatched(), 2);
    let refresh = gate
        .snapshot()
        .unwrap()
        .class(SubscriptionRequestClass::AuthRefresh);
    assert_eq!(refresh.reserved(), 1);
    assert_eq!(refresh.dispatched(), 1);
}

#[tokio::test]
async fn production_oauth_session_workflow_connect_retry_requires_fresh_authorization() {
    for inference_limit in [1, 2] {
        let temp = tempfile::tempdir().unwrap();
        let success = local_response(200, "text/event-stream", LOCAL_SUCCESS_SSE);
        let (client, origin, mut peer) = local_peer(vec![success]).await;
        let client = client.with_faults(Some(
            plakous_http::FaultPlan::parse("fault=connect,nth=1").unwrap(),
        ));
        let gate = inference_gate(inference_limit);
        let manager = manager_with_client(private_store(&temp), client.clone())
            .with_subscription_request_gate(gate.clone());
        manager
            .persist_test_grant(
                "offline-access-one",
                "offline-refresh-one",
                3600,
                CLAUDE_OAUTH_INFERENCE_SCOPE,
            )
            .unwrap();
        let result = timeout(
            Duration::from_secs(5),
            start_local_model(manager, client, origin, true),
        )
        .await
        .unwrap();
        let inference = gate
            .snapshot()
            .unwrap()
            .class(SubscriptionRequestClass::Inference);
        assert_eq!(inference.reserved(), 1);
        assert_eq!(inference.dispatched(), 1);
        let events = result.expect("unsent workflow refunds then takes a fresh permit");
        assert!(
            events
                .iter()
                .any(|event| matches!(event, plakous_model::ModelEvent::ResponseCompleted(_)))
        );
        let requests = timeout(Duration::from_secs(5), &mut peer)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(requests.len(), 1, "unsent retry adds no paid dispatch");
        let wire = std::str::from_utf8(&requests[0]).unwrap();
        assert!(wire.contains("cc_prompt_index=7; cc_turn_index=9;"));
    }
}

#[tokio::test]
async fn production_oauth_incomplete_requests_never_refund() {
    for fault in ["record-rejected", "write-reset"] {
        let temp = tempfile::tempdir().unwrap();
        let success = local_response(200, "text/event-stream", LOCAL_SUCCESS_SSE);
        let (client, origin, mut peer) = local_peer(vec![success]).await;
        let client = client.with_faults(Some(
            plakous_http::FaultPlan::parse(&format!("fault={fault},nth=1")).unwrap(),
        ));
        let gate = inference_gate(2);
        let manager = manager_with_client(private_store(&temp), client.clone())
            .with_subscription_request_gate(gate.clone());
        manager
            .persist_test_grant(
                "offline-access-one",
                "offline-refresh-one",
                3600,
                CLAUDE_OAUTH_INFERENCE_SCOPE,
            )
            .unwrap();
        let result = timeout(
            Duration::from_secs(5),
            start_local_model(manager, client, origin, true),
        )
        .await
        .unwrap();
        let inference = gate
            .snapshot()
            .unwrap()
            .class(SubscriptionRequestClass::Inference);
        assert_eq!(inference.reserved(), 2);
        assert_eq!(inference.dispatched(), 2);
        assert!(inference.input_token_upper_bound() > 0);
        let events = result.expect("incomplete workflow needs another consumed permit");
        assert!(
            events
                .iter()
                .any(|event| matches!(event, plakous_model::ModelEvent::ResponseCompleted(_)))
        );
        let requests = timeout(Duration::from_secs(5), &mut peer)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(requests.len(), 1, "unsent retry adds no paid dispatch");
        let wire = std::str::from_utf8(&requests[0]).unwrap();
        assert!(wire.contains("cc_prompt_index=7; cc_turn_index=9;"));
    }
}

#[tokio::test]
async fn production_oauth_session_workflow_401_never_refreshes_or_redispatches() {
    let temp = tempfile::tempdir().unwrap();
    let rejected = local_response(
        401,
        "application/json",
        r#"{"type":"error","error":{"type":"authentication_error","message":"fixture"}}"#,
    );
    let (client, origin, peer) = local_peer(vec![rejected]).await;
    let gate = request_gate(1, 2);
    let manager = manager_with_client(private_store(&temp), client.clone())
        .with_test_token_origin(&origin)
        .unwrap()
        .with_subscription_request_gate(gate.clone());
    manager
        .persist_test_grant(
            "offline-access-one",
            "offline-refresh-one",
            3600,
            CLAUDE_OAUTH_INFERENCE_SCOPE,
        )
        .unwrap();
    let result = timeout(
        Duration::from_secs(5),
        start_local_model(manager, client, origin, true),
    )
    .await
    .unwrap();
    assert!(result.is_err(), "workflow must surface the first 401");
    let requests = timeout(Duration::from_secs(5), peer)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with(b"POST /v1/messages?beta=true "));
    let snapshot = gate.snapshot().unwrap();
    let inference = snapshot.class(SubscriptionRequestClass::Inference);
    assert_eq!(inference.reserved(), 1);
    assert_eq!(inference.dispatched(), 1);
    let refresh = snapshot.class(SubscriptionRequestClass::AuthRefresh);
    assert_eq!(refresh.reserved(), 0);
    assert_eq!(refresh.dispatched(), 0);
}

/// The peer reads the turn's request and drops TLS without a byte or a
/// `close_notify`: the cost-only turn replays it with a fresh permit.
#[tokio::test]
async fn production_oauth_turn_replays_a_dropped_exchange_after_send() {
    let temp = tempfile::tempdir().unwrap();
    let success = local_response(200, "text/event-stream", LOCAL_SUCCESS_SSE);
    let (client, origin, peer) = local_peer(vec![Vec::new(), success]).await;
    let gate = inference_gate(2);
    let manager = manager_with_client(private_store(&temp), client.clone())
        .with_subscription_request_gate(gate.clone());
    manager
        .persist_test_grant(
            "offline-access-one",
            "offline-refresh-one",
            3600,
            CLAUDE_OAUTH_INFERENCE_SCOPE,
        )
        .unwrap();
    let events = timeout(
        Duration::from_secs(10),
        start_local_model(manager, client, origin, false),
    )
    .await
    .unwrap()
    .unwrap();
    let retries = events
        .iter()
        .filter_map(|event| match event {
            plakous_model::ModelEvent::Retry(retry) => Some(retry.cause().kind()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(retries, ["transport_after_send"]);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, plakous_model::ModelEvent::ResponseCompleted(_)))
    );
    let requests = timeout(Duration::from_secs(5), peer)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], requests[1]);
    let inference = gate
        .snapshot()
        .unwrap()
        .class(SubscriptionRequestClass::Inference);
    assert_eq!(inference.dispatched(), 2);
}

/// A session workflow (compaction) is single-dispatch: the same dropped
/// exchange ends it, and the queued success is never requested.
#[tokio::test]
async fn production_oauth_session_workflow_drop_is_never_replayed() {
    let temp = tempfile::tempdir().unwrap();
    let success = local_response(200, "text/event-stream", LOCAL_SUCCESS_SSE);
    let (client, origin, peer) = local_peer(vec![Vec::new(), success]).await;
    let gate = inference_gate(2);
    let manager = manager_with_client(private_store(&temp), client.clone())
        .with_subscription_request_gate(gate.clone());
    manager
        .persist_test_grant(
            "offline-access-one",
            "offline-refresh-one",
            3600,
            CLAUDE_OAUTH_INFERENCE_SCOPE,
        )
        .unwrap();
    let result = timeout(
        Duration::from_secs(10),
        start_local_model(manager, client, origin, true),
    )
    .await
    .unwrap();
    assert!(
        result.is_err(),
        "the workflow surfaces the dropped exchange"
    );
    let inference = gate
        .snapshot()
        .unwrap()
        .class(SubscriptionRequestClass::Inference);
    assert_eq!(inference.reserved(), 1);
    assert_eq!(inference.dispatched(), 1);
    peer.abort();
}

#[tokio::test]
async fn production_oauth_retry_stops_when_second_inference_permit_is_denied() {
    let temp = tempfile::tempdir().unwrap();
    let refusal = local_response(
        429,
        "application/json",
        r#"{"type":"error","error":{"type":"rate_limit_error","message":"fixture"}}"#,
    );
    let (client, origin, peer) = local_peer(vec![refusal]).await;
    let gate = inference_gate(1);
    let manager = manager_with_client(private_store(&temp), client.clone())
        .with_subscription_request_gate(gate.clone());
    manager
        .persist_test_grant(
            "offline-access-one",
            "offline-refresh-one",
            3600,
            CLAUDE_OAUTH_INFERENCE_SCOPE,
        )
        .unwrap();
    let result = timeout(
        Duration::from_secs(5),
        start_local_model(manager, client, origin, false),
    )
    .await
    .unwrap();
    assert!(result.is_err(), "a denied second permit must end the turn");
    assert_eq!(
        timeout(Duration::from_secs(5), peer)
            .await
            .unwrap()
            .unwrap()
            .len(),
        1
    );
    let inference = gate
        .snapshot()
        .unwrap()
        .class(SubscriptionRequestClass::Inference);
    assert_eq!(inference.reserved(), 1);
    assert_eq!(inference.dispatched(), 1);
    assert_eq!(inference.denied(), 1);
}

#[test]
fn cancelled_retry_drops_reserved_permit_without_dispatch_authorization() {
    let gate = inference_gate(1);
    let permit = gate.acquire_inference(32).expect("reserve retry permit");
    let authorize = permit_authorization(permit);
    assert_eq!(
        gate.snapshot()
            .unwrap()
            .class(SubscriptionRequestClass::Inference)
            .dispatched(),
        0,
        "refresh completion alone must not count a physical send"
    );
    drop(authorize);
    let inference = gate
        .snapshot()
        .unwrap()
        .class(SubscriptionRequestClass::Inference);
    assert_eq!(inference.dispatched(), 0);
    assert_eq!(inference.abandoned(), 1);
}

#[test]
fn one_retry_dispatch_authorization_consumes_one_permit() {
    let gate = inference_gate(1);
    let permit = gate.acquire_inference(32).expect("reserve retry permit");
    let authorize = permit_authorization(permit);
    (authorize.authorize)().expect("first physical send");
    assert!((authorize.authorize)().is_err(), "permit cannot be reused");
    let inference = gate
        .snapshot()
        .unwrap()
        .class(SubscriptionRequestClass::Inference);
    assert_eq!(inference.dispatched(), 1);
    assert_eq!(inference.abandoned(), 0);
}

#[test]
fn refresh_transport_preserves_typed_cause_and_failure_prose() {
    let error = map_credential_error(ClaudeNativeAuthError::Transport(Some(
        plakous_model::ModelNetworkFailure {
            phase: plakous_model::ModelRetryPhase::Resolution,
            sent: plakous_model::ModelNetworkTransmission::NotSent,
            timed_out: true,
        },
    )));
    assert_eq!(error.kind(), ModelErrorKind::Transport);
    assert_eq!(
        error.message(),
        "Claude sign-in refresh failed: DNS lookup timed out before anything was sent; credential unchanged"
    );
}

#[test]
fn refresh_without_unsent_proof_never_claims_the_credential_is_unchanged() {
    for cause in [
        None,
        Some(plakous_model::ModelNetworkFailure {
            phase: plakous_model::ModelRetryPhase::ResponseBody,
            sent: plakous_model::ModelNetworkTransmission::MayHaveSent,
            timed_out: false,
        }),
    ] {
        let error = map_credential_error(ClaudeNativeAuthError::Transport(cause));
        assert_eq!(error.kind(), ModelErrorKind::Authentication);
        assert!(!error.message().contains("credential unchanged"));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn failed_refresh_does_not_consume_inference_class() {
    let temp = tempfile::tempdir().unwrap();
    let gate = inference_gate(1);
    let manager = manager(private_store(&temp)).with_subscription_request_gate(gate.clone());
    manager
        .persist_test_grant(
            "access-token-value",
            "refresh-token-value",
            3600,
            CLAUDE_OAUTH_INFERENCE_SCOPE,
        )
        .unwrap();
    manager.logout_local().unwrap();
    let error = refresh_then_reserve_inference(&manager, 32)
        .await
        .err()
        .expect("logged-out Claude refresh must fail");
    assert_eq!(error.kind(), ModelErrorKind::Authentication);
    let inference = gate
        .snapshot()
        .unwrap()
        .class(SubscriptionRequestClass::Inference);
    assert_eq!(inference.reserved(), 0);
    assert_eq!(inference.dispatched(), 0);
    assert_eq!(inference.denied(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn missing_inference_gate_fails_after_refresh() {
    let temp = tempfile::tempdir().unwrap();
    let manager = manager(private_store(&temp));
    manager
        .persist_test_grant(
            "access-token-value",
            "refresh-token-value",
            3600,
            CLAUDE_OAUTH_INFERENCE_SCOPE,
        )
        .unwrap();
    let error = refresh_then_reserve_inference(&manager, 32)
        .await
        .err()
        .expect("Claude inference must fail closed without a gate");
    assert_eq!(error.kind(), ModelErrorKind::ResourceLimit);
}

#[tokio::test(flavor = "current_thread")]
async fn generation_change_after_refresh_still_reserves_inference() {
    let temp = tempfile::tempdir().unwrap();
    let gate = inference_gate(1);
    let manager = manager(private_store(&temp)).with_subscription_request_gate(gate.clone());
    let first = manager
        .persist_test_grant(
            "access-token-one",
            "refresh-token-one",
            3600,
            CLAUDE_OAUTH_INFERENCE_SCOPE,
        )
        .unwrap();
    manager
        .persist_test_grant(
            "access-token-two",
            "refresh-token-two",
            3600,
            CLAUDE_OAUTH_INFERENCE_SCOPE,
        )
        .unwrap();
    let permit = refresh_then_reserve_inference(&manager, 32)
        .await
        .expect("rotated Claude generation must still reserve inference");
    drop(permit);
    let (token, generation, _) = manager.dispatch_credentials().unwrap();
    assert_eq!(token, "access-token-two");
    assert_ne!(generation, first.generation().get());
    let inference = gate
        .snapshot()
        .unwrap()
        .class(SubscriptionRequestClass::Inference);
    assert_eq!(inference.reserved(), 1);
    assert_eq!(inference.denied(), 0);
}

#[test]
fn oauth_access_token_rebind_keeps_prepared_target_binding() {
    let now = Timestamp::new(CLAUDE_OAUTH_REVIEWED_AT_UNIX, 0).expect("reviewed at");
    let expires_at = pin_expires_at(now).expect("pin expiry");
    let route = AnthropicPlatformRoute::claude_oauth(
        ApiKey::new("oauth-access-token-one").expect("key"),
        AnthropicAccountIdentity::new(TEST_GRANT_BINDING).expect("account"),
    )
    .expect("route");
    let resolved = oauth_resolved_target(
        route,
        "claude-sonnet-5",
        test_context_authority(),
        now,
        expires_at,
    )
    .expect("enrolled sonnet-5 descriptor");
    let binding =
        AnthropicMessagesTargetBinding::from_resolved(&resolved).expect("prepared binding");
    let rotated = resolved
        .route()
        .with_oauth_access_token("oauth-access-token-two")
        .expect("OAuth rebind");
    binding
        .validate_route(&rotated)
        .expect("rebind must retain prepared credential authority");
    let independently_wrapped = AnthropicPlatformRoute::claude_oauth(
        ApiKey::new("oauth-access-token-two").expect("key"),
        AnthropicAccountIdentity::new(TEST_GRANT_BINDING).expect("account"),
    )
    .expect("independent route");
    assert!(
        binding.validate_route(&independently_wrapped).is_err(),
        "independently wrapping the rotated token must not reuse prepared authority"
    );
}
