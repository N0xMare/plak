//! E4 (spec 02 §2.6, spec 05 rebuild-equals-incremental): a sealed-reasoning
//! final answer streamed through the real Anthropic Messages and OpenAI
//! Responses kernels persists one thinking row and one assistant row, and
//! deleting the views and reopening rebuilds exactly those rows.
//!
//! The SSE is synthetic: fixed fixture text, a fixture signature and a
//! fixture ciphertext, never a provider capture.

use std::sync::Arc;

use plakous_http::{Client, Limits, Origin, TlsConfigIdentity};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::*;
use crate::binding::PlatformProvider;
use crate::runtime::test_support::fresh_injected_attached_host;

const THINKING: &str = "Synthetic plan: the fixture asks for one short answer.";
const ANSWER: &str = "Synthetic answer from the sealed-reasoning fixture.";

fn sse(provider: PlatformProvider, model: &str) -> String {
    let events = if provider == PlatformProvider::Anthropic {
        vec![
            serde_json::json!({"type":"message_start","message":{"id":"msg_fixture","type":"message","role":"assistant","model":model,"content":[],"stop_reason":null,"usage":{"input_tokens":8,"output_tokens":0}}}),
            serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
            serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":THINKING}}),
            serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"fixture-signature"}}),
            serde_json::json!({"type":"content_block_stop","index":0}),
            serde_json::json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
            serde_json::json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":ANSWER}}),
            serde_json::json!({"type":"content_block_stop","index":1}),
            serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":12}}),
            serde_json::json!({"type":"message_stop"}),
        ]
    } else {
        let summary = serde_json::json!({"type":"summary_text","text":THINKING});
        let reasoning = serde_json::json!({"type":"reasoning","id":"rs_fixture","status":"completed","summary":[summary],"encrypted_content":"fixture-ciphertext"});
        let message = serde_json::json!({"type":"message","id":"msg_fixture","role":"assistant","status":"completed","content":[{"type":"output_text","text":ANSWER,"annotations":[]}]});
        vec![
            serde_json::json!({"type":"response.created","response":{"id":"resp_fixture","object":"response","status":"in_progress","model":model}}),
            serde_json::json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_fixture","status":"in_progress","summary":[]}}),
            serde_json::json!({"type":"response.reasoning_summary_part.added","item_id":"rs_fixture","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":""}}),
            serde_json::json!({"type":"response.reasoning_summary_text.delta","item_id":"rs_fixture","output_index":0,"summary_index":0,"delta":THINKING}),
            serde_json::json!({"type":"response.reasoning_summary_text.done","item_id":"rs_fixture","output_index":0,"summary_index":0,"text":THINKING}),
            serde_json::json!({"type":"response.reasoning_summary_part.done","item_id":"rs_fixture","output_index":0,"summary_index":0,"part":summary}),
            serde_json::json!({"type":"response.output_item.done","output_index":0,"item":reasoning}),
            serde_json::json!({"type":"response.output_item.added","output_index":1,"item":{"type":"message","id":"msg_fixture","status":"in_progress","role":"assistant","content":[]}}),
            serde_json::json!({"type":"response.content_part.added","item_id":"msg_fixture","output_index":1,"content_index":0,"part":{"type":"output_text","text":""}}),
            serde_json::json!({"type":"response.output_text.delta","item_id":"msg_fixture","output_index":1,"content_index":0,"delta":ANSWER}),
            serde_json::json!({"type":"response.output_text.done","item_id":"msg_fixture","output_index":1,"content_index":0,"text":ANSWER}),
            serde_json::json!({"type":"response.content_part.done","item_id":"msg_fixture","output_index":1,"content_index":0,"part":{"type":"output_text","text":ANSWER,"annotations":[]}}),
            serde_json::json!({"type":"response.output_item.done","output_index":1,"item":message}),
            serde_json::json!({"type":"response.completed","response":{"id":"resp_fixture","object":"response","status":"completed","model":model,"output":[reasoning,message],"usage":{"input_tokens":8,"output_tokens":12,"total_tokens":20}}}),
        ]
    };
    events
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
        .collect()
}

/// A loopback TLS endpoint answering every request with `body`.
async fn endpoint(body: String) -> (Client, Origin, tokio::task::JoinHandle<()>) {
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
        TlsConfigIdentity::from_parts(1, &[b"sealed-reasoning", cert.der().as_ref()]).unwrap(),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = Origin::parse(&format!("https://{}", listener.local_addr().unwrap())).unwrap();
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nrequest-id: req_fixture\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let peer = tokio::spawn(async move {
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server));
        loop {
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
            let size: usize = String::from_utf8_lossy(&bytes[..end])
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap_or(0);
            while bytes.len() < end + size {
                let count = stream.read(&mut buffer).await.unwrap();
                assert_ne!(count, 0);
                bytes.extend_from_slice(&buffer[..count]);
            }
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });
    (client, origin, peer)
}

