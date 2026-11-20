use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use plakous::gateway::GatewayCustody;
use plakous::gateway::pins::anthropic::{CLAUDE_CREDENTIAL_HEADERS, CLAUDE_UPSTREAM_PINS};
use plakous::gateway::pins::openai::{CODEX_CREDENTIAL_HEADERS, CODEX_UPSTREAM_PINS};
use plakous::gateway::pins::xai::{SUPERGROK_CREDENTIAL_HEADERS, SUPERGROK_UPSTREAM_PINS};
use plakous::gateway::server::{InProcessGateway, LocalCustody, http1_upstream, new_instance};
use plakous::subscriptions::custody::SubscriptionCustodian;
use plakous::subscriptions::test_support::TestDialOrigin;
use plakous::subscriptions::{NativeProvider, NativeSubscriptions, SubscriptionPaths};
use plakous_host_protocol::wire::{
    Answer, ClientFrame, CreationModel, Hello, HostFrame, NodeAddress, Op, Outcome, RequestRefusal,
    RunnerLabel, WIRE_VERSION, WorkspaceOptions, WorkspaceProbe,
};
use plakous_host_protocol::{Draft, SessionIntent, SessionObservation};
use serde_json::json;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _};

use super::{Bound, HostLock, SessionHost, next, send};
use crate::commands::application::{AccountOps as _, PlakousApplication};
use crate::gateway::HostCustody;
use crate::home::paths::PlakPaths;
use crate::host::sessions::ProductSessions;
use crate::shell::services::Services;
use crate::surface_protocol::PlakExtension;

const CODEX: &str = "gpt-5.6-luna";
const GROK: &str = "grok-4.5";
const CLAUDE: &str = "claude-sonnet-5";
const MODELS: [(&str, NativeProvider, &str); 3] = [
    ("codex", NativeProvider::Codex, CODEX),
    ("grok", NativeProvider::Xai, GROK),
    ("claude", NativeProvider::Claude, CLAUDE),
];

struct Wire {
    client: std::os::unix::net::UnixStream,
    reader: BufReader<std::os::unix::net::UnixStream>,
    request_id: u64,
}

impl Wire {
    fn request(&mut self, op: Op<PlakExtension>) -> Outcome<PlakExtension> {
        self.request_id += 1;
        send(
            &mut self.client,
            &ClientFrame::Request {
                id: self.request_id,
                op,
            },
        );
        loop {
            if let HostFrame::Response { id, outcome } = next(&mut self.reader)
                && id == self.request_id
            {
                return outcome;
            }
        }
    }

    fn attach(&mut self, node: &NodeAddress) -> u64 {
        match self.request(Op::Attach { node: node.clone() }) {
            Outcome::Ok(Answer::Attached { attachment }) => attachment,
            other => panic!("attach: {other:?}"),
        }
    }

    fn intent(
        &mut self,
        attachment: u64,
        intent: SessionIntent<PlakExtension>,
    ) -> Outcome<PlakExtension> {
        self.request(Op::Intent {
            attachment,
            client_id: matches!(&intent, SessionIntent::Prompt(_))
                .then(|| uuid::Uuid::new_v4().to_string().parse().unwrap()),
            intent,
            blobs: Vec::new(),
        })
    }

    fn fresh_codex(&mut self, workspace: &Path) -> NodeAddress {
        match self.request(Op::NewSession {
            runner: RunnerLabel::local(),
            profile: None,
            model: Some(CreationModel {
                account: "codex".into(),
                model: CODEX.into(),
            }),
            workspace: workspace.to_str().unwrap().to_owned(),
            template: Some("fixture-codex".into()),
            request_id: uuid::Uuid::new_v4().to_string(),
            accounts: Vec::new(),
        }) {
            Outcome::Ok(Answer::Created { node, .. }) => node,
            other => panic!("new Codex root: {other:?}"),
        }
    }

    fn assert_readiness(&mut self, attachment: u64, account: &str) {
        assert_accepted(self.intent(attachment, SessionIntent::RefreshReadiness));
        loop {
            if let HostFrame::Event {
                attachment: observed,
                observation: SessionObservation::ProviderSnapshot { providers },
                ..
            } = next(&mut self.reader)
                && observed == attachment
            {
                assert!(providers.iter().any(|provider| provider.id == account
                    && provider.state == plakous_host_protocol::views::ProviderState::Ready));
                return;
            }
        }
    }
}

