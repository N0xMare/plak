//! Real production adapters through durable public session compaction.
use super::*;
use crate::platform::ApiWindow;
use plakous::sessions::*;
use plakous_http::{Client, Limits, Origin, TlsConfigIdentity};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

async fn runtime(
    provider: PlatformProvider,
    spec: &ApiRuntimeSpec<'_>,
    client: Client,
    origin: Origin,
) -> SessionRuntimeBinding {
    runtime_with_source(provider, spec, client, origin, None, ApiWindow::Working)
        .await
        .0
}

async fn runtime_with_source(
    provider: PlatformProvider,
    spec: &ApiRuntimeSpec<'_>,
    client: Client,
    origin: Origin,
    source: Option<Arc<dyn plakous::anthropic::AnthropicApiKeySource>>,
    window: ApiWindow,
) -> (SessionRuntimeBinding, plakous::model::Digest) {
    let native_ptc = spec.composition == CompositionSelection::NativePtc;
    let registry = crate::agent::coding_tool_registry_with_extensions(
        spec.plan_state_authority,
        spec.composition,
        native_ptc,
        spec.topology_extensions,
    )
    .unwrap();
    let row = platform_model(provider, spec.literal_model).unwrap();
    let behavior = platform_behavior(
        provider,
        spec.effort.unwrap_or(ReasoningEffort::Low),
        &row,
        native_ptc,
        crate::api_runtime::configured_tools(
            crate::coding_routes::platform_presentation(provider),
            spec.composition,
            spec.wire,
            &registry,
            &[],
        )
        .unwrap(),
    )
    .unwrap();
    let maximum = NonZeroU64::new(row.window(window)).unwrap();
    let keys = Credentials("synthetic-compaction-key");
    let configured = match provider {
        PlatformProvider::Anthropic => {
            plakous::anthropic::AnthropicLanguageModel::configured_with_local_transport(
                source.unwrap_or_else(|| keys.anthropic().unwrap()),
                "plak/anthropic",
                row.anthropic.as_ref().unwrap(),
                &row.binding_evidence(provider, window),
                behavior,
                maximum,
                client,
                origin,
            )
            .unwrap()
        }
        PlatformProvider::OpenAi => {
            use plakous::openai::*;
            let authority = OpenAiPlatformModelBuilder::new(
                CredentialReference::application("plak/openai").unwrap(),
                keys.openai().unwrap(),
                OpenAiPlatformModelFacts::from_model_id(spec.literal_model).unwrap(),
                PlatformStoragePolicy::Stateless,
                PlatformProjectPolicy::DefaultProject,
            )
            .context_envelope(maximum)
            .seal()
            .unwrap();
            let provisional = authority.selected_configuration(behavior).unwrap();
            let selected = authority
                .selected_configuration(exact_route_retention(&provisional))
                .unwrap();
            Arc::new(
                OpenAiPlatformModel::with_local_transport(authority, selected, client, origin)
                    .unwrap(),
            )
            .configured()
            .await
            .unwrap()
        }
        PlatformProvider::Xai => {
            use plakous::xai::*;
            let source = keys.xai().unwrap();
            let authority = source.grant().unwrap().authority().clone();
            let provisional = xai_platform_selection_for(spec.literal_model, behavior).unwrap();
            let selected =
                xai_platform_selection_for(spec.literal_model, exact_route_retention(&provisional))
                    .unwrap();
            Arc::new(
                XaiPlatformModel::with_local_transport(
                    authority,
                    source,
                    maximum,
                    selected,
                    Arc::new(plakous::model::SystemClock),
                    client,
                    origin,
                )
                .unwrap(),
            )
            .configured()
            .await
            .unwrap()
        }
    };
    let descriptor = configured
        .registered()
        .descriptor()
        .binding_digest()
        .unwrap();
    (
        crate::api_runtime::runtime_binding(spec, configured, registry).unwrap(),
        descriptor,
    )
}

/// A production platform binding for `model` over a loopback transport, for
/// host-level tests that inject it as the root runtime.
pub(crate) async fn platform_binding(
    provider: PlatformProvider,
    model: &'static str,
    workspace: &std::path::Path,
    opening: bool,
    client: Client,
    origin: Origin,
) -> SessionRuntimeBinding {
    let extensions = crate::tools::ProductToolExtensions::default();
    let spec = ApiRuntimeSpec {
        job_config: plakous::tools::JobConfig::default(),
        account: provider.account(),
        sidecars: &NoWorkers,
        product_home: None,
        literal_model: model,
        effort: Some(ReasoningEffort::Low),
        composition: CompositionSelection::Direct,
        wire: provider.protocol(),
        workspace_root: workspace,
        cwd: workspace,
        plan_state_authority: "sealed-reasoning-fixture",
        opening_durable_session: opening,
        tool_fence: crate::agent::SessionToolFence::Full,
        scheduled_compaction: false,
        max_model_rounds: crate::agent::NATIVE_RUN_MAX_MODEL_ROUNDS,
        topology_extensions: &extensions,
        telemetry: None,
        spawned_child: false,
    };
    runtime(provider, &spec, client, origin).await
}

