//! Real-worker tests of the Python presentation on the Monty cell worker,
//! ported from the monty-pool session: each drives `plakous-py-host` through
//! the facade's session seam.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "integration tests against a live Python worker"
)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use plakous_agent::internal::{
    LocalCompositionContentItem, LocalCompositionEvent, LocalCompositionEventReceiver,
    LocalCompositionExecuteRequest, LocalCompositionNestedDelivery,
    LocalCompositionNestedResponder, LocalCompositionResponse, LocalCompositionSessionBundle,
    LocalCompositionSessionLoss, LocalCompositionShutdownResult, LocalCompositionTerminateResult,
    LocalCompositionWaitOutcome, LocalCompositionWaitRequest,
};
use plakous_cell::{BindingInput, CellClient, EngineConfig, MONTY_WORKER};

use super::python::{Monty, PYTHON_WORKER_REQUEST_BOUND};
use super::session::session_bundle;
use plakous_compose::{PythonInput, PythonPtcPresentationV1, PythonPtcV1, python_bootstrap};

const MAX_OUTSTANDING_NESTED: usize = 16;
const MAX_PRINT_BYTES: usize = 1_048_576 - 64;

struct PythonToolBinding {
    tool_name: String,
    input_kind: BindingInput,
}

fn fixture_registry(
    names: &[plakous_model::ToolName],
) -> Result<plakous_tools::FrozenToolRegistry, Box<dyn std::error::Error>> {
    use plakous_model::{
        ToolConcurrency, ToolDefinition, ToolEffect, ToolInputContract, ToolOutputContract,
    };
    use plakous_tools::{
        RegisteredToolId, ToolExecutorBindingIdentity, ToolExecutorId, ToolRegistration,
        ToolRegistryBuilder,
    };

    let mut registry = ToolRegistryBuilder::default();
    for (index, name) in names.iter().enumerate() {
        registry.register(
            ToolRegistration::new(
                RegisteredToolId::new(format!("fixture.python.{index}"))?,
                ToolExecutorId::new(format!("fixture.python.{index}"))?,
                ToolExecutorBindingIdentity::new(1, &[name.as_str().as_bytes()])?,
                ToolDefinition::new(
                    name.clone(),
                    "fixture",
                    "Python worker fixture tool.",
                    ToolInputContract::structured(plakous_tools::object_schema(
                        serde_json::json!({}),
                        &[],
                    ))?,
                    ToolOutputContract::utf8(4096)?,
                    ToolConcurrency::Parallel,
                    ToolEffect::ReadOnly,
                )?,
                Arc::new(plakous_test_support::ScriptedTool::new([])),
            )
            .nested_eligible(),
        )?;
    }
    PythonPtcV1::register_markers_into(&mut registry)?;
    Ok(registry.freeze())
}

fn fixture_bootstrap(
    bindings: &BTreeMap<String, PythonToolBinding>,
) -> Result<Option<plakous_cell::MontyBootstrap>, Box<dyn std::error::Error>> {
    use plakous_model::{Digest, DigestDomain, IntegrationClass, ToolName};

    if bindings.is_empty() {
        return Ok(None);
    }
    let names = bindings
        .keys()
        .map(|name| ToolName::new(name.clone()))
        .collect::<Result<Vec<_>, _>>()?;
    let configuration = serde_json::from_value(serde_json::to_value(Digest::sha256(
        DigestDomain::EffectiveProviderConfiguration,
        5,
        &[b"python-worker-fixture"],
    )?)?)?;
    let target = plakous_tools::ProviderModelProjectionIdentityV2::new(
        IntegrationClass::Custom,
        "custom.responses",
        "fixture.route",
        "fixture.python",
        "responses-family",
        "fixture.model",
        "fixture.model",
        configuration,
        1,
    )?;
    let descriptor = PythonPtcV1::compile(
        fixture_registry(&names)?,
        target,
        PythonPtcPresentationV1::StructuredFunctions,
        names,
    )?;
    if descriptor
        .nested_tools()
        .iter()
        .any(|tool| tool.python_name() != tool.canonical_name().as_str())
    {
        return Err("fixture exports must already be valid Python identifiers".into());
    }
    let mut bootstrap =
        python_bootstrap(descriptor.host_api())?.ok_or("fixture bootstrap missing")?;
    // Worker protocol tests select dispatch aliases and string/dictionary codecs
    // independently of the registry used to generate the coroutine source.
    for binding in &mut bootstrap.bindings {
        let fixture = &bindings[&binding.tool];
        binding.tool.clone_from(&fixture.tool_name);
        binding.input = match fixture.input_kind {
            BindingInput::Dictionary => PythonInput::Dictionary,
            BindingInput::String => PythonInput::String,
        };
    }
    Ok(Some(super::python::worker_bootstrap(bootstrap)))
}

/// The built `plakous-py-host`: `PLAK_PY_HOST`, else the test build's own.
fn py_host() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("PLAK_PY_HOST") {
        let path = PathBuf::from(&explicit);
        assert!(
            path.is_file(),
            "PLAK_PY_HOST is set to {} but the file is missing",
            path.display()
        );
        return Some(path);
    }
    std::env::current_exe()
        .ok()
        .and_then(|path| {
            path.parent()
                .and_then(Path::parent)
                .map(|directory| directory.join("plakous-py-host"))
        })
        .filter(|path| path.is_file())
}

async fn python_session_within(
    worker: &Path,
    bindings: BTreeMap<String, PythonToolBinding>,
    request_bound: Duration,
) -> Result<LocalCompositionSessionBundle, String> {
    let bootstrap = fixture_bootstrap(&bindings).map_err(|error| error.to_string())?;
    let client = CellClient::spawn(worker, MONTY_WORKER, EngineConfig::MONTY)
        .await
        .map_err(|error| error.to_string())?;
    Ok(session_bundle(
        Arc::new(client),
        Box::new(Monty::new(bootstrap, request_bound)),
    ))
}

async fn python_session(
    worker: &Path,
    bindings: BTreeMap<String, PythonToolBinding>,
) -> Result<LocalCompositionSessionBundle, String> {
    python_session_within(worker, bindings, PYTHON_WORKER_REQUEST_BOUND).await
}

#[test]
fn the_python_request_bound_is_ninety_seconds() {
    assert_eq!(PYTHON_WORKER_REQUEST_BOUND, Duration::from_secs(90));
}

#[tokio::test]
async fn killing_the_python_worker_does_not_kill_parent() {
    let Some(worker) = py_host() else { return };
    let client = CellClient::spawn(&worker, MONTY_WORKER, EngineConfig::MONTY)
        .await
        .expect("worker");
    let pid = client.child_id().expect("local worker has a pid");
    let status = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .expect("kill worker");
    assert!(status.success());
    let marker =
        std::env::temp_dir().join(format!("plak-py-host-parent-alive-{}", std::process::id()));
    std::fs::write(&marker, b"alive").expect("parent still writes after worker kill");
    assert_eq!(std::fs::read(&marker).expect("parent reads"), b"alive");
    let _ = std::fs::remove_file(&marker);
    let _ = client.wait_loss().await;
}

#[tokio::test]
async fn yielded_cell_wait_resumes_after_host_wait_and_is_cancellation_safe() {
    let Some(worker) = py_host() else { return };
    let bundle = python_session_within(
        &worker,
        BTreeMap::from([("wait_agent".into(), dictionary_tool("wait_agent"))]),
        Duration::from_secs(1),
    )
    .await
    .expect("bundle");
    let session = bundle.session;
    let mut events = bundle.events;
    let mut request = lazy_tool_request("print('before host wait')\nprint(await wait_agent({}))");
    request.yield_time_ms = 1;
    let started = session.execute(request).await.expect("execute");
    let id = started.cell_id;
    let LocalCompositionEvent::NestedCall { responder, .. } =
        events.receive().await.expect("events").expect("call")
    else {
        panic!("nested call")
    };
    let yielded = tokio::time::timeout(Duration::from_secs(2), started.initial_response)
        .await
        .expect("yield deadline")
        .expect("yield");
    let LocalCompositionResponse::Yielded { items, .. } = yielded else {
        panic!("yield")
    };
    assert!(items.iter().any(|item| matches!(item, LocalCompositionContentItem::Text(text) if text.contains("before host wait"))));
    let wait_request = LocalCompositionWaitRequest {
        cell_id: id.clone(),
        yield_time_ms: 60_000,
    };
    assert!(
        tokio::time::timeout(
            Duration::from_millis(1100),
            session.wait(wait_request.clone())
        )
        .await
        .is_err(),
        "host waits do not spend the worker compute bound; dropping observation keeps the cell"
    );
    assert_eq!(
        responder
            .resolve(serde_json::json!("settled"))
            .await
            .expect("delivery"),
        LocalCompositionNestedDelivery::Accepted
    );
    let LocalCompositionWaitOutcome::Live(LocalCompositionResponse::Result {
        items, error, ..
    }) = session.wait(wait_request).await.expect("wait")
    else {
        panic!("terminal result")
    };
    assert!(error.is_none());
    assert!(!items.iter().any(|item| matches!(item, LocalCompositionContentItem::Text(text) if text.contains("before host wait"))), "wait only returns unread output");
    assert!(items.iter().any(
        |item| matches!(item, LocalCompositionContentItem::Text(text) if text.contains("settled"))
    ));
    assert!(matches!(
        session
            .wait(LocalCompositionWaitRequest {
                cell_id: id,
                yield_time_ms: 0,
            })
            .await
            .expect("retired wait"),
        LocalCompositionWaitOutcome::Missing(_)
    ));
    let _ = session.shutdown().await;
}