fn prompt() -> SessionIntent<PlakExtension> {
    SessionIntent::Prompt(Draft::from("gateway fixture prompt"))
}

fn assert_accepted(outcome: Outcome<PlakExtension>) {
    assert_eq!(outcome, Outcome::Ok(Answer::Accepted { duplicate: false }));
}

fn catalog(line: &str) -> Option<serde_json::Value> {
    if line.starts_with("GET /backend-api/codex/models?") {
        Some(json!({"models": [{
            "slug": CODEX, "visibility": "list", "priority": 1,
            "supported_in_api": true, "tool_mode": "code_mode_only",
            "use_responses_lite": true,
            "supported_reasoning_levels": [{"effort": "low"}],
            "shell_type": "unified_exec", "input_modalities": ["text"],
            "context_window": 272_000, "max_context_window": 1_000_000,
            "effective_context_window_percent": 95, "auto_compact_token_limit": 240_000
        }]}))
    } else if line.starts_with("GET /v1/models ") {
        Some(json!({"object": "list", "data": [{
            "id": GROK, "object": "model", "created": 1, "owned_by": "xai",
            "contextWindow": 256_000
        }]}))
    } else {
        None
    }
}

async fn fixture_upstream(
    listener: tokio::net::TcpListener,
    catalogs: Arc<AtomicUsize>,
    inference: tokio::sync::mpsc::UnboundedSender<String>,
) {
    while let Ok((socket, _)) = listener.accept().await {
        let mut socket = tokio::io::BufReader::new(socket);
        let mut line = String::new();
        socket.read_line(&mut line).await.unwrap();
        let catalog = catalog(&line);
        let is_inference = [
            "POST /backend-api/codex/responses ",
            "POST /v1/responses ",
            "POST /v1/messages?",
        ]
        .iter()
        .any(|path| line.starts_with(path));
        let mut length = 0;
        loop {
            line.clear();
            socket.read_line(&mut line).await.unwrap();
            if line == "\r\n" || line.is_empty() {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse().unwrap();
            }
        }
        let mut body = vec![0; length];
        socket.read_exact(&mut body).await.unwrap();
        let (status, reply) = if let Some(catalog) = catalog {
            catalogs.fetch_add(1, Ordering::SeqCst);
            ("200 OK", catalog)
        } else {
            if is_inference {
                let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert!(request.to_string().contains("gateway fixture prompt"));
                inference
                    .send(request["model"].as_str().unwrap().to_owned())
                    .unwrap();
            }
            (
                "400 Bad Request",
                json!({
                    "error": {"message": "fixture refusal", "type": "invalid_request_error"}
                }),
            )
        };
        let reply = reply.to_string();
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
            reply.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
    }
}