async fn endpoint(
    provider: PlatformProvider,
    model: &'static str,
) -> (
    Client,
    Origin,
    tokio::task::JoinHandle<Vec<serde_json::Value>>,
) {
    endpoint_with_cut(provider, model, None).await
}

async fn endpoint_with_cut(
    provider: PlatformProvider,
    model: &'static str,
    outage: Option<Arc<std::sync::atomic::AtomicBool>>,
) -> (
    Client,
    Origin,
    tokio::task::JoinHandle<Vec<serde_json::Value>>,
) {
    endpoint_with_requests(provider, model, outage, 3, None).await
}

async fn endpoint_with_requests(
    provider: PlatformProvider,
    model: &'static str,
    outage: Option<Arc<std::sync::atomic::AtomicBool>>,
    request_count: usize,
    heads: Option<Arc<std::sync::Mutex<Vec<String>>>>,
) -> (
    Client,
    Origin,
    tokio::task::JoinHandle<Vec<serde_json::Value>>,
) {
    endpoint_reporting(provider, model, outage, request_count, heads, vec![8]).await
}

/// The endpoint, reporting `input_tokens[n]` provider input usage on the
/// n-th response (the last value repeats).
async fn endpoint_reporting(
    provider: PlatformProvider,
    model: &'static str,
    outage: Option<Arc<std::sync::atomic::AtomicBool>>,
    request_count: usize,
    heads: Option<Arc<std::sync::Mutex<Vec<String>>>>,
    input_tokens: Vec<u64>,
) -> (
    Client,
    Origin,
    tokio::task::JoinHandle<Vec<serde_json::Value>>,
) {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let crypto = Arc::new(rustls::crypto::ring::default_provider());
    let server = rustls::ServerConfig::builder_with_provider(crypto.clone())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into(),
        )
        .unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.der().clone()).unwrap();
    let tls = rustls::ClientConfig::builder_with_provider(crypto)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let client = Client::with_tls_config_and_identity(
        Limits::default(),
        tls,
        TlsConfigIdentity::from_parts(1, &[b"hosted-compaction", cert.der().as_ref()]).unwrap(),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = Origin::parse(&format!("https://{}", listener.local_addr().unwrap())).unwrap();
    let peer = tokio::spawn(async move {
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server));
        let mut requests = Vec::new();
        for _ in 0..request_count {
            let (socket, _) = listener.accept().await.unwrap();
            let mut stream = acceptor.accept(socket).await.unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0; 4096];
            let end = loop {
                let count = stream.read(&mut buffer).await.unwrap();
                assert_ne!(count, 0);
                bytes.extend_from_slice(&buffer[..count]);
                if let Some(end) = bytes.windows(4).position(|value| value == b"\r\n\r\n") {
                    break end + 4;
                }
            };
            if let Some(heads) = &heads {
                heads
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&bytes[..end]).into_owned());
            }
            let size: usize = String::from_utf8_lossy(&bytes[..end])
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap();
            while bytes.len() < end + size {
                let count = stream.read(&mut buffer).await.unwrap();
                assert_ne!(count, 0);
                bytes.extend_from_slice(&buffer[..count]);
            }
            requests.push(serde_json::from_slice(&bytes[end..end + size]).unwrap());
            let input_tokens = input_tokens
                .get(requests.len() - 1)
                .or(input_tokens.last())
                .copied()
                .unwrap_or(8);
            let events = if provider == PlatformProvider::Anthropic {
                vec![
                    serde_json::json!({"type":"message_start","message":{"id":"msg_local","type":"message","role":"assistant","model":model,"content":[],"stop_reason":null,"usage":{"input_tokens":input_tokens,"output_tokens":0}}}),
                    serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                    serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"The user requested a local fixture. No files changed."}}),
                    serde_json::json!({"type":"content_block_stop","index":0}),
                    serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":12}}),
                    serde_json::json!({"type":"message_stop"}),
                ]
            } else {
                vec![
                    serde_json::json!({"type":"response.created","response":{"id":"resp_local","object":"response","status":"in_progress","model":model}}),
                    serde_json::json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_local","status":"in_progress","role":"assistant","content":[]}}),
                    serde_json::json!({"type":"response.content_part.added","output_index":0,"content_index":0,"part":{"type":"output_text","text":""}}),
                    serde_json::json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"The user requested a local fixture. No files changed."}),
                    serde_json::json!({"type":"response.output_text.done","output_index":0,"content_index":0,"text":"The user requested a local fixture. No files changed."}),
                    serde_json::json!({"type":"response.content_part.done","output_index":0,"content_index":0,"part":{"type":"output_text","text":"The user requested a local fixture. No files changed.","annotations":[]}}),
                    serde_json::json!({"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"msg_local","role":"assistant","status":"completed","content":[{"type":"output_text","text":"The user requested a local fixture. No files changed.","annotations":[]}]}}),
                    serde_json::json!({"type":"response.completed","response":{"id":"resp_local","object":"response","status":"completed","model":model,"output":[{"type":"message","id":"msg_local","role":"assistant","status":"completed","content":[{"type":"output_text","text":"The user requested a local fixture. No files changed.","annotations":[]}]}],"usage":{"input_tokens":input_tokens,"output_tokens":12,"total_tokens":input_tokens + 12}}}),
                ]
            };
            let body: String = events
                .into_iter()
                .enumerate()
                .map(|(index, mut event)| {
                    if provider != PlatformProvider::Anthropic {
                        event["sequence_number"] = serde_json::json!(index);
                    }
                    format!(
                        "event: {}\ndata: {event}\n\n",
                        event["type"].as_str().unwrap()
                    )
                })
                .collect();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nrequest-id: req_synthetic_compact\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            if requests.len() == 3
                && let Some(outage) = &outage
            {
                // Dispatch already owns its synthetic key. Only successor measurement fails.
                outage.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            stream.write_all(response.as_bytes()).await.unwrap();
        }
        requests
    });
    (client, origin, peer)
}