#[tokio::test]
async fn yielded_cell_termination_joins_and_unknown_id_does_not_stop_it() {
    let Some(worker) = py_host() else { return };
    let bundle = python_session(
        &worker,
        BTreeMap::from([("wait_agent".into(), dictionary_tool("wait_agent"))]),
    )
    .await
    .expect("bundle");
    let session = bundle.session;
    let mut events = bundle.events;
    let mut request = lazy_tool_request("await wait_agent({})");
    request.yield_time_ms = 0;
    let started = session.execute(request).await.expect("execute");
    let id = started.cell_id;
    let LocalCompositionEvent::NestedCall { responder, .. } =
        events.receive().await.expect("events").expect("call")
    else {
        panic!("call")
    };
    assert!(matches!(
        started.initial_response.await.expect("yield"),
        LocalCompositionResponse::Yielded { .. }
    ));
    assert!(matches!(
        session.terminate("999".into()).await,
        LocalCompositionTerminateResult::Joined(LocalCompositionWaitOutcome::Missing(_))
    ));
    assert!(
        matches!(session.terminate(id.clone()).await, LocalCompositionTerminateResult::Joined(LocalCompositionWaitOutcome::Live(LocalCompositionResponse::Terminated { cell_id, .. })) if cell_id == id)
    );
    assert!(matches!(
        responder
            .resolve(serde_json::json!("late"))
            .await
            .expect("non-delivery"),
        LocalCompositionNestedDelivery::NotObserved(_)
    ));
    assert!(session.generation_lost());
    let _ = session.shutdown().await;
}

fn dictionary_tool(name: &str) -> PythonToolBinding {
    PythonToolBinding {
        tool_name: name.into(),
        input_kind: BindingInput::Dictionary,
    }
}

async fn run_source(
    worker: &Path,
    bindings: BTreeMap<String, PythonToolBinding>,
    source: &str,
) -> LocalCompositionResponse {
    let bundle = python_session(worker, bindings)
        .await
        .expect("monty session");
    let session = bundle.session;
    let started = session
        .execute(LocalCompositionExecuteRequest {
            start_admission: plakous_agent::internal::LocalCompositionStartAdmission::new(|| {
                Box::pin(async { Ok(()) })
            }),
            tool_call_id: "c1".into(),
            enabled_tools: Vec::new(),
            source: source.into(),
            yield_time_ms: 90_000,
            max_output_tokens: 10_000,
        })
        .await
        .expect("execute");
    let response = (started.initial_response).await.expect("initial response");
    let _ = session.shutdown().await;
    response
}

enum NestedPlan {
    Resolve(serde_json::Value),
    Reject(String),
}

async fn dispatch_nested_calls(
    events: &mut Box<dyn LocalCompositionEventReceiver>,
    plan: impl IntoIterator<Item = (String, NestedPlan)>,
) {
    let mut deliveries = Vec::new();
    for (tool_name, outcome) in plan {
        let event = events
            .receive()
            .await
            .expect("events channel")
            .expect("nested call event");
        let LocalCompositionEvent::NestedCall { call, responder } = event else {
            panic!("expected nested call");
        };
        assert_eq!(call.tool_name, tool_name);
        let delivery = match outcome {
            NestedPlan::Resolve(value) => responder.resolve(value),
            NestedPlan::Reject(reason) => responder.reject(reason),
        };
        deliveries.push(tokio::spawn(delivery));
    }
    join_deliveries(deliveries).await;
}

async fn dispatch_nested_calls_out_of_order(
    events: &mut Box<dyn LocalCompositionEventReceiver>,
    expected: usize,
    resolve_order: &[(&str, NestedPlan)],
) {
    let mut pending = BTreeMap::<String, Box<dyn LocalCompositionNestedResponder>>::new();
    while pending.len() < expected {
        let event = events
            .receive()
            .await
            .expect("events channel")
            .expect("nested call event");
        let LocalCompositionEvent::NestedCall { call, responder } = event else {
            panic!("expected nested call");
        };
        pending.insert(call.tool_name, responder);
    }
    let mut deliveries = Vec::new();
    for (tool_name, outcome) in resolve_order {
        let responder = pending
            .remove(*tool_name)
            .unwrap_or_else(|| panic!("missing nested call for {tool_name}"));
        let delivery = match outcome {
            NestedPlan::Resolve(value) => responder.resolve(value.clone()),
            NestedPlan::Reject(reason) => responder.reject(reason.clone()),
        };
        deliveries.push(tokio::spawn(delivery));
    }
    assert!(pending.is_empty(), "unresolved nested calls remain");
    join_deliveries(deliveries).await;
}

async fn dispatch_same_tool_parallel(
    events: &mut Box<dyn LocalCompositionEventReceiver>,
    tool_name: &str,
    expected: usize,
    resolve_order: &[NestedPlan],
) {
    let mut pending = BTreeMap::<String, Box<dyn LocalCompositionNestedResponder>>::new();
    while pending.len() < expected {
        let event = events
            .receive()
            .await
            .expect("events channel")
            .expect("nested call event");
        let LocalCompositionEvent::NestedCall { call, responder } = event else {
            panic!("expected nested call");
        };
        assert_eq!(call.tool_name, tool_name);
        assert!(
            !pending.contains_key(&call.runtime_call_id),
            "duplicate runtime_call_id {}",
            call.runtime_call_id
        );
        pending.insert(call.runtime_call_id, responder);
    }
    let mut deliveries = Vec::new();
    for outcome in resolve_order {
        let (runtime_call_id, responder) = pending.pop_first().expect("pending nested call");
        let delivery = match outcome {
            NestedPlan::Resolve(value) => responder.resolve(value.clone()),
            NestedPlan::Reject(reason) => responder.reject(reason.clone()),
        };
        deliveries.push(tokio::spawn(delivery));
        let _ = runtime_call_id;
    }
    assert!(pending.is_empty(), "unresolved nested calls remain");
    join_deliveries(deliveries).await;
}

async fn join_deliveries(
    deliveries: Vec<tokio::task::JoinHandle<Result<LocalCompositionNestedDelivery, String>>>,
) {
    for delivery in deliveries {
        let delivery = tokio::time::timeout(Duration::from_secs(10), delivery)
            .await
            .expect("batch delivery must not deadlock")
            .expect("delivery task")
            .expect("nested delivery");
        assert_eq!(delivery, LocalCompositionNestedDelivery::Accepted);
    }
}