fn start_gateway(
    runtime: &tokio::runtime::Runtime,
    paths: &PlakPaths,
    gateway_state: &Path,
    catalogs: Arc<AtomicUsize>,
    inference: tokio::sync::mpsc::UnboundedSender<String>,
) -> (HostCustody, impl FnOnce()) {
    for (_, provider, _) in MODELS {
        let notice = NativeSubscriptions::notice(provider).unwrap();
        crate::gateway::gateway_seed::seed(gateway_state, provider, None, notice.expires_at());
    }
    let subscriptions =
        NativeSubscriptions::new(SubscriptionPaths::from_xdg_state_home(gateway_state).unwrap())
            .with_clock(crate::commands::application::test_clock());
    let upstream = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let upstream_addr = upstream.local_addr().unwrap();
    runtime.spawn(fixture_upstream(upstream, catalogs, inference));
    let local = LocalCustody::new(new_instance().unwrap())
        .with_kind(
            "codex",
            Arc::new(SubscriptionCustodian::new(
                subscriptions.clone(),
                NativeProvider::Codex,
            )),
            CODEX_UPSTREAM_PINS,
        )
        .with_kind(
            "grok",
            Arc::new(SubscriptionCustodian::new(
                subscriptions.clone(),
                NativeProvider::Xai,
            )),
            SUPERGROK_UPSTREAM_PINS,
        )
        .with_kind(
            "claude",
            Arc::new(SubscriptionCustodian::new(
                subscriptions,
                NativeProvider::Claude,
            )),
            CLAUDE_UPSTREAM_PINS,
        );
    let dial = TestDialOrigin::loopback_http(upstream_addr.ip(), upstream_addr.port()).unwrap();
    let gateway = InProcessGateway::server(local, http1_upstream().unwrap())
        .with_dial_override("https://chatgpt.com", dial.clone())
        .with_dial_override("https://cli-chat-proxy.grok.com", dial.clone())
        .with_dial_override("https://api.anthropic.com", dial);
    let custody =
        GatewayCustody::connect(
            InProcessGateway::endpoint_for(&gateway),
            None,
            |kind| match kind {
                "codex" => Some(CODEX_CREDENTIAL_HEADERS),
                "grok" => Some(SUPERGROK_CREDENTIAL_HEADERS),
                "claude" => Some(CLAUDE_CREDENTIAL_HEADERS),
                _ => None,
            },
        )
        .unwrap();
    HostCustody::in_process_for_test(custody, paths, gateway_state)
}

fn recorded_root(services: &Services, workspace: &Path, account: &str, model: &str) -> NodeAddress {
    let mut record = crate::store::sessions::SessionRecord::new(workspace.to_owned());
    record.account = Some(account.into());
    record.model = Some(model.into());
    record.effort = Some("low".into());
    services
        .session_records()
        .unwrap()
        .save(&mut record)
        .unwrap();
    NodeAddress {
        tree_id: record.node.tree_id,
        agent_id: record.node.agent_id,
    }
}

fn start_host(
    runtime: &tokio::runtime::Runtime,
    directory: &Path,
    services: &Services,
) -> (Wire, Arc<AtomicBool>) {
    let host = SessionHost::new(
        ProductSessions::new(services, None, Some(runtime.handle().clone())).unwrap(),
        "fixture",
        1,
    );
    let socket = directory.join("s");
    let lock = HostLock::take(&directory.join("host.lock"), "fixture").unwrap();
    let mut bound = {
        let _runtime = runtime.enter();
        Bound::bind(&socket, &lock).unwrap()
    };
    let stopping = Arc::new(AtomicBool::new(false));
    let stop = Arc::clone(&stopping);
    runtime.spawn(async move {
        let _lock = lock;
        bound
            .serve(
                &host,
                move || stop.load(Ordering::Acquire),
                None,
                Arc::default(),
                || {},
            )
            .await;
    });
    let client = std::os::unix::net::UnixStream::connect(socket).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    let mut wire = Wire {
        reader: BufReader::new(client.try_clone().unwrap()),
        client,
        request_id: 0,
    };
    send(
        &mut wire.client,
        &ClientFrame::Hello(Hello {
            proto: WIRE_VERSION,
            binary_id: "client".into(),
            caps: Vec::new(),
        }),
    );
    assert!(matches!(next(&mut wire.reader), HostFrame::Welcome(_)));
    (wire, stopping)
}

fn assert_picker(wire: &mut Wire, workspace: &Path) {
    let Outcome::Ok(Answer::Options { options }) = wire.request(Op::SessionOptions {
        workspace: Some(WorkspaceProbe {
            runner: RunnerLabel::local(),
            path: workspace.to_str().unwrap().to_owned(),
        }),
    }) else {
        panic!("session picker response missing")
    };
    let Some(WorkspaceOptions::Valid { models, .. }) = options.workspace else {
        panic!("workspace picker response missing")
    };
    for catalog in [options.models, models] {
        for (account, _, model) in MODELS {
            assert!(
                catalog.accounts.iter().any(|section| section.ready
                    && section.account == account
                    && section.entries.iter().any(|entry| entry.id == model)),
                "picker missing {account}/{model}: {catalog:?}"
            );
        }
    }
}