#[tokio::test]
async fn openai_product_tools_preserve_optional_schemas_on_production_wire() {
    let directory = tempfile::tempdir().unwrap();
    let extensions = crate::tools::ProductToolExtensions::default();
    let provider = PlatformProvider::OpenAi;
    let spec = ApiRuntimeSpec {
        job_config: plakous::tools::JobConfig::default(),
        account: provider.account(),
        sidecars: &NoWorkers,
        product_home: None,
        literal_model: "gpt-5.6-luna",
        effort: Some(ReasoningEffort::Low),
        composition: CompositionSelection::Direct,
        wire: provider.protocol(),
        workspace_root: directory.path(),
        cwd: directory.path(),
        plan_state_authority: "schema-fixture",
        opening_durable_session: false,
        tool_fence: crate::agent::SessionToolFence::Full,
        scheduled_compaction: false,
        max_model_rounds: crate::agent::NATIVE_RUN_MAX_MODEL_ROUNDS,
        topology_extensions: &extensions,
        telemetry: None,
        spawned_child: false,
    };
    let (client, origin, peer) =
        endpoint_with_requests(provider, "gpt-5.6-luna", None, 1, None).await;
    let binding = runtime(provider, &spec, client, origin).await;
    let sessions =
        AgentSessions::new(SessionBackend::sqlite(directory.path().join("schema.sqlite")).unwrap())
            .unwrap();
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("schema:create").unwrap(),
            binding,
        ))
        .await
        .unwrap()
    else {
        panic!("schema fixture session was not created")
    };
    let result = session
        .agent()
        .prompt("Return the synthetic fixture response without invoking tools.")
        .await
        .unwrap()
        .result()
        .await
        .unwrap();
    assert!(matches!(
        result.outcome(),
        plakous::agent::TurnResultOutcome::Completed(_)
    ));
    session.release().await.unwrap();
    let requests = tokio::time::timeout(std::time::Duration::from_secs(5), peer)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(requests.len(), 1);
    let wire_tools = requests[0]["tools"].as_array().unwrap();
    assert_ne!(wire_tools.as_slice(), [] as [serde_json::Value; 0]);
    // The OpenAI API wears the Codex pack: `apply_patch` is its one free-form
    // custom tool, and every function keeps its non-strict schema.
    for wire in wire_tools {
        if wire["name"] == "apply_patch" {
            assert_eq!(wire["type"], "custom");
            continue;
        }
        assert_eq!(wire["type"], "function");
        assert_eq!(wire["strict"], false);
    }
    let process_tools = plakous::tools::ProcessToolProjectionKind::CodexUnifiedExec
        .tools(plakous::tools::JobConfig::default())
        .unwrap();
    for name in ["exec_command", "write_stdin"] {
        let wire = wire_tools.iter().find(|wire| wire["name"] == name).unwrap();
        let registered = process_tools
            .iter()
            .find(|tool| tool.name().as_str() == name)
            .unwrap();
        assert_eq!(
            wire["parameters"],
            registered.schema().wire_document(),
            "{name}"
        );
    }
}