#[tokio::test]
async fn nested_call_arrives_before_initial_response() {
    let Some(worker) = py_host() else {
        eprintln!(
            "skipping nested_call_arrives_before_initial_response: Python worker not available (set PLAK_PY_HOST)"
        );
        return;
    };
    let mut bindings = BTreeMap::new();
    bindings.insert("fetch".into(), dictionary_tool("echo_tool"));
    let bundle = python_session(&worker, bindings)
        .await
        .expect("monty session");
    let session = bundle.session;
    let mut events = bundle.events;
    let started = session
        .execute(LocalCompositionExecuteRequest {
            start_admission: plakous_agent::internal::LocalCompositionStartAdmission::new(|| {
                Box::pin(async { Ok(()) })
            }),
            tool_call_id: "c1".into(),
            enabled_tools: Vec::new(),
            source: "await fetch({'q': 'ping'})".into(),
            yield_time_ms: 90_000,
            max_output_tokens: 10_000,
        })
        .await
        .expect("execute");
    let initial = started.initial_response;
    tokio::pin!(initial);
    let event = tokio::time::timeout(Duration::from_secs(30), events.receive())
        .await
        .expect("timed out waiting for nested call")
        .expect("events channel")
        .expect("nested call event");
    let LocalCompositionEvent::NestedCall { call, responder } = event else {
        panic!("expected nested call");
    };
    assert_eq!(call.tool_name, "echo_tool");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut initial)
            .await
            .is_err(),
        "initial response resolved before nested reply"
    );
    let delivery = responder
        .resolve(serde_json::json!("pong"))
        .await
        .expect("resolve nested call");
    assert_eq!(delivery, LocalCompositionNestedDelivery::Accepted);
    let response = initial.await.expect("initial response");
    match response {
        LocalCompositionResponse::Result { items, error, .. } => {
            assert!(error.is_none(), "{error:?}");
            assert!(
                items.iter().any(|item| matches!(
                    item,
                    LocalCompositionContentItem::Text(text) if text.contains("pong")
                )),
                "{items:?}"
            );
        }
        other => panic!("unexpected response {other:?}"),
    }
    let _ = session.shutdown().await;
}

#[tokio::test]
async fn async_tool_call_returns_reply_text() {
    let Some(worker) = py_host() else {
        eprintln!(
            "skipping async_tool_call_returns_reply_text: Python worker not available (set PLAK_PY_HOST)"
        );
        return;
    };
    let mut bindings = BTreeMap::new();
    bindings.insert("fetch".into(), dictionary_tool("echo_tool"));
    let bundle = python_session(&worker, bindings)
        .await
        .expect("monty session");
    let session = bundle.session;
    let mut events = bundle.events;
    let started = session
        .execute(LocalCompositionExecuteRequest {
            start_admission: plakous_agent::internal::LocalCompositionStartAdmission::new(|| {
                Box::pin(async { Ok(()) })
            }),
            tool_call_id: "c1".into(),
            enabled_tools: Vec::new(),
            source: "await fetch({'q': 'ping'})".into(),
            yield_time_ms: 90_000,
            max_output_tokens: 10_000,
        })
        .await
        .expect("execute");
    dispatch_nested_calls(
        &mut events,
        [(
            "echo_tool".into(),
            NestedPlan::Resolve(serde_json::json!("pong")),
        )],
    )
    .await;
    let response = (started.initial_response).await.expect("initial response");
    let _ = session.shutdown().await;
    match response {
        LocalCompositionResponse::Result { items, error, .. } => {
            assert!(error.is_none(), "{error:?}");
            assert!(
                items.iter().any(|item| matches!(
                    item,
                    LocalCompositionContentItem::Text(text) if text.contains("pong")
                )),
                "{items:?}"
            );
        }
        other => panic!("unexpected response {other:?}"),
    }
}