fn assert_cold_codex(wire: &mut Wire, root: &NodeAddress) {
    let cold = wire.attach(root);
    for intent in [SessionIntent::RefreshReadiness, prompt()] {
        let outcome = wire.intent(cold, intent);
        let Outcome::Refused(RequestRefusal::SessionUnavailable { detail }) = outcome else {
            panic!("cold Codex root must fail closed: {outcome:?}");
        };
        assert!(
            detail.contains("with the same Plak home and gateway"),
            "{detail}"
        );
    }
}

fn assert_cache_boundary(paths: &PlakPaths, gateway_state: &Path, host_state: &Path) {
    assert!(!host_state.join("plak/subscriptions").exists());
    assert_eq!(
        std::fs::read_dir(gateway_state.join("plak/subscriptions/catalog"))
            .unwrap()
            .count(),
        0
    );
    let cache = paths.home().join("cache/subscription-catalogs");
    let entries = std::fs::read_dir(cache)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(entries.len(), 3);
    assert!(
        entries
            .iter()
            .all(|entry| super::mode(&entry.path()) == 0o600)
    );
}

#[test]
#[ignore = "run with -p plakd; the workspace test graph unifies provider fixture clocks with the product"]
fn gateway_host_wire_uses_host_catalogs_and_dispatches_subscription_prompts() {
    let (directory, paths) = PlakPaths::isolated_test_home();
    let workspace = directory.path().join("work");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir_all(paths.user_agents_dir()).unwrap();
    std::fs::write(
        paths.user_agents_dir().join("fixture-codex.toml"),
        format!("account = \"codex\"\nmodel = \"{CODEX}\"\neffort = \"low\"\ndescription = \"fixture root\"\n"),
    )
    .unwrap();
    let gateway_state = directory.path().join("gateway-state");
    let host_state = directory.path().join("host-state");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let catalogs = Arc::new(AtomicUsize::new(0));
    let (inference, mut dispatched) = tokio::sync::mpsc::unbounded_channel();
    let (custody, drain_gateway) = start_gateway(
        &runtime,
        &paths,
        &gateway_state,
        Arc::clone(&catalogs),
        inference,
    );
    let services = Services::for_home_with_state(paths.clone(), host_state.clone())
        .with_test_custody(custody.clone());
    let prepared = crate::commands::runner::registry::prepare(&services, None).unwrap();
    let services = prepared.services;
    services.runners().ready_local(prepared.local, 1).unwrap();
    let existing = recorded_root(&services, &workspace, "codex", CODEX);
    let (mut wire, stopping) = start_host(&runtime, directory.path(), &services);
    let fresh = wire.fresh_codex(&workspace);
    for root in [&existing, &fresh] {
        assert_cold_codex(&mut wire, root);
    }
    assert_eq!(
        catalogs.load(Ordering::SeqCst),
        0,
        "cold attach must not discover"
    );

    // The CLI's models backend runs outside the host and repairs its cache.
    let mut app = PlakousApplication::new(host_state.clone(), None, custody)
        .unwrap()
        .with_account_home(Some(paths.clone()));
    for (account, _, exact) in MODELS {
        assert!(
            app.models(&account.parse().unwrap())
                .unwrap()
                .models
                .iter()
                .any(|model| model.exact_id == exact && model.unavailable_reason.is_none())
        );
    }
    assert_picker(&mut wire, &workspace);
    assert_eq!(
        catalogs.load(Ordering::SeqCst),
        2,
        "picker reads cache; Claude enumerates enrolled models"
    );

    for (root, account, expected) in [
        (existing, "codex", CODEX),
        (fresh, "codex", CODEX),
        (
            recorded_root(&services, &workspace, "grok", GROK),
            "grok",
            GROK,
        ),
        (
            recorded_root(&services, &workspace, "claude", CLAUDE),
            "claude",
            CLAUDE,
        ),
    ] {
        let warm = wire.attach(&root);
        wire.assert_readiness(warm, account);
        assert_accepted(wire.intent(warm, prompt()));
        let actual = runtime
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(10), dispatched.recv()).await
            })
            .unwrap()
            .unwrap();
        assert_eq!(actual, expected);
    }
    assert_cache_boundary(&paths, &gateway_state, &host_state);
    stopping.store(true, Ordering::Release);
    drain_gateway();
}