/// Codex Code Mode on the OpenAI API, on the production Platform wire: the
/// free-form `exec` custom tool with its Lark grammar and the `wait` function
/// are the only tools, parallel calls are off, nothing is stored, and the
/// request carries no Codex OAuth client identity.
#[tokio::test]
async fn openai_api_native_ptc_wire_is_code_mode_without_the_codex_costume() {
    let directory = tempfile::tempdir().unwrap();
    let worker = directory.path().join("uninvoked-worker");
    std::fs::write(&worker, b"offline artifact identity fixture").unwrap();
    let workers = FixtureWorkers(worker);
    let extensions = crate::tools::ProductToolExtensions::default();
    let provider = PlatformProvider::OpenAi;
    let composition = api_composition(
        provider,
        "gpt-5.6-luna",
        CompositionSelection::FollowCatalog,
    )
    .unwrap();
    assert_eq!(composition, CompositionSelection::NativePtc);
    let spec = ApiRuntimeSpec {
        job_config: plakous::tools::JobConfig::default(),
        account: provider.account(),
        sidecars: &workers,
        product_home: None,
        literal_model: "gpt-5.6-luna",
        effort: Some(ReasoningEffort::Low),
        composition,
        wire: provider.protocol(),
        workspace_root: directory.path(),
        cwd: directory.path(),
        plan_state_authority: "code-mode-fixture",
        opening_durable_session: false,
        tool_fence: crate::agent::SessionToolFence::Full,
        scheduled_compaction: false,
        max_model_rounds: crate::agent::NATIVE_RUN_MAX_MODEL_ROUNDS,
        topology_extensions: &extensions,
        telemetry: None,
        spawned_child: false,
    };
    let heads = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (client, origin, peer) =
        endpoint_with_requests(provider, "gpt-5.6-luna", None, 1, Some(heads.clone())).await;
    let binding = runtime(provider, &spec, client, origin).await;
    let sessions = AgentSessions::new(
        SessionBackend::sqlite(directory.path().join("code-mode.sqlite")).unwrap(),
    )
    .unwrap();
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("code-mode:create").unwrap(),
            binding,
        ))
        .await
        .unwrap()
    else {
        panic!("code mode fixture session was not created")
    };
    let result = session
        .agent()
        .prompt("Return the synthetic fixture response without invoking tools.")
        .await
        .unwrap()
        .result()
        .await
        .unwrap();
    assert!(
        matches!(
            result.outcome(),
            plakous::agent::TurnResultOutcome::Completed(_)
        ),
        "{:?}",
        result.outcome()
    );
    session.release().await.unwrap();
    let requests = tokio::time::timeout(std::time::Duration::from_secs(5), peer)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(requests.len(), 1);
    let body = &requests[0];
    let tools = body["tools"].as_array().unwrap();
    let names = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(names.len(), 2, "{names:?}");
    let exec = tools.iter().find(|tool| tool["name"] == "exec").unwrap();
    assert_eq!(exec["type"], "custom");
    assert_eq!(exec["format"]["type"], "grammar");
    assert_eq!(exec["format"]["syntax"], "lark");
    assert!(
        exec["format"]["definition"]
            .as_str()
            .is_some_and(|grammar| !grammar.is_empty())
    );
    let wait = tools.iter().find(|tool| tool["name"] == "wait").unwrap();
    assert_eq!(wait["type"], "function");
    assert_eq!(body["parallel_tool_calls"], false);
    assert_eq!(body["store"], false);
    assert_eq!(body["model"], "gpt-5.6-luna");
    let head = heads.lock().unwrap()[0].to_ascii_lowercase();
    let header_names = head
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':').map(|(name, _)| name.trim().to_owned()))
        .collect::<Vec<_>>();
    assert!(header_names.iter().any(|name| name == "authorization"));
    for name in &header_names {
        assert!(
            !matches!(
                name.as_str(),
                "originator" | "chatgpt-account-id" | "openai-beta" | "session_id" | "version"
            ) && !name.starts_with("x-codex")
                && !name.starts_with("x-openai-internal"),
            "Codex OAuth costume header `{name}` on the API wire: {header_names:?}"
        );
    }
    if let Some(agent) = head
        .lines()
        .find_map(|line| line.strip_prefix("user-agent:"))
    {
        assert!(agent.trim().starts_with("plak/"), "user-agent {agent}");
    }
}