#[tokio::test]
async fn asyncio_gather_resumes_in_program_order() {
    let Some(worker) = py_host() else {
        eprintln!(
            "skipping asyncio_gather_resumes_in_program_order: Python worker not available (set PLAK_PY_HOST)"
        );
        return;
    };
    let mut bindings = BTreeMap::new();
    bindings.insert("a".into(), dictionary_tool("tool_a"));
    bindings.insert("b".into(), dictionary_tool("tool_b"));
    let bundle = python_session(&worker, bindings)
        .await
        .expect("monty session");
    let session = bundle.session;
    let mut events = bundle.events;
    let started = session
        .execute(LocalCompositionExecuteRequest {
            start_admission: plakous_agent::internal::LocalCompositionStartAdmission::new(|| {
                Box::pin(async { Ok(()) })
            }),
            tool_call_id: "c1".into(),
            enabled_tools: Vec::new(),
            source: "import asyncio\nawait asyncio.gather(a({'x': 1}), b({'y': 2}))".into(),
            yield_time_ms: 90_000,
            max_output_tokens: 10_000,
        })
        .await
        .expect("execute");
    dispatch_nested_calls_out_of_order(
        &mut events,
        2,
        &[
            ("tool_b", NestedPlan::Resolve(serde_json::json!("rb"))),
            ("tool_a", NestedPlan::Resolve(serde_json::json!("ra"))),
        ],
    )
    .await;
    let response = (started.initial_response).await.expect("initial response");
    let _ = session.shutdown().await;
    match response {
        LocalCompositionResponse::Result { items, error, .. } => {
            assert!(error.is_none(), "{error:?}");
            let text = items
                .iter()
                .filter_map(|item| match item {
                    LocalCompositionContentItem::Text(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            assert!(text.contains("ra") && text.contains("rb"), "{text:?}");
            let first_pos = text.find("ra").expect("ra");
            let second_pos = text.find("rb").expect("rb");
            assert!(
                first_pos < second_pos,
                "expected program order [ra, rb], got {text:?}"
            );
        }
        other => panic!("unexpected response {other:?}"),
    }
}

#[tokio::test]
async fn asyncio_gather_same_tool_uses_distinct_runtime_call_ids() {
    let Some(worker) = py_host() else {
        eprintln!(
            "skipping asyncio_gather_same_tool_uses_distinct_runtime_call_ids: Python worker not available (set PLAK_PY_HOST)"
        );
        return;
    };
    let mut bindings = BTreeMap::new();
    bindings.insert("Read".into(), dictionary_tool("read_tool"));
    let bundle = python_session(&worker, bindings)
        .await
        .expect("monty session");
    let session = bundle.session;
    let mut events = bundle.events;
    let started = session
        .execute(LocalCompositionExecuteRequest {
            start_admission: plakous_agent::internal::LocalCompositionStartAdmission::new(|| {
                Box::pin(async { Ok(()) })
            }),
            tool_call_id: "c1".into(),
            enabled_tools: Vec::new(),
            source: "import asyncio\nawait asyncio.gather(Read({'p': 'a'}), Read({'p': 'b'}))"
                .into(),
            yield_time_ms: 90_000,
            max_output_tokens: 10_000,
        })
        .await
        .expect("execute");
    dispatch_same_tool_parallel(
        &mut events,
        "read_tool",
        2,
        &[
            NestedPlan::Resolve(serde_json::json!("ra")),
            NestedPlan::Resolve(serde_json::json!("rb")),
        ],
    )
    .await;
    let response = (started.initial_response).await.expect("initial response");
    let _ = session.shutdown().await;
    match response {
        LocalCompositionResponse::Result { items, error, .. } => {
            assert!(error.is_none(), "{error:?}");
            let text = items
                .iter()
                .filter_map(|item| match item {
                    LocalCompositionContentItem::Text(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            assert!(text.contains("ra") && text.contains("rb"), "{text:?}");
        }
        other => panic!("unexpected response {other:?}"),
    }
}

#[tokio::test]
async fn failing_nested_call_is_catchable_in_python() {
    let Some(worker) = py_host() else {
        eprintln!(
            "skipping failing_nested_call_is_catchable_in_python: Python worker not available (set PLAK_PY_HOST)"
        );
        return;
    };
    let mut bindings = BTreeMap::new();
    bindings.insert("bad".into(), dictionary_tool("bad_tool"));
    let bundle = python_session(&worker, bindings)
        .await
        .expect("monty session");
    let session = bundle.session;
    let mut events = bundle.events;
    let started = session
        .execute(LocalCompositionExecuteRequest {
            start_admission: plakous_agent::internal::LocalCompositionStartAdmission::new(|| {
                Box::pin(async { Ok(()) })
            }),
            tool_call_id: "c1".into(),
            enabled_tools: Vec::new(),
            source:
                "try:\n    await bad({})\nexcept Exception as e:\n    print(type(e).__name__, e)"
                    .into(),
            yield_time_ms: 90_000,
            max_output_tokens: 10_000,
        })
        .await
        .expect("execute");
    dispatch_nested_calls(
        &mut events,
        [(
            "bad_tool".into(),
            NestedPlan::Reject("denied by host".into()),
        )],
    )
    .await;
    let response = (started.initial_response).await.expect("initial response");
    let _ = session.shutdown().await;
    match response {
        LocalCompositionResponse::Result { items, error, .. } => {
            assert!(error.is_none(), "{error:?}");
            assert!(
                items.iter().any(|item| matches!(
                    item,
                    LocalCompositionContentItem::Text(text)
                        if text.contains("RuntimeError") && text.contains("denied by host")
                )),
                "{items:?}"
            );
        }
        other => panic!("unexpected response {other:?}"),
    }
}

#[tokio::test]
async fn unbound_tool_name_fails_with_name_error() {
    let Some(worker) = py_host() else {
        eprintln!(
            "skipping unbound_tool_name_fails_with_name_error: Python worker not available (set PLAK_PY_HOST)"
        );
        return;
    };
    let response = run_source(&worker, BTreeMap::new(), "await missing_tool({})").await;
    match response {
        LocalCompositionResponse::Result { items, error, .. } => {
            assert!(error.is_some(), "expected cell error, got success");
            assert!(
                error.as_deref().is_some_and(|e| e.contains("NameError")),
                "expected NameError in error field, got items={items:?} error={error:?}"
            );
            assert!(
                !items.iter().any(|item| matches!(
                    item,
                    LocalCompositionContentItem::Text(text) if text.contains("NameError")
                )),
                "runtime exception must not appear in printed items: {items:?}"
            );
        }
        other => panic!("unexpected response {other:?}"),
    }
}

#[tokio::test]
async fn uncaught_python_exception_surfaces_cell_error() {
    let Some(worker) = py_host() else {
        eprintln!(
            "skipping uncaught_python_exception_surfaces_cell_error: Python worker not available (set PLAK_PY_HOST)"
        );
        return;
    };
    let response = run_source(&worker, BTreeMap::new(), "raise ValueError('boom')").await;
    match response {
        LocalCompositionResponse::Result { items, error, .. } => {
            assert!(
                error.is_some(),
                "expected cell error for uncaught exception"
            );
            assert!(
                error
                    .as_deref()
                    .is_some_and(|text| text.contains("ValueError")),
                "{error:?}"
            );
            assert!(
                !items.iter().any(|item| matches!(
                    item,
                    LocalCompositionContentItem::Text(text) if text.contains("ValueError")
                )),
                "runtime exception must not appear in printed items: {items:?}"
            );
        }
        other => panic!("unexpected response {other:?}"),
    }
}

#[tokio::test]
async fn concurrent_execute_while_cell_running_is_rejected() {
    let Some(worker) = py_host() else {
        return;
    };
    let mut bindings = BTreeMap::new();
    bindings.insert("fetch".into(), dictionary_tool("echo_tool"));
    let bundle = python_session(&worker, bindings)
        .await
        .expect("monty session");
    let session = bundle.session;
    let mut events = bundle.events;
    let started = session
        .execute(LocalCompositionExecuteRequest {
            start_admission: plakous_agent::internal::LocalCompositionStartAdmission::new(|| {
                Box::pin(async { Ok(()) })
            }),
            tool_call_id: "c1".into(),
            enabled_tools: Vec::new(),
            source: "await fetch({'q': 'ping'})".into(),
            yield_time_ms: 90_000,
            max_output_tokens: 10_000,
        })
        .await
        .expect("execute first cell");
    let first_cell_id = started.cell_id.clone();
    let event = tokio::time::timeout(Duration::from_secs(30), events.receive())
        .await
        .expect("timed out waiting for nested call")
        .expect("events channel")
        .expect("nested call event");
    let LocalCompositionEvent::NestedCall { call, responder } = event else {
        panic!("expected nested call");
    };
    assert_eq!(call.cell_id, first_cell_id);
    let second = session
        .execute(LocalCompositionExecuteRequest {
            start_admission: plakous_agent::internal::LocalCompositionStartAdmission::new(|| {
                Box::pin(async { Ok(()) })
            }),
            tool_call_id: "c2".into(),
            enabled_tools: Vec::new(),
            source: "1 + 1".into(),
            yield_time_ms: 90_000,
            max_output_tokens: 10_000,
        })
        .await
        .expect("execute second cell while first is running");
    let refused_cell_id = second.cell_id.clone();
    let refused_response = (second.initial_response)
        .await
        .expect("refused cell initial response");
    match refused_response {
        LocalCompositionResponse::Result { items, error, .. } => {
            assert!(items.is_empty(), "{items:?}");
            assert!(
                error.as_deref().is_some_and(|message| {
                    message.contains("still running") && message.contains(&first_cell_id)
                }),
                "{error:?}"
            );
        }
        other => panic!("unexpected refused cell response {other:?}"),
    }
    let delivery = responder
        .resolve(serde_json::json!("pong"))
        .await
        .expect("resolve nested call");
    assert_eq!(delivery, LocalCompositionNestedDelivery::Accepted);
    match started.initial_response.await.expect("first cell terminal") {
        LocalCompositionResponse::Result { items, error, .. } => {
            assert!(error.is_none(), "{error:?}");
            assert!(items.iter().any(|item| matches!(item,
                LocalCompositionContentItem::Text(text) if text.contains("pong"))));
        }
        other => panic!("expected terminal response: {other:?}"),
    }
    let missing = session
        .wait(LocalCompositionWaitRequest {
            cell_id: first_cell_id,
            yield_time_ms: 90_000,
        })
        .await
        .expect("wait after completion");
    assert!(
        matches!(missing, LocalCompositionWaitOutcome::Missing(_)),
        "stream entry should be removed after terminal wait"
    );
    let refused_wait = session
        .wait(LocalCompositionWaitRequest {
            cell_id: refused_cell_id,
            yield_time_ms: 90_000,
        })
        .await
        .expect("wait refused cell");
    assert!(
        matches!(refused_wait, LocalCompositionWaitOutcome::Missing(_)),
        "refused cell should not register a stream entry"
    );
    let _ = session.shutdown().await;
}

#[tokio::test]
async fn successive_cells_retain_state_without_a_cumulative_duration_budget() {
    let Some(worker) = py_host() else {
        eprintln!(
            "skipping successive_cells_retain_state_without_a_cumulative_duration_budget: Python worker not available (set PLAK_PY_HOST)"
        );
        return;
    };
    tokio::time::timeout(Duration::from_secs(30), async {
        let bundle = python_session(&worker, BTreeMap::new())
            .await
            .expect("monty session");
        let session = bundle.session;
        for index in 0..20 {
            let started = session
                .execute(LocalCompositionExecuteRequest {
                    start_admission: plakous_agent::internal::LocalCompositionStartAdmission::new(
                        || Box::pin(async { Ok(()) }),
                    ),
                    tool_call_id: format!("cell-{index}"),
                    enabled_tools: Vec::new(),
                    source: if index == 0 {
                        "total = 0\ntotal".into()
                    } else {
                        "total += 1\ntotal".into()
                    },
                    yield_time_ms: 90_000,
                    max_output_tokens: 10_000,
                })
                .await
                .expect("execute");
            let LocalCompositionResponse::Result { items, error, .. } =
                started.initial_response.await.expect("initial response")
            else {
                panic!("expected result response");
            };
            assert!(error.is_none(), "{error:?}");
            assert!(matches!(
                items.as_slice(),
                [LocalCompositionContentItem::Text(text)] if text == &index.to_string()
            ));
        }
        assert!(matches!(
            session.shutdown().await,
            LocalCompositionShutdownResult::Joined { .. }
        ));
    })
    .await
    .expect("successive cells must complete within the scenario bound");
}

/// A request bound short enough to observe the kill-and-reap path without
/// waiting out the production [`PYTHON_WORKER_REQUEST_BOUND`]. Only the bound
/// differs; the session is built by the production code path.
const TEST_WORKER_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

fn blocking_cell(tool_call_id: &str, source: &str) -> LocalCompositionExecuteRequest {
    LocalCompositionExecuteRequest {
        start_admission: plakous_agent::internal::LocalCompositionStartAdmission::new(|| {
            Box::pin(async { Ok(()) })
        }),
        tool_call_id: tool_call_id.into(),
        enabled_tools: Vec::new(),
        source: source.into(),
        yield_time_ms: 90_000,
        max_output_tokens: 10_000,
    }
}

#[tokio::test]
async fn a_stuck_worker_request_is_killed_and_reported_as_session_loss() {
    // A cell the worker never answers must not hold the parent open: the
    // session kills the worker at its request bound and the cell reports a session
    // loss, which is a strictly different outcome from a Python exception.
    let Some(worker) = py_host() else {
        eprintln!(
            "skipping a_stuck_worker_request_is_killed_and_reported_as_session_loss: Python worker not available (set PLAK_PY_HOST)"
        );
        return;
    };
    tokio::time::timeout(Duration::from_secs(30), async {
        let bundle = python_session_within(&worker, BTreeMap::new(), TEST_WORKER_REQUEST_TIMEOUT)
            .await
            .expect("monty session");
        let session = bundle.session;

        // Pin the contrasting classification on the same live session: a raised
        // Python exception is a cell result carrying an error, never a loss.
        let raised = session
            .execute(blocking_cell("raises", "raise ValueError('not a loss')"))
            .await
            .expect("execute raising cell")
            .initial_response
            .await
            .expect("a Python exception is not a session loss");
        let LocalCompositionResponse::Result { error, .. } = raised else {
            panic!("expected a result response for a raising cell");
        };
        assert!(
            error.is_some_and(|error| error.contains("not a loss")),
            "a raised exception must surface as the cell's error"
        );

        let loss = session
            .execute(blocking_cell("stuck", "while True:\n    pass\n"))
            .await
            .expect("execute stuck cell")
            .initial_response
            .await
            .expect_err("a worker that never answers is a session loss");
        let LocalCompositionSessionLoss::HostLost(detail) = loss else {
            panic!("a stuck worker must be host loss, not a cell-level error");
        };
        assert!(
            detail.to_lowercase().contains("timeout")
                || detail.to_lowercase().contains("timed out"),
            "loss detail should name the expired request bound: {detail}"
        );

        // The request bound killed the worker; that kill is a sticky loss, so
        // shutdown stays fail-closed on a session that was already lost
        // rather than certifying a clean join.
        assert!(
            matches!(
                session.shutdown().await,
                LocalCompositionShutdownResult::Lost(_)
            ),
            "a session lost to a request timeout must not certify a joined shutdown"
        );
    })
    .await
    .expect("the stuck-worker scenario must finish inside its own bound");
}

#[tokio::test]
async fn a_stuck_worker_request_after_nested_reply_is_killed() {
    let Some(worker) = py_host() else { return };
    let bundle = python_session_within(
        &worker,
        BTreeMap::from([("fetch".into(), dictionary_tool("fetch"))]),
        TEST_WORKER_REQUEST_TIMEOUT,
    )
    .await
    .expect("session");
    let mut events = bundle.events;
    let session = bundle.session;
    let started = session
        .execute(blocking_cell(
            "stuck-after-reply",
            "await fetch({})\nwhile True:\n    pass",
        ))
        .await
        .expect("start");
    let LocalCompositionEvent::NestedCall { responder, .. } =
        events.receive().await.expect("events").expect("call")
    else {
        panic!("nested call");
    };
    let delivery = tokio::spawn(responder.resolve(serde_json::json!(7)));
    let loss = tokio::time::timeout(Duration::from_secs(15), started.initial_response)
        .await
        .expect("compute watchdog")
        .expect_err("no cell result");
    assert!(
        matches!(loss, LocalCompositionSessionLoss::HostLost(ref detail) if detail.contains("timed out")),
        "{loss:?}"
    );
    assert_eq!(
        delivery.await.expect("delivery").expect("outcome"),
        LocalCompositionNestedDelivery::HostLost,
        "computation without an interpreter event provides no receipt"
    );
    assert!(matches!(
        session.shutdown().await,
        LocalCompositionShutdownResult::Lost(_)
    ));
}

#[tokio::test]
async fn allocator_exit_after_nested_result_is_not_acceptance() {
    let Some(worker) = py_host() else { return };
    let bundle = python_session(
        &worker,
        BTreeMap::from([("fetch".into(), dictionary_tool("fetch"))]),
    )
    .await
    .expect("session");
    let mut events = bundle.events;
    let session = bundle.session;
    // The value fits the interpreter's soft budget. Exporting it after the
    // reply needs a second copy and crosses the allocator's hard ceiling.
    let started = session
        .execute(blocking_cell(
            "hard-ceiling",
            "x = 'a' * (240 * 1024 * 1024)\nawait fetch({})\nx",
        ))
        .await
        .expect("start");
    let LocalCompositionEvent::NestedCall { responder, .. } =
        tokio::time::timeout(Duration::from_secs(15), events.receive())
            .await
            .expect("call bound")
            .expect("events")
            .expect("call")
    else {
        panic!("nested call");
    };
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(15),
            responder.resolve(serde_json::json!(7))
        )
        .await
        .expect("delivery bound")
        .expect("outcome"),
        LocalCompositionNestedDelivery::HostLost
    );
    assert!(matches!(
        started.initial_response.await,
        Err(LocalCompositionSessionLoss::HostLost(_))
    ));
    let _ = session.shutdown().await;
}

#[tokio::test]
async fn terminal_cells_retain_state_without_retaining_response_streams() {
    let Some(worker) = py_host() else { return };
    let bundle = python_session(&worker, BTreeMap::new())
        .await
        .expect("session");
    for n in 0..32 {
        let started = bundle
            .session
            .execute(LocalCompositionExecuteRequest {
                start_admission: plakous_agent::internal::LocalCompositionStartAdmission::new(
                    || Box::pin(async { Ok(()) }),
                ),
                tool_call_id: format!("cell-{n}"),
                enabled_tools: vec![],
                source: if n == 0 {
                    "counter = 1\ncounter".into()
                } else {
                    "counter += 1\ncounter".into()
                },
                yield_time_ms: 90_000,
                max_output_tokens: 100,
            })
            .await
            .expect("execute");
        let cell_id = started.cell_id;
        let response = started.initial_response.await.expect("terminal");
        assert!(matches!(
            response,
            LocalCompositionResponse::Result { error: None, .. }
        ));
        assert!(matches!(
            bundle
                .session
                .wait(LocalCompositionWaitRequest {
                    cell_id,
                    yield_time_ms: 1
                })
                .await
                .expect("wait"),
            LocalCompositionWaitOutcome::Missing(_)
        ));
    }
    assert!(matches!(
        bundle.session.shutdown().await,
        LocalCompositionShutdownResult::Joined { .. }
    ));
}

async fn assert_real_worker_future_receipts(
    source: &str,
    expected_inputs: &[serde_json::Value],
    expected_output: &str,
) {
    let Some(worker) = py_host() else {
        eprintln!("skipping future receipt regression: set PLAK_PY_HOST");
        return;
    };
    let bundle = python_session(
        &worker,
        BTreeMap::from([("fetch".into(), dictionary_tool("fetch_tool"))]),
    )
    .await
    .expect("monty session");
    let session = bundle.session;
    let mut events = bundle.events;
    let outcome = tokio::time::timeout(Duration::from_secs(30), async {
        let started = session
            .execute(lazy_tool_request(source))
            .await
            .expect("execute");
        let mut calls = Vec::new();
        let mut replies = tokio::task::JoinSet::new();
        for index in 0..expected_inputs.len() {
            let event = events
                .receive()
                .await
                .expect("event channel")
                .expect("nested call");
            let LocalCompositionEvent::NestedCall { call, responder } = event else {
                panic!("expected nested call");
            };
            calls.push(call);
            replies.spawn(async move {
                (
                    index,
                    responder.resolve(serde_json::json!("consumed reply")).await,
                )
            });
        }
        // Poll every receipt together so a batch never waits for its first
        // receipt before delivering the remaining results.
        let (deliveries, terminal) = tokio::join!(
            async {
                let mut deliveries = Vec::new();
                while let Some(delivery) = replies.join_next().await {
                    deliveries.push(delivery.expect("delivery task"));
                }
                deliveries.sort_by_key(|(index, _)| *index);
                deliveries
                    .into_iter()
                    .map(|(_, delivery)| delivery)
                    .collect::<Vec<_>>()
            },
            started.initial_response
        );
        (calls, deliveries, terminal)
    })
    .await;
    let shutdown = session.shutdown().await;
    drop(session);
    assert!(
        events
            .receive()
            .await
            .expect("closed event channel")
            .is_none(),
        "unexpected nested dispatch"
    );
    assert!(matches!(
        shutdown,
        LocalCompositionShutdownResult::Joined { .. }
    ));
    let (calls, deliveries, terminal) =
        outcome.expect("future receipts must settle without deadlock");
    let mut call_ids = std::collections::BTreeSet::new();
    assert_eq!(calls.len(), expected_inputs.len());
    for (call, expected_input) in calls.into_iter().zip(expected_inputs) {
        assert_eq!(call.input.as_ref(), Some(expected_input));
        assert_eq!(call.tool_name, "fetch_tool");
        assert!(
            call_ids.insert(call.runtime_call_id),
            "distinct runtime calls"
        );
    }
    for delivery in deliveries {
        assert_eq!(
            delivery.expect("delivery outcome"),
            LocalCompositionNestedDelivery::Accepted
        );
    }
    let LocalCompositionResponse::Result { items, error, .. } = terminal.expect("cell terminal")
    else {
        panic!("expected terminal result");
    };
    assert!(error.is_none(), "{error:?}");
    assert!(
        items.iter().any(|item| matches!(
            item,
            LocalCompositionContentItem::Text(text) if text.contains(expected_output)
        )),
        "{items:?}"
    );
}

#[tokio::test]
async fn unawaited_bound_tool_never_dispatches() {
    assert_real_worker_future_receipts(
        "print(fetch({}))\nprint('cell finished without awaiting')",
        &[],
        "cell finished without awaiting",
    )
    .await;
}

#[tokio::test]
async fn awaited_bound_tool_accepts_consumed_result() {
    assert_real_worker_future_receipts(
        "print(await fetch({}))",
        &[serde_json::json!({})],
        "consumed reply",
    )
    .await;
}

#[tokio::test]
async fn mixed_unused_and_awaited_bound_tools_dispatch_only_the_awaited_call() {
    assert_real_worker_future_receipts(
        "orphan = fetch({'kind': 'orphan'})\nprint(await fetch({'kind': 'awaited'}))",
        &[serde_json::json!({"kind": "awaited"})],
        "consumed reply",
    )
    .await;
}

#[tokio::test]
async fn undecodable_nested_result_never_acknowledges_delivery() {
    let Some(worker) = py_host() else {
        return;
    };
    let bundle = python_session(
        &worker,
        BTreeMap::from([("fetch".into(), dictionary_tool("fetch_tool"))]),
    )
    .await
    .expect("session");
    let mut events = bundle.events;
    let session = bundle.session;
    let started = session
        .execute(LocalCompositionExecuteRequest {
            start_admission: plakous_agent::internal::LocalCompositionStartAdmission::new(|| {
                Box::pin(async { Ok(()) })
            }),
            tool_call_id: "undecodable-nested".into(),
            enabled_tools: Vec::new(),
            source: "await fetch({})".into(),
            yield_time_ms: 90_000,
            max_output_tokens: 10_000,
        })
        .await
        .expect("execute");
    let event = events
        .receive()
        .await
        .expect("event channel")
        .expect("nested call");
    let LocalCompositionEvent::NestedCall { responder, .. } = event else {
        panic!("expected nested call");
    };
    // Above i64::MAX: the host cannot encode it, so the batch is never sent.
    let value = serde_json::json!(u64::MAX);
    assert!(
        matches!(
            responder.resolve(value).await.expect("delivery outcome"),
            LocalCompositionNestedDelivery::NotObserved(_)
        ),
        "a batch this host refused to send never reached the cell, which is \
         definite non-delivery rather than an unknown acceptance"
    );
    assert!(matches!(
        started.initial_response.await,
        Err(LocalCompositionSessionLoss::ProtocolViolation(_))
    ));
    assert!(matches!(
        session.shutdown().await,
        LocalCompositionShutdownResult::Joined { .. }
    ));
}

#[tokio::test]
async fn uncaught_nested_rejection_is_accepted_and_preserves_python_exception() {
    let Some(worker) = py_host() else {
        return;
    };
    let bundle = python_session(
        &worker,
        BTreeMap::from([("bad".into(), dictionary_tool("bad_tool"))]),
    )
    .await
    .expect("session");
    let mut events = bundle.events;
    let session = bundle.session;
    let started = session
        .execute(LocalCompositionExecuteRequest {
            start_admission: plakous_agent::internal::LocalCompositionStartAdmission::new(|| {
                Box::pin(async { Ok(()) })
            }),
            tool_call_id: "uncaught-nested".into(),
            enabled_tools: Vec::new(),
            source: "await bad({\n    'patch': '''not a diff\nat all'''\n})".into(),
            yield_time_ms: 90_000,
            max_output_tokens: 10_000,
        })
        .await
        .expect("execute");
    dispatch_nested_calls(
        &mut events,
        [(
            "bad_tool".into(),
            NestedPlan::Reject("exact nested failure".into()),
        )],
    )
    .await;
    let response = started
        .initial_response
        .await
        .expect("definite Python cell failure");
    assert!(
        matches!(response, LocalCompositionResponse::Result { error: Some(ref message), .. }
        if message.contains("RuntimeError") && message.contains("exact nested failure"))
    );
    assert!(matches!(
        session.shutdown().await,
        LocalCompositionShutdownResult::Joined { .. }
    ));
}

fn lazy_tool_request(source: impl Into<String>) -> LocalCompositionExecuteRequest {
    LocalCompositionExecuteRequest {
        start_admission: plakous_agent::internal::LocalCompositionStartAdmission::new(|| {
            Box::pin(async { Ok(()) })
        }),
        tool_call_id: "lazy-tool-cell".into(),
        enabled_tools: Vec::new(),
        source: source.into(),
        yield_time_ms: 90_000,
        max_output_tokens: 10_000,
    }
}

#[tokio::test]
async fn lazy_tool_aliases_user_functions_and_state_persist_across_cells() {
    let Some(worker) = py_host() else { return };
    let bundle = python_session(
        &worker,
        BTreeMap::from([("fetch".into(), dictionary_tool("fetch_tool"))]),
    )
    .await
    .expect("session");
    let session = bundle.session;
    let mut events = bundle.events;
    let outcome = tokio::time::timeout(Duration::from_secs(30), async {
        let first = session.execute(lazy_tool_request(
            "alias = fetch\nvalues = []\nasync def collect(value):\n    answer = await alias({'value': value})\n    values.append(answer)\n    return answer",
        )).await.expect("define persistent state");
        assert!(matches!(first.initial_response.await.expect("definition terminal"),
            LocalCompositionResponse::Result { error: None, .. }));
        for value in [1, 2] {
            let started = session.execute(lazy_tool_request(format!(
                "print(await collect({value}))\nprint(values)"
            ))).await.expect("execute retained definition");
            let event = events.receive().await.expect("events").expect("nested call");
            let LocalCompositionEvent::NestedCall { call, responder } = event else {
                panic!("expected nested call");
            };
            assert_eq!(call.tool_name, "fetch_tool");
            assert_eq!(call.input, Some(serde_json::json!({"value": value})));
            assert_eq!(responder.resolve(serde_json::json!(format!("reply-{value}"))).await.expect("receipt"),
                LocalCompositionNestedDelivery::Accepted);
            let LocalCompositionResponse::Result { items, error, .. } = started.initial_response.await.expect("terminal") else {
                panic!("expected result");
            };
            assert!(error.is_none(), "{error:?}");
            let text = items.iter().filter_map(|item| match item {
                LocalCompositionContentItem::Text(text) => Some(text.as_str()),
                _ => None,
            }).collect::<Vec<_>>().join("");
            assert!(text.contains("reply-1"), "{text}");
            if value == 2 { assert!(text.contains("reply-2"), "{text}"); }
        }
    }).await;
    assert!(matches!(
        session.shutdown().await,
        LocalCompositionShutdownResult::Joined { .. }
    ));
    drop(session);
    assert!(events.receive().await.expect("closed events").is_none());
    outcome.expect("persistent wrappers must complete");
}

#[tokio::test]
async fn malformed_public_tool_names_cannot_enter_worker_bootstrap() {
    let Some(worker) = py_host() else { return };
    for name in [
        "",
        "await",
        "has-dash",
        "fetch():\n    print('injected')\n#",
        "_plakous_host_0",
    ] {
        assert!(
            python_session(
                &worker,
                BTreeMap::from([(name.into(), dictionary_tool("fetch_tool"))])
            )
            .await
            .is_err(),
            "invalid export {name:?} must be refused"
        );
    }
}

#[tokio::test]
async fn cancellation_joins_all_unfinished_awaited_tool_calls() {
    let Some(worker) = py_host() else { return };
    let bundle = python_session(
        &worker,
        BTreeMap::from([("fetch".into(), dictionary_tool("fetch_tool"))]),
    )
    .await
    .expect("session");
    let session = bundle.session;
    let mut events = bundle.events;
    let started = session
        .execute(lazy_tool_request(format!(
            "import asyncio\nawait asyncio.gather(*[fetch({{'index': n}}) for n in range({MAX_OUTSTANDING_NESTED})])"
        )))
        .await
        .expect("execute bounded gather");
    let mut responders = Vec::new();
    let received = tokio::time::timeout(Duration::from_secs(30), async {
        for index in 0..MAX_OUTSTANDING_NESTED {
            let event = events
                .receive()
                .await
                .expect("events")
                .expect("nested call");
            let LocalCompositionEvent::NestedCall { call, responder } = event else {
                panic!("expected nested call");
            };
            assert_eq!(call.input, Some(serde_json::json!({"index": index})));
            responders.push(responder);
        }
    })
    .await;
    let terminated = session.terminate(started.cell_id).await;
    let shutdown = session.shutdown().await;
    received.expect("all bounded concurrent calls must be emitted before any reply");
    assert!(matches!(
        terminated,
        LocalCompositionTerminateResult::Joined(_)
    ));
    assert!(matches!(
        shutdown,
        LocalCompositionShutdownResult::Joined { .. }
    ));
    assert!(
        started.initial_response.await.is_err(),
        "cancelled feed has no successful terminal"
    );
    // The cell was terminated before any of these was answered, so the host can
    // prove none of them was observed. That is a definite non-delivery, and the
    // cancellation the turn already knows about stays its own outcome.
    for responder in responders {
        let delivery = responder
            .resolve(serde_json::json!("too late"))
            .await
            .expect("settled receipt");
        assert!(
            matches!(delivery, LocalCompositionNestedDelivery::NotObserved(_)),
            "{delivery:?}"
        );
    }
    drop(session);
    assert!(events.receive().await.expect("closed events").is_none());
}

#[tokio::test]
async fn lazy_tool_invalid_arguments_are_catchable_without_dispatch() {
    assert_real_worker_future_receipts(
        "errors = 0\nfor value in [None, [], 1, 'bad']:\n    try:\n        await fetch(value)\n    except TypeError:\n        errors += 1\ntry:\n    await fetch({}, {})\nexcept TypeError:\n    errors += 1\nassert errors == 5\nprint(await fetch(arguments={}))",
        &[serde_json::json!({})],
        "consumed reply",
    )
    .await;
}

#[tokio::test]
async fn unused_tool_coroutines_do_not_consume_nested_call_capacity() {
    assert_real_worker_future_receipts(
        "unused = [fetch({'index': index}) for index in range(100)]\nprint(await fetch({'kind': 'awaited'}))",
        &[serde_json::json!({"kind": "awaited"})],
        "consumed reply",
    )
    .await;
}

#[tokio::test]
async fn gathered_tool_coroutines_refuse_dispatch_beyond_the_outstanding_limit() {
    let Some(worker) = py_host() else { return };
    let bundle = python_session(
        &worker,
        BTreeMap::from([("fetch".into(), dictionary_tool("fetch_tool"))]),
    )
    .await
    .expect("session");
    let session = bundle.session;
    let mut events = bundle.events;
    let started = session
        .execute(lazy_tool_request(format!(
            "import asyncio\nawait asyncio.gather(*[fetch({{'index': n}}) for n in range({})])",
            MAX_OUTSTANDING_NESTED + 1
        )))
        .await
        .expect("execute oversized gather");
    let outcome = tokio::time::timeout(Duration::from_secs(30), started.initial_response).await;
    assert!(matches!(
        session.shutdown().await,
        LocalCompositionShutdownResult::Joined { .. }
    ));
    drop(session);
    let mut dispatched = 0;
    while let Some(event) = events.receive().await.expect("events") {
        let LocalCompositionEvent::NestedCall { call, responder } = event else {
            panic!("expected nested call");
        };
        assert_eq!(call.input, Some(serde_json::json!({"index": dispatched})));
        dispatched += 1;
        // The cell refused the oversized gather on its own outstanding limit
        // and settled before these arrived. Its definite error is what the turn
        // reports, and a late result it provably never saw must not turn that
        // known outcome into an unknown one.
        let delivery = responder
            .resolve(serde_json::json!("too late"))
            .await
            .expect("lost receipt");
        assert!(
            matches!(delivery, LocalCompositionNestedDelivery::NotObserved(_)),
            "{delivery:?}"
        );
    }
    assert_eq!(dispatched, MAX_OUTSTANDING_NESTED);
    let LocalCompositionResponse::Result { error, .. } = outcome
        .expect("limit refusal must settle without replies")
        .expect("cell terminal")
    else {
        panic!("expected cell error");
    };
    assert!(
        error
            .as_deref()
            .is_some_and(|error| error.contains("outstanding")),
        "{error:?}"
    );
}

#[tokio::test]
async fn freeform_tool_preserves_string_input_and_rejects_dictionary_without_dispatch() {
    let Some(worker) = py_host() else { return };
    let bundle = python_session(
        &worker,
        BTreeMap::from([(
            "patch".into(),
            PythonToolBinding {
                tool_name: "patch_tool".into(),
                input_kind: BindingInput::String,
            },
        )]),
    )
    .await
    .expect("session");
    let session = bundle.session;
    let mut events = bundle.events;
    let outcome = tokio::time::timeout(Duration::from_secs(30), async {
        let started = session.execute(lazy_tool_request(
            "try:\n    await patch({})\nexcept TypeError:\n    pass\nelse:\n    raise AssertionError('dictionary accepted')\nprint(await patch(arguments='line 1\\nline 2'))"
        )).await.expect("execute string tool");
        let LocalCompositionEvent::NestedCall { call, responder } = events.receive().await.expect("events").expect("string call") else {
            panic!("expected nested call");
        };
        assert_eq!(call.tool_name, "patch_tool");
        assert_eq!(call.input, Some(serde_json::json!("line 1\nline 2")));
        assert_eq!(responder.resolve(serde_json::json!("applied")).await.expect("receipt"), LocalCompositionNestedDelivery::Accepted);
        started.initial_response.await.expect("terminal")
    }).await;
    assert!(matches!(
        session.shutdown().await,
        LocalCompositionShutdownResult::Joined { .. }
    ));
    drop(session);
    assert!(events.receive().await.expect("closed events").is_none());
    let LocalCompositionResponse::Result { items, error, .. } =
        outcome.expect("string tool completed")
    else {
        panic!("expected result");
    };
    assert!(error.is_none(), "{error:?}");
    assert!(items.iter().any(
        |item| matches!(item, LocalCompositionContentItem::Text(text) if text.contains("applied"))
    ));
}

#[tokio::test]
async fn cold_tool_coroutine_created_in_one_cell_can_be_awaited_in_the_next() {
    let Some(worker) = py_host() else { return };
    let bundle = python_session(
        &worker,
        BTreeMap::from([("fetch".into(), dictionary_tool("fetch_tool"))]),
    )
    .await
    .expect("session");
    let session = bundle.session;
    let mut events = bundle.events;
    let outcome = tokio::time::timeout(Duration::from_secs(30), async {
        let first = session
            .execute(lazy_tool_request("pending = fetch({'value': 'retained'})"))
            .await
            .expect("create cold coroutine");
        let first_cell = first.cell_id;
        assert!(matches!(
            first.initial_response.await.expect("first terminal"),
            LocalCompositionResponse::Result { error: None, .. }
        ));
        let second = session
            .execute(lazy_tool_request("print(await pending)"))
            .await
            .expect("await retained coroutine");
        let LocalCompositionEvent::NestedCall { call, responder } = events
            .receive()
            .await
            .expect("events")
            .expect("nested call")
        else {
            panic!("expected nested call");
        };
        assert_ne!(call.cell_id, first_cell);
        assert_eq!(call.cell_id, second.cell_id);
        assert_eq!(call.input, Some(serde_json::json!({"value": "retained"})));
        assert_eq!(
            responder
                .resolve(serde_json::json!("retained result"))
                .await
                .expect("receipt"),
            LocalCompositionNestedDelivery::Accepted
        );
        second.initial_response.await.expect("second terminal")
    })
    .await;
    assert!(matches!(
        session.shutdown().await,
        LocalCompositionShutdownResult::Joined { .. }
    ));
    drop(session);
    assert!(events.receive().await.expect("closed events").is_none());
    let LocalCompositionResponse::Result { items, error, .. } =
        outcome.expect("retained coroutine completed")
    else {
        panic!("expected result");
    };
    assert!(error.is_none(), "{error:?}");
    assert!(items.iter().any(|item| matches!(item, LocalCompositionContentItem::Text(text) if text.contains("retained result"))));
}

async fn run_cells(
    worker: &Path,
    bindings: BTreeMap<String, PythonToolBinding>,
    sources: &[&str],
) -> Vec<LocalCompositionResponse> {
    let bundle = python_session(worker, bindings)
        .await
        .expect("monty session");
    let session = bundle.session;
    let mut responses = Vec::new();
    for (index, source) in sources.iter().enumerate() {
        let started = session
            .execute(LocalCompositionExecuteRequest {
                start_admission: plakous_agent::internal::LocalCompositionStartAdmission::new(
                    || Box::pin(async { Ok(()) }),
                ),
                tool_call_id: format!("c{index}"),
                enabled_tools: Vec::new(),
                source: (*source).into(),
                yield_time_ms: 90_000,
                max_output_tokens: 10_000,
            })
            .await
            .expect("execute");
        responses.push(started.initial_response.await.expect("initial response"));
    }
    let _ = session.shutdown().await;
    responses
}

fn cell_outcome(response: &LocalCompositionResponse) -> (String, Option<String>) {
    let LocalCompositionResponse::Result { items, error, .. } = response else {
        panic!("expected a terminal cell result, got {response:?}");
    };
    let text = items
        .iter()
        .filter_map(|item| match item {
            LocalCompositionContentItem::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    (text, error.clone())
}

#[tokio::test]
async fn os_calls_are_refused_and_clock_and_random_are_answered_in_the_worker() {
    let Some(worker) = py_host() else { return };
    let source = r"import os
import time
import random
import asyncio
from datetime import date, datetime
from pathlib import Path

results = []

def probe(label, thunk):
    try:
        thunk()
        results.append(label + ': ok')
    except Exception as error:
        results.append(label + ': ' + type(error).__name__)

probe('open', lambda: open('/etc/hosts').read())
probe('path', lambda: Path('/etc/hosts').read_text())
probe('getenv', lambda: os.getenv('HOME'))
probe('environ', lambda: os.environ)
probe('urandom', lambda: os.urandom(4))
probe('sleep', lambda: time.sleep(0))
probe('isfile', lambda: os.path.isfile('/etc/hosts'))
probe('exists', lambda: os.path.exists('/etc/hosts'))
probe('isdir', lambda: os.path.isdir('/etc'))
probe('islink', lambda: os.path.islink('/etc/hosts'))
probe('lexists', lambda: os.path.lexists('/etc/hosts'))
probe('getsize', lambda: os.path.getsize('/etc/hosts'))
probe('realpath', lambda: os.path.realpath('/etc/hosts'))
probe('expanduser', lambda: os.path.expanduser('~'))
probe('expandvars', lambda: os.path.expandvars('$HOME'))
probe('glob', lambda: list(Path('/etc').glob('*')))
probe('rglob', lambda: list(Path('/etc').rglob('*')))
probe('os.walk', lambda: list(os.walk('/etc')))
probe('Path.walk', lambda: list(Path('/etc').walk()))
try:
    list(Path('/etc').glob('*'))
except RuntimeError as error:
    assert str(error) == 'filesystem access is not available in cells; use the nested tools'
probe('scan', lambda: list(os.scandir('/etc')))
try:
    await asyncio.sleep(0)
    results.append('async_sleep: ok')
except Exception as error:
    results.append('async_sleep: ' + type(error).__name__)
try:
    import plak_host_module
    results.append('host_import: ok')
except Exception as error:
    results.append('host_import: ' + type(error).__name__)
probe('now', lambda: datetime.now())
probe('today', lambda: date.today())
probe('time', lambda: time.time())
probe('random', lambda: random.random())
probe('cwd', lambda: os.getcwd())
'\n'.join(results)";
    let responses = run_cells(&worker, BTreeMap::new(), &[source]).await;
    let (text, error) = cell_outcome(&responses[0]);
    assert_eq!(error, None, "{text}");
    assert_eq!(
        text,
        "open: PermissionError\npath: PermissionError\ngetenv: RuntimeError\n\
         environ: RuntimeError\nurandom: RuntimeError\nsleep: RuntimeError\n\
         isfile: PermissionError\nexists: PermissionError\nisdir: PermissionError\n\
         islink: PermissionError\nlexists: PermissionError\ngetsize: PermissionError\n\
         realpath: PermissionError\nexpanduser: RuntimeError\nexpandvars: RuntimeError\n\
         glob: RuntimeError\nrglob: RuntimeError\nos.walk: RuntimeError\n\
         Path.walk: RuntimeError\nscan: RuntimeError\nasync_sleep: RuntimeError\n\
         host_import: ModuleNotFoundError\nnow: ok\ntoday: ok\ntime: ok\nrandom: ok\ncwd: ok"
    );
}

#[tokio::test]
async fn the_session_outlives_upstreams_default_suspension_budget() {
    let Some(worker) = py_host() else { return };
    let refusals = r"import os
refused = 0
for _ in range(600):
    try:
        os.getenv('HOME')
    except RuntimeError:
        refused += 1
refused";
    let responses = run_cells(&worker, BTreeMap::new(), &[refusals, refusals]).await;
    for response in &responses {
        assert_eq!(cell_outcome(response), ("600".to_owned(), None));
    }
}

#[tokio::test]
async fn final_values_render_as_python_str() {
    let Some(worker) = py_host() else { return };
    let responses = run_cells(
        &worker,
        BTreeMap::new(),
        &[
            "1 + 2j",
            "True",
            "1.5",
            "[1, 'a', None, (2, 3)]",
            "'text'",
            "None",
        ],
    )
    .await;
    let rendered: Vec<_> = responses.iter().map(cell_outcome).collect();
    assert_eq!(
        rendered,
        [
            "(1+2j)",
            "True",
            "1.5",
            "[1, 'a', None, (2, 3)]",
            "text",
            "None"
        ]
        .map(|text| (text.to_owned(), None))
    );
}

#[tokio::test]
async fn shared_values_cannot_expand_past_the_host_bounds() {
    let Some(worker) = py_host() else { return };
    let shared = "a = [1]\nfor _ in range(40):\n    a = [a, a]\n";
    let responses = run_cells(
        &worker,
        BTreeMap::from([("fetch".into(), dictionary_tool("fetch_tool"))]),
        &[
            &format!("{shared}a"),
            &format!(
                "{shared}try:\n    await fetch({{'x': a}})\nexcept ValueError as error:\n    print(error)\n"
            ),
        ],
    )
    .await;
    let (text, error) = cell_outcome(&responses[0]);
    assert_eq!(error, None);
    assert!(text.len() <= MAX_PRINT_BYTES, "{}", text.len());
    assert!(text.ends_with("\n[Python result truncated at 1 MiB]"));
    assert_eq!(
        cell_outcome(&responses[1]),
        ("Python tool argument exceeds 1 MiB\nNone".to_owned(), None)
    );
}

#[tokio::test]
async fn print_streams_keep_their_order_within_a_cell() {
    let Some(worker) = py_host() else { return };
    let responses = run_cells(
        &worker,
        BTreeMap::new(),
        &["import sys\nprint('one')\nprint('two', file=sys.stderr)\nprint('three')"],
    )
    .await;
    assert_eq!(
        cell_outcome(&responses[0]),
        ("one\ntwo\nthree\nNone".to_owned(), None)
    );
}

#[tokio::test]
async fn the_interpreter_memory_limit_fails_the_cell_and_keeps_the_session() {
    let Some(worker) = py_host() else { return };
    let responses = run_cells(
        &worker,
        BTreeMap::new(),
        &["x = 'a' * (300 * 1024 * 1024)", "1 + 1"],
    )
    .await;
    let (_, error) = cell_outcome(&responses[0]);
    assert!(
        error
            .as_deref()
            .is_some_and(|error| error.contains("MemoryError")),
        "{error:?}"
    );
    assert_eq!(cell_outcome(&responses[1]), ("2".to_owned(), None));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_large_int_result_renders_in_bounded_host_time_and_tears_down() {
    let Some(worker) = py_host() else { return };
    let bundle = python_session(&worker, BTreeMap::new())
        .await
        .expect("monty session");
    let session = bundle.session;
    let mut build = blocking_cell("p0", "big = 1 << 64_000_000\n0");
    build.yield_time_ms = 30_000;
    let build = session.execute(build).await.expect("execute");
    let build = build.initial_response.await.expect("initial response");
    assert_eq!(cell_outcome(&build), ("0".to_owned(), None));
    let started = Instant::now();
    let mut render = blocking_cell("p1", "big");
    render.yield_time_ms = 30_000;
    let render = session.execute(render).await.expect("execute");
    let render = render.initial_response.await.expect("initial response");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        cell_outcome(&render),
        ("<int with 64000001 bits>".to_owned(), None)
    );
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(10), session.shutdown())
            .await
            .expect("teardown stays bounded"),
        LocalCompositionShutdownResult::Joined { .. }
    ));
}