/// Injects the production binding of `provider` as the root runtime: built
/// on a scoped thread so the host's own runtime never nests a `block_on`.
fn inject(
    config: &mut crate::runtime::HostConfig<TestExtension>,
    server: &tokio::runtime::Handle,
    provider: PlatformProvider,
    model: &'static str,
    opening: bool,
    client: &Client,
    origin: &Origin,
) {
    let handle = server.clone();
    let workspace = config.cwd.clone();
    let (client, origin) = (client.clone(), origin.clone());
    let build = move || {
        let (handle, workspace, client, origin) = (
            handle.clone(),
            workspace.clone(),
            client.clone(),
            origin.clone(),
        );
        std::thread::spawn(move || {
            handle.block_on(crate::platform::tests::compaction::platform_binding(
                provider, model, &workspace, opening, client, origin,
            ))
        })
        .join()
        .unwrap()
    };
    let target = build().target_binding();
    config.test_injection = Some(Arc::new(crate::runtime::TestHostInjection {
        runtime: Arc::new(move |_| Ok(build())),
        rebind_target: target,
        observe: Arc::new(|_, _| {}),
        child_runtime: None,
        rebind_builds: std::sync::Mutex::new(Vec::new()),
    }));
}

/// The reasoning text the host streamed live for the one turn.
fn live_reasoning(events: &[HostEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            HostEvent::ReasoningDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn run_prompt(config: crate::runtime::HostConfig<TestExtension>, prompt: &str) -> Vec<HostEvent> {
    let (actions, events, worker) = crate::runtime::startup::spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    send_attached_action(&actions, UserAction::Prompt(prompt.to_owned().into()));
    loop {
        let event = events
            .recv_timeout(Duration::from_secs(30))
            .unwrap_or_else(|_| panic!("a host event after {seen:#?}"));
        let done = matches!(&event, HostEvent::TurnLifecycle(lifecycle)
            if lifecycle.phase != plakous_host_protocol::TurnPhase::Admitted);
        seen.push(event);
        if done {
            break;
        }
    }
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();
    seen
}

#[test]
fn a_sealed_reasoning_final_answer_rebuilds_to_its_live_thinking_and_assistant_rows() {
    let server = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    for (provider, model) in [
        (PlatformProvider::Anthropic, "claude-sonnet-5"),
        (PlatformProvider::OpenAi, "gpt-5.6-luna"),
    ] {
        let (client, origin, peer) = server.block_on(endpoint(sse(provider, model)));
        let root = tempfile::tempdir().unwrap();
        let (mut config, _) = fresh_injected_attached_host(root.path());
        let store = Arc::new(MemoryStore::new());
        config.store = store.clone();
        inject(
            &mut config,
            server.handle(),
            provider,
            model,
            false,
            &client,
            &origin,
        );
        let id = config.session.id.clone();
        let seen = run_prompt(config, "answer the fixture");
        assert_eq!(
            live_reasoning(&seen),
            THINKING,
            "{provider:?}: live reasoning"
        );
        assert!(
            seen.iter()
                .any(|event| matches!(event, HostEvent::TurnLifecycle(lifecycle)
                if lifecycle.phase == plakous_host_protocol::TurnPhase::Completed)),
            "{provider:?}: the turn completed: {seen:#?}"
        );

        let live = caught_up(root.path(), &store, &id);
        let shown: Vec<String> = live
            .rows
            .values()
            .map(|row| match row {
                TranscriptRow::User { text, .. } => format!("user:{text}"),
                TranscriptRow::Thinking(thinking) => format!("thinking:{}", thinking.text),
                TranscriptRow::Assistant { text } => format!("assistant:{text}"),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(
            shown,
            [
                "user:answer the fixture".to_owned(),
                format!("thinking:{THINKING}"),
                format!("assistant:{ANSWER}"),
            ],
            "{provider:?}: one thinking row and one assistant row"
        );

        store.delete_views(&id);
        let mut config = reopened(root.path(), &store, &id);
        inject(
            &mut config,
            server.handle(),
            provider,
            model,
            true,
            &client,
            &origin,
        );
        run_prompts(config, &[]);
        let rebuilt = caught_up(root.path(), &store, &id);
        assert_eq!(rebuilt.rows, live.rows, "{provider:?}: rebuilt rows");
        assert_eq!(
            rebuilt.state.counters, live.state.counters,
            "{provider:?}: rebuilt counters"
        );
        peer.abort();
    }
}