#[tokio::test]
async fn production_api_sessions_compact_once_and_reopen_with_original_configuration() {
    for (provider, model) in [
        (PlatformProvider::Anthropic, "claude-sonnet-5"),
        (PlatformProvider::OpenAi, "gpt-5.6-luna"),
        (PlatformProvider::Xai, "grok-4.6"),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let extensions = crate::tools::ProductToolExtensions::default();
        let mut spec = ApiRuntimeSpec {
            job_config: plakous::tools::JobConfig::default(),
            account: provider.account(),
            sidecars: &NoWorkers,
            product_home: None,
            literal_model: model,
            effort: Some(ReasoningEffort::Low),
            composition: CompositionSelection::Direct,
            wire: provider.protocol(),
            workspace_root: directory.path(),
            cwd: directory.path(),
            plan_state_authority: "compaction-fixture",
            opening_durable_session: false,
            tool_fence: crate::agent::SessionToolFence::Full,
            scheduled_compaction: false,
            max_model_rounds: crate::agent::NATIVE_RUN_MAX_MODEL_ROUNDS,
            topology_extensions: &extensions,
            telemetry: None,
            spawned_child: false,
        };
        let (client, origin, peer) = endpoint(provider, model).await;
        let binding = runtime(provider, &spec, client.clone(), origin.clone()).await;
        let original = binding.target_binding().clone();
        let sessions = AgentSessions::new(
            SessionBackend::sqlite(directory.path().join("session.sqlite")).unwrap(),
        )
        .unwrap();
        let CreateSessionOutcome::Created { session, .. } = sessions
            .create(CreateSessionRequest::new(
                SessionOperationId::new("create").unwrap(),
                binding,
            ))
            .await
            .unwrap()
        else {
            panic!("not created")
        };
        for input in ["older intent", "latest protected intent"] {
            let result = session
                .agent()
                .prompt(input)
                .await
                .unwrap()
                .result()
                .await
                .unwrap();
            if let plakous::agent::TurnResultOutcome::Failed(error)
            | plakous::agent::TurnResultOutcome::RecoveryRequired(error) = result.outcome()
            {
                panic!("{provider:?}: {error}");
            }
            assert!(
                matches!(
                    result.outcome(),
                    plakous::agent::TurnResultOutcome::Completed(_)
                ),
                "{provider:?}: {:?}",
                result.outcome()
            );
        }
        let source = session.locator();
        spec.opening_durable_session = true;
        let binding = runtime(provider, &spec, client.clone(), origin.clone()).await;
        assert_eq!(binding.target_binding(), original);
        let outcome = sessions
            .compact(CompactSessionRequest::new(
                SessionOperationId::new("compact").unwrap(),
                source,
                CompactReason::Manual,
                binding.with_compaction_output_limit().unwrap(),
            ))
            .await
            .unwrap_or_else(|error| panic!("{provider:?} compact: {error}"));
        let CompactSessionOutcome::Compacted { target, .. } = outcome else {
            panic!("{provider:?}: {outcome:?}")
        };
        assert_eq!(session.locator(), target);
        let target = session.release().await.unwrap();
        let binding = runtime(provider, &spec, client, origin).await;
        assert_eq!(binding.target_binding(), original);
        let OpenSessionOutcome::Opened { session, .. } = sessions
            .open(OpenSessionRequest::new(
                SessionOperationId::new("reopen").unwrap(),
                target,
                binding,
            ))
            .await
            .unwrap()
        else {
            panic!("not opened")
        };
        session.release().await.unwrap();
        let requests = tokio::time::timeout(std::time::Duration::from_secs(5), peer)
            .await
            .unwrap()
            .unwrap();
        assert_compaction_wire(provider, &requests);
    }
}

fn assert_compaction_wire(provider: PlatformProvider, requests: &[serde_json::Value]) {
    // No manual thinking budget here, so the semantic summary bound is the
    // 8,192-token summary plus its default 8,192-token thinking reserve.
    let compaction_bound = plakous::model::SEMANTIC_COMPACTION_MAX_OUTPUT_TOKENS.get();
    assert_eq!(requests.len(), 3);
    assert_ne!(
        requests[0]["tools"].as_array().unwrap().as_slice(),
        [] as [serde_json::Value; 0]
    );
    assert_eq!(requests[0]["tools"], requests[1]["tools"]);
    assert_eq!(requests[0]["tools"], requests[2]["tools"]);
    if provider == PlatformProvider::Anthropic {
        assert_eq!(requests[0]["max_tokens"], 128_000);
        assert_eq!(requests[0]["thinking"], requests[2]["thinking"]);
        assert_eq!(requests[0]["output_config"], requests[2]["output_config"]);
        assert_eq!(requests[2]["max_tokens"], compaction_bound);
        assert_eq!(requests[2]["tool_choice"]["type"], "none");
    } else {
        let output_bound = if provider == PlatformProvider::Xai {
            // xAI publishes no output limit; the bound is reserved out of
            // the shared context.
            128_000
        } else {
            128_000
        };
        assert_eq!(requests[0]["max_output_tokens"], output_bound);
        assert_eq!(requests[0]["reasoning"], requests[2]["reasoning"]);
        assert_eq!(requests[2]["max_output_tokens"], compaction_bound);
        assert_eq!(requests[2]["tool_choice"], "none");
    }
}

struct RecoverableKey(Arc<std::sync::atomic::AtomicBool>);
impl plakous::anthropic::AnthropicApiKeySource for RecoverableKey {
    fn current_key(
        &self,
    ) -> Result<plakous::anthropic::AnthropicApiKey, plakous::anthropic::AnthropicError> {
        if self.0.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(plakous::anthropic::AnthropicError::new(
                plakous::anthropic::AnthropicErrorKind::Authentication,
                plakous::anthropic::AnthropicErrorPhase::Prepare,
                "synthetic source temporarily unavailable",
            ));
        }
        plakous::anthropic::AnthropicApiKey::new("synthetic-compaction-key")
    }
}

#[tokio::test]
async fn completed_compaction_recovers_after_reconstructing_fresh_model_without_redispatch() {
    let provider = PlatformProvider::Anthropic;
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("recovery.sqlite");
    let extensions = crate::tools::ProductToolExtensions::default();
    let mut spec = ApiRuntimeSpec {
        job_config: plakous::tools::JobConfig::default(),
        account: provider.account(),
        sidecars: &NoWorkers,
        product_home: None,
        literal_model: "claude-sonnet-5",
        effort: Some(ReasoningEffort::Low),
        composition: CompositionSelection::Direct,
        wire: provider.protocol(),
        workspace_root: directory.path(),
        cwd: directory.path(),
        plan_state_authority: "recovery-fixture",
        opening_durable_session: false,
        tool_fence: crate::agent::SessionToolFence::Full,
        scheduled_compaction: false,
        max_model_rounds: crate::agent::NATIVE_RUN_MAX_MODEL_ROUNDS,
        topology_extensions: &extensions,
        telemetry: None,
        spawned_child: false,
    };
    let outage = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let source: Arc<dyn plakous::anthropic::AnthropicApiKeySource> =
        Arc::new(RecoverableKey(outage.clone()));
    let (client, origin, peer) =
        endpoint_with_cut(provider, "claude-sonnet-5", Some(outage.clone())).await;
    let attempts = Arc::new(ExchangeStarts::default());
    let client = client.with_observer(Some(attempts.clone()));
    let (binding, descriptor) = runtime_with_source(
        provider,
        &spec,
        client.clone(),
        origin.clone(),
        Some(source.clone()),
        ApiWindow::Working,
    )
    .await;
    let original = binding.target_binding();
    let original_descriptor = descriptor;
    let sessions = AgentSessions::new(SessionBackend::sqlite(&database).unwrap()).unwrap();
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("create").unwrap(),
            binding,
        ))
        .await
        .unwrap()
    else {
        panic!("not created")
    };
    for input in ["older intent", "latest protected intent"] {
        let result = session
            .agent()
            .prompt(input)
            .await
            .unwrap()
            .result()
            .await
            .unwrap();
        assert!(matches!(
            result.outcome(),
            plakous::agent::TurnResultOutcome::Completed(_)
        ));
    }
    let locator = session.locator();
    spec.opening_durable_session = true;
    let (binding, descriptor) = runtime_with_source(
        provider,
        &spec,
        client.clone(),
        origin.clone(),
        Some(source.clone()),
        ApiWindow::Working,
    )
    .await;
    let paid_descriptor = descriptor;
    assert_ne!(paid_descriptor, original_descriptor);
    let operation = SessionOperationId::new("compact-recover").unwrap();
    let outcome = sessions
        .compact(CompactSessionRequest::new(
            operation.clone(),
            locator,
            CompactReason::Manual,
            binding.with_compaction_output_limit().unwrap(),
        ))
        .await
        .unwrap();
    assert!(
        matches!(outcome, CompactSessionOutcome::RecoveryRequired { .. }),
        "{outcome:?}"
    );
    session.release().await.unwrap();
    drop(sessions);
    let requests = peer.await.unwrap();
    assert_compaction_wire(provider, &requests);
    assert_eq!(attempts.0.load(std::sync::atomic::Ordering::SeqCst), 3);
    // The provider endpoint is now gone: finalized paid evidence must suffice.
    outage.store(false, std::sync::atomic::Ordering::SeqCst);
    let sessions = AgentSessions::new(SessionBackend::sqlite(&database).unwrap()).unwrap();
    let (binding, descriptor) = runtime_with_source(
        provider,
        &spec,
        client.clone(),
        origin.clone(),
        Some(source.clone()),
        ApiWindow::Working,
    )
    .await;
    assert_eq!(binding.target_binding(), original);
    assert_ne!(descriptor, paid_descriptor);
    let outcome = sessions
        .recover(RecoverSessionRequest::new(
            operation,
            locator,
            binding.with_compaction_output_limit().unwrap(),
        ))
        .await
        .unwrap();
    let RecoverSessionOutcome::Compacted { target, .. } = outcome else {
        panic!("recovery did not finalize: {outcome:?}")
    };
    let (binding, _) = runtime_with_source(
        provider,
        &spec,
        client,
        origin,
        Some(source),
        ApiWindow::Working,
    )
    .await;
    let OpenSessionOutcome::Opened { session, .. } = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("reopen").unwrap(),
            target,
            binding,
        ))
        .await
        .unwrap()
    else {
        panic!("recovered successor did not open")
    };
    session.release().await.unwrap();
    assert_eq!(
        attempts.0.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "recovery attempted another HTTP exchange"
    );
}

#[derive(Default)]
struct ExchangeStarts(std::sync::atomic::AtomicUsize);
impl plakous_http::ExchangeObserver for ExchangeStarts {
    fn start(&self, _body: &[u8]) -> Option<Box<dyn plakous_http::ExchangeObservation>> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        None
    }
}

/// The API working window is provider input tokens on every route, not
/// request bytes. Two prompts whose wire bytes together exceed the window are
/// sent; once the provider reports its input count, the meter compares that
/// count (plus a conservative byte charge for what was added since) against
/// the token window and its compaction trigger.
#[tokio::test]
async fn api_working_window_is_measured_in_provider_tokens() {
    use crate::context::{
        ContextDisposition, occupancy_requires_scheduled_compact, project_context_status,
    };
    for (provider, model) in [
        (PlatformProvider::Anthropic, "claude-haiku-5-5"),
        (PlatformProvider::OpenAi, "gpt-5.6-luna"),
        (PlatformProvider::Xai, "grok-4.6"),
    ] {
        let row = platform_model(provider, model).unwrap();
        let working = row.window(ApiWindow::Working);
        assert!(working < row.window(ApiWindow::Full), "{provider:?}");
        // Each prompt is 0.6 bytes per window token, so the second request's
        // wire is larger than the window while its tokens fit.
        let prompt = "word ".repeat(usize::try_from(working * 3 / 25).unwrap());
        let first_report = working * 3 / 20;
        // (reported input tokens, window, whether fence (a) compacts)
        for (reported, window, compacts) in [
            (working * 3 / 5, ApiWindow::Working, false),
            (working * 19 / 20, ApiWindow::Working, true),
            (working * 21 / 20, ApiWindow::Working, true),
            (working * 21 / 20, ApiWindow::Full, false),
        ] {
            let label = format!("{provider:?} reported={reported} window={window:?}");
            let directory = tempfile::tempdir().unwrap();
            let extensions = crate::tools::ProductToolExtensions::default();
            let spec = ApiRuntimeSpec {
                job_config: plakous::tools::JobConfig::default(),
                account: provider.account(),
                sidecars: &NoWorkers,
                product_home: None,
                literal_model: model,
                effort: Some(ReasoningEffort::Low),
                composition: CompositionSelection::Direct,
                wire: provider.protocol(),
                workspace_root: directory.path(),
                cwd: directory.path(),
                plan_state_authority: "window-fixture",
                opening_durable_session: false,
                tool_fence: crate::agent::SessionToolFence::Full,
                scheduled_compaction: false,
                max_model_rounds: crate::agent::NATIVE_RUN_MAX_MODEL_ROUNDS,
                topology_extensions: &extensions,
                telemetry: None,
                spawned_child: false,
            };
            let (client, origin, peer) =
                endpoint_reporting(provider, model, None, 2, None, vec![first_report, reported])
                    .await;
            let (binding, _) =
                runtime_with_source(provider, &spec, client, origin, None, window).await;
            let sessions = AgentSessions::new(
                SessionBackend::sqlite(directory.path().join("session.sqlite")).unwrap(),
            )
            .unwrap();
            let CreateSessionOutcome::Created { session, .. } = sessions
                .create(CreateSessionRequest::new(
                    SessionOperationId::new("create").unwrap(),
                    binding,
                ))
                .await
                .unwrap()
            else {
                panic!("{label}: not created")
            };
            for _ in 0..2 {
                let result = session
                    .agent()
                    .prompt(prompt.clone())
                    .await
                    .unwrap()
                    .result()
                    .await
                    .unwrap();
                let outcome = match result.outcome() {
                    plakous::agent::TurnResultOutcome::Completed(_) => None,
                    plakous::agent::TurnResultOutcome::Failed(error)
                    | plakous::agent::TurnResultOutcome::RecoveryRequired(error) => {
                        Some(error.to_string())
                    }
                    other @ plakous::agent::TurnResultOutcome::Cancelled(_) => {
                        Some(format!("{other:?}"))
                    }
                };
                assert_eq!(outcome, None, "{label}: a prompt was not sent");
            }
            let requests = peer.await.unwrap();
            let wire_bytes =
                u64::try_from(serde_json::to_vec(&requests[1]).unwrap().len()).unwrap();
            assert!(wire_bytes > working, "{label}: {wire_bytes} bytes");
            let status = sessions
                .context_status(ContextStatusRequest::new(
                    SessionOperationId::new("status").unwrap(),
                    session.locator(),
                    ContextQuery::WithPendingUserInput("next".to_owned().into()),
                ))
                .await
                .unwrap();
            let SessionContextStatusKind::Available(status) = status.kind() else {
                panic!("{label}: {:?}", status.kind())
            };
            let projection = project_context_status(status);
            let ceiling = projection.safe_admitted_input_ceiling().unwrap().tokens();
            assert_eq!(ceiling, row.window(window), "{label}");
            let occupied = projection.occupied_tokens().unwrap();
            assert!(
                occupied >= reported && occupied < reported + 4_096,
                "{label}: occupied {occupied} is not the provider count plus the new prompt"
            );
            assert!(projection.context_anchor().is_some(), "{label}: unanchored");
            assert_eq!(
                occupancy_requires_scheduled_compact(projection),
                compacts,
                "{label}: occupied {occupied}, trigger {:?}, disposition {:?}",
                projection.auto_compact_trigger(),
                projection.disposition()
            );
            if !compacts {
                assert_eq!(
                    projection.disposition(),
                    ContextDisposition::Fits,
                    "{label}"
                );
            }
            session.release().await.unwrap();
        }
    }
}

/// `none` is an explicit persisted effort. Startup reconstructs it and the
/// production request carries `reasoning.effort: "none"` on the routes whose
/// ladders publish it (OpenAI Responses `AllNamed`, xAI `NoneThroughXHigh`).
#[tokio::test]
async fn explicit_none_effort_is_reconstructed_and_sent_on_the_api_wire() {
    for (provider, model) in [
        (PlatformProvider::OpenAi, "gpt-6-luna"),
        (PlatformProvider::Xai, "grok-4.3"),
    ] {
        let reconstructed = crate::runtime::effort_from_record(Some("none"))
            .unwrap()
            .expect("none is explicit");
        assert_eq!(reconstructed, ReasoningEffort::None, "{provider:?}");
        assert_eq!(
            crate::runtime::effort_from_record(Some("NONE")).unwrap(),
            Some(ReasoningEffort::None)
        );
        let directory = tempfile::tempdir().unwrap();
        let extensions = crate::tools::ProductToolExtensions::default();
        let spec = ApiRuntimeSpec {
            job_config: plakous::tools::JobConfig::default(),
            account: provider.account(),
            sidecars: &NoWorkers,
            product_home: None,
            literal_model: model,
            effort: Some(reconstructed),
            composition: CompositionSelection::Direct,
            wire: provider.protocol(),
            workspace_root: directory.path(),
            cwd: directory.path(),
            plan_state_authority: "none-effort",
            opening_durable_session: false,
            tool_fence: crate::agent::SessionToolFence::Full,
            scheduled_compaction: false,
            max_model_rounds: crate::agent::NATIVE_RUN_MAX_MODEL_ROUNDS,
            topology_extensions: &extensions,
            telemetry: None,
            spawned_child: false,
        };
        let (client, origin, peer) = endpoint_with_requests(provider, model, None, 1, None).await;
        let binding = runtime(provider, &spec, client, origin).await;
        let sessions = AgentSessions::new(
            SessionBackend::sqlite(directory.path().join("none-effort.sqlite")).unwrap(),
        )
        .unwrap();
        let CreateSessionOutcome::Created { session, .. } = sessions
            .create(CreateSessionRequest::new(
                SessionOperationId::new("none-effort:create").unwrap(),
                binding,
            ))
            .await
            .unwrap()
        else {
            panic!("{provider:?}: session was not created")
        };
        let result = session
            .agent()
            .prompt("Reply OK")
            .await
            .unwrap()
            .result()
            .await
            .unwrap();
        assert!(
            matches!(
                result.outcome(),
                plakous::agent::TurnResultOutcome::Completed(_)
            ),
            "{provider:?}: {:?}",
            result.outcome()
        );
        session.release().await.unwrap();
        let requests = tokio::time::timeout(std::time::Duration::from_secs(5), peer)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(requests.len(), 1, "{provider:?}");
        assert_eq!(requests[0]["model"], model, "{provider:?}");
        assert_eq!(
            requests[0]["reasoning"]["effort"], "none",
            "{provider:?} request: {}",
            requests[0]
        );
        // A resume reads the stored spelling again and gets the same effort.
        assert_eq!(
            crate::runtime::effort_from_record(Some("none")).unwrap(),
            Some(reconstructed)
        );
    }
}
