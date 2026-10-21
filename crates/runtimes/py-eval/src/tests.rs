#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::too_many_lines,
    reason = "integration tests against a live monty worker"
)]

use super::{
    ChildExitEscalation, ChildReapBounds, LocalCompositionEvent, LocalCompositionEventReceiver,
    LocalCompositionNestedDelivery, LocalCompositionNestedResponder, LocalCompositionResponse,
    codec, reap_child_exit_within, session_bundle,
};
use monty_pool::{Pool, PoolConfig, ReplConfig};
use plakous_agent::internal::{
    LocalCompositionContentItem, LocalCompositionExecuteRequest, LocalCompositionWaitOutcome,
    LocalCompositionWaitRequest,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

#[tokio::test]
async fn yielded_cell_wait_resumes_after_host_wait_and_is_cancellation_safe() {
    let Some(worker) = monty_worker() else { return };
    let bundle = super::session_bundle_within(
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
    let Some(worker) = monty_worker() else { return };
    let bundle = session_bundle(
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
        plakous_agent::internal::LocalCompositionTerminateResult::Joined(
            LocalCompositionWaitOutcome::Missing(_)
        )
    ));
    assert!(
        matches!(session.terminate(id.clone()).await, plakous_agent::internal::LocalCompositionTerminateResult::Joined(LocalCompositionWaitOutcome::Live(LocalCompositionResponse::Terminated { cell_id, .. })) if cell_id == id)
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

#[tokio::test]
async fn terminating_a_buffered_result_preserves_persistent_globals() {
    use plakous_agent::internal::LocalCompositionSession as _;
    let Some(worker) = monty_worker() else { return };
    let pool = Pool::new(super::pool_config(&worker, super::WORKER_REQUEST_TIMEOUT))
        .await
        .expect("pool");
    let checkout = pool
        .checkout(&super::repl_config())
        .await
        .expect("checkout");
    let (events_tx, _events) = tokio::sync::mpsc::channel(1);
    let session = super::MontySession {
        checkout: std::sync::Arc::new(tokio::sync::Mutex::new(Some(checkout))),
        feed_task: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
        cells: std::sync::Arc::new(tokio::sync::Mutex::new(BTreeMap::new())),
        events_tx,
        next_cell: std::sync::atomic::AtomicU64::new(1),
        bindings: std::sync::Arc::new(BTreeMap::new()),
        last_child_exit: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
        generation_lost: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    let started = session
        .execute(blocking_cell("buffered", "counter = 41\ncounter"))
        .await
        .expect("execute");
    let id = started.cell_id.clone();
    drop(started.initial_response);
    let mut completion = session.cells.lock().await[&id.parse::<u64>().expect("id")]
        .completion
        .clone();
    tokio::time::timeout(
        Duration::from_secs(10),
        completion.wait_for(Option::is_some),
    )
    .await
    .expect("completion bound")
    .expect("completion");
    let terminal = session.terminate(id).await;
    assert!(
        matches!(terminal, plakous_agent::internal::LocalCompositionTerminateResult::Joined(LocalCompositionWaitOutcome::Live(LocalCompositionResponse::Result { error: None, ref items, .. })) if items.iter().any(|item| matches!(item, LocalCompositionContentItem::Text(text) if text.contains("41"))))
    );
    assert!(!session.generation_lost());
    let next = session
        .execute(blocking_cell("next", "counter + 1"))
        .await
        .expect("next execute")
        .initial_response
        .await
        .expect("next result");
    assert!(
        matches!(next, LocalCompositionResponse::Result { error: None, ref items, .. } if items.iter().any(|item| matches!(item, LocalCompositionContentItem::Text(text) if text.contains("42"))))
    );
    let _ = session.shutdown().await;
}

#[test]
fn cumulative_print_cap_reports_truncation_once_on_a_char_boundary() {
    let mut printed = super::Printed {
        text: String::new(),
        truncated: false,
        output: std::sync::Arc::new(std::sync::Mutex::new(super::PrintObservation::default())),
    };
    super::append_print(&mut printed, &"é".repeat(super::MAX_PRINT_BYTES));
    assert!(printed.text.len() <= super::MAX_PRINT_BYTES);
    assert!(printed.text.contains("[Python output truncated at 1 MiB]"));
    let header = "Python cell running with cell ID 18446744073709551615\nOutput:\n";
    assert!(
        header.len() + printed.text.len() <= 1_048_576,
        "the truncation notice must survive status-inclusive projection"
    );
    let first = super::drain_print(&printed.output);
    assert_ne!(first, [] as [LocalCompositionContentItem; 0]);
    super::append_print(&mut printed, "more dropped output");
    assert_eq!(
        super::drain_print(&printed.output),
        [] as [LocalCompositionContentItem; 0]
    );
}

fn dictionary_tool(name: &str) -> super::PythonToolBinding {
    super::PythonToolBinding {
        tool_name: name.into(),
        input_kind: super::PythonToolInput::Dictionary,
    }
}

fn monty_worker() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("PLAK_MONTY_WORKER") {
        let path = PathBuf::from(&explicit);
        assert!(
            path.is_file(),
            "PLAK_MONTY_WORKER is set to {} but the file is missing",
            path.display()
        );
        return Some(path);
    }
    if let Some(path) = std::env::current_exe()
        .ok()
        .and_then(|path| {
            path.parent()
                .and_then(Path::parent)
                .map(|directory| directory.join("monty"))
        })
        .filter(|path| path.is_file())
    {
        return Some(path);
    }
    None
}

async fn run_source(
    worker: &Path,
    bindings: BTreeMap<String, super::PythonToolBinding>,
    source: &str,
) -> LocalCompositionResponse {
    let bundle = session_bundle(worker, bindings)
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

#[test]
fn worker_limits_bound_requests_without_expiring_the_session() {
    let pool = super::pool_config(Path::new("/unused-worker"), super::WORKER_REQUEST_TIMEOUT);
    assert_eq!(pool.min_processes, 0);
    assert_eq!(pool.max_processes, 1);
    assert_eq!(super::WORKER_REQUEST_TIMEOUT, Duration::from_secs(90));
    assert_eq!(pool.request_timeout, Some(Duration::from_secs(90)));

    let limits = super::repl_config().limits.expect("resource limits");
    assert_eq!(limits.max_memory, Some(256 * 1024 * 1024));
    assert_eq!(limits.max_feed_duration, None);
    assert_eq!(limits.max_turn_duration, None);
}

#[test]
fn unsupported_json_number_becomes_protocol_error() {
    let value = serde_json::Value::Number(serde_json::Number::from(i64::MAX as u64 + 1));
    let err = codec::json_to_monty(&value).expect_err("integers above i64::MAX are not monty-safe");
    assert!(err.contains("unsupported JSON integer magnitude"), "{err}");
}

#[tokio::test]
async fn killing_monty_worker_does_not_kill_parent() {
    let Some(worker) = monty_worker() else {
        eprintln!(
            "skipping killing_monty_worker_does_not_kill_parent: monty worker not available (set PLAK_MONTY_WORKER)"
        );
        return;
    };
    let pool = Pool::new(PoolConfig {
        min_processes: 0,
        max_processes: 1,
        request_timeout: Some(Duration::from_secs(10)),
        ..PoolConfig::subprocess(&worker)
    })
    .await
    .expect("spawn monty pool");
    let checkout = pool
        .checkout(&ReplConfig::default())
        .await
        .expect("checkout monty worker");
    let pid = checkout.pid().expect("local monty worker has a pid");
    let status = Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .expect("kill worker");
    assert!(status.success());
    let marker =
        std::env::temp_dir().join(format!("plak-monty-parent-alive-{}", std::process::id()));
    std::fs::write(&marker, b"alive").expect("parent still writes after worker kill");
    let body = std::fs::read(&marker).expect("parent still reads after worker kill");
    assert_eq!(body, b"alive");
    let _ = std::fs::remove_file(&marker);
    drop(checkout);
    drop(pool);
}

#[tokio::test]
async fn nested_call_arrives_before_initial_response() {
    let Some(worker) = monty_worker() else {
        eprintln!(
            "skipping nested_call_arrives_before_initial_response: monty worker not available (set PLAK_MONTY_WORKER)"
        );
        return;
    };
    let mut bindings = BTreeMap::new();
    bindings.insert("fetch".into(), dictionary_tool("echo_tool"));
    let bundle = session_bundle(&worker, bindings)
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
    let Some(worker) = monty_worker() else {
        eprintln!(
            "skipping async_tool_call_returns_reply_text: monty worker not available (set PLAK_MONTY_WORKER)"
        );
        return;
    };
    let mut bindings = BTreeMap::new();
    bindings.insert("fetch".into(), dictionary_tool("echo_tool"));
    let bundle = session_bundle(&worker, bindings)
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
    let Some(worker) = monty_worker() else {
        eprintln!(
            "skipping asyncio_gather_resumes_in_program_order: monty worker not available (set PLAK_MONTY_WORKER)"
        );
        return;
    };
    let mut bindings = BTreeMap::new();
    bindings.insert("a".into(), dictionary_tool("tool_a"));
    bindings.insert("b".into(), dictionary_tool("tool_b"));
    let bundle = session_bundle(&worker, bindings)
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
    let Some(worker) = monty_worker() else {
        eprintln!(
            "skipping asyncio_gather_same_tool_uses_distinct_runtime_call_ids: monty worker not available (set PLAK_MONTY_WORKER)"
        );
        return;
    };
    let mut bindings = BTreeMap::new();
    bindings.insert("Read".into(), dictionary_tool("read_tool"));
    let bundle = session_bundle(&worker, bindings)
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
    let Some(worker) = monty_worker() else {
        eprintln!(
            "skipping failing_nested_call_is_catchable_in_python: monty worker not available (set PLAK_MONTY_WORKER)"
        );
        return;
    };
    let mut bindings = BTreeMap::new();
    bindings.insert("bad".into(), dictionary_tool("bad_tool"));
    let bundle = session_bundle(&worker, bindings)
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
    let Some(worker) = monty_worker() else {
        eprintln!(
            "skipping unbound_tool_name_fails_with_name_error: monty worker not available (set PLAK_MONTY_WORKER)"
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
    let Some(worker) = monty_worker() else {
        eprintln!(
            "skipping uncaught_python_exception_surfaces_cell_error: monty worker not available (set PLAK_MONTY_WORKER)"
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
    let Some(worker) = monty_worker() else {
        eprintln!(
            "skipping concurrent_execute_while_cell_running_is_rejected: monty worker not available (set PLAK_MONTY_WORKER)"
        );
        return;
    };
    let mut bindings = BTreeMap::new();
    bindings.insert("fetch".into(), dictionary_tool("echo_tool"));
    let bundle = session_bundle(&worker, bindings)
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

/// Bounds short enough to keep the escalation test fast; the shape is exactly
/// the production [`ChildReapBounds::DEFAULT`] ladder.
const TEST_REAP_BOUNDS: ChildReapBounds = ChildReapBounds {
    voluntary: Duration::from_millis(200),
    terminate: Duration::from_millis(200),
    kill: Duration::from_secs(2),
};

#[tokio::test]
async fn a_worker_that_ignores_sigterm_is_killed_inside_the_shutdown_bound() {
    // A wedged monty worker used to hold shutdown open forever: the reap was an
    // unbounded `waitpid`. Stand in a child that ignores SIGTERM and prove the
    // reap still returns inside its own bound with the child gone.
    // `exec` so the ignored SIGTERM disposition (which survives exec) belongs to
    // the pid we reap, with no orphan grandchild left behind.
    let mut stubborn = Command::new("sh")
        .arg("-c")
        .arg(r#"trap "" TERM; exec sleep 60"#)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn a child that ignores SIGTERM");
    let pid = stubborn.id();
    let started = Instant::now();
    let exit = reap_child_exit_within(
        pid,
        plakous_js_eval::sample_children_rusage(),
        TEST_REAP_BOUNDS,
    )
    .await;
    let elapsed = started.elapsed();

    assert_eq!(exit.escalation, ChildExitEscalation::Killed);
    let bound = TEST_REAP_BOUNDS.voluntary + TEST_REAP_BOUNDS.terminate + TEST_REAP_BOUNDS.kill;
    assert!(
        elapsed < bound,
        "shutdown took {elapsed:?}, past the {bound:?} bound"
    );
    // The pid is no longer a waitable child of ours: it was signalled and reaped.
    assert!(
        stubborn.try_wait().is_err(),
        "the stubborn child was left unreaped"
    );
}

#[tokio::test]
async fn a_worker_that_exits_on_its_own_is_never_signalled() {
    let mut cooperative = Command::new("sh")
        .arg("-c")
        .arg("exit 0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn a cooperative child");
    let pid = cooperative.id();
    let exit = reap_child_exit_within(
        pid,
        plakous_js_eval::sample_children_rusage(),
        TEST_REAP_BOUNDS,
    )
    .await;

    assert_eq!(exit.escalation, ChildExitEscalation::Voluntary);
    assert!(
        cooperative.try_wait().is_err(),
        "the cooperative child was left unreaped"
    );
}

#[tokio::test]
async fn unreaped_worker_never_certifies_joined_shutdown() {
    use super::*;
    let session = session_with_exit(ChildProcessExit {
        child: ChildProcessKind::PythonWorker,
        user_cpu: Duration::ZERO,
        system_cpu: Duration::ZERO,
        peak_rss_bytes: None,
        escalation: ChildExitEscalation::Unreaped,
    });
    assert!(matches!(
        session.shutdown().await,
        LocalCompositionShutdownResult::Lost(_)
    ));
    assert!(matches!(
        session.shutdown().await,
        LocalCompositionShutdownResult::Lost(_)
    ));
    let (_sender, completion) = watch::channel(Some(Ok(LocalCompositionResponse::Result {
        cell_id: "1".into(),
        items: Vec::new(),
        error: None,
    })));
    session.cells.lock().await.insert(
        1,
        CellObservation {
            completion,
            output: Arc::new(std::sync::Mutex::new(PrintObservation::default())),
        },
    );
    assert!(
        matches!(
            session.terminate("1".into()).await,
            LocalCompositionTerminateResult::Lost(_)
        ),
        "a buffered result cannot override unreaped cleanup evidence"
    );
}

#[tokio::test]
async fn successive_cells_retain_state_without_a_cumulative_duration_budget() {
    let Some(worker) = monty_worker() else {
        eprintln!(
            "skipping successive_cells_retain_state_without_a_cumulative_duration_budget: monty worker not available (set PLAK_MONTY_WORKER)"
        );
        return;
    };
    tokio::time::timeout(Duration::from_secs(30), async {
        let bundle = session_bundle(&worker, BTreeMap::new())
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
            super::LocalCompositionShutdownResult::Joined { .. }
        ));
    })
    .await
    .expect("successive cells must complete within the scenario bound");
}

/// A request bound short enough to observe the kill-and-reap path without
/// waiting out the production [`super::WORKER_REQUEST_TIMEOUT`]. Only the bound
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
    // A cell the worker never answers must not hold the parent open: the pool
    // kills the worker at its request bound and the cell reports a session
    // loss, which is a strictly different outcome from a Python exception.
    let Some(worker) = monty_worker() else {
        eprintln!(
            "skipping a_stuck_worker_request_is_killed_and_reported_as_session_loss: monty worker not available (set PLAK_MONTY_WORKER)"
        );
        return;
    };
    tokio::time::timeout(Duration::from_secs(30), async {
        let bundle =
            super::session_bundle_within(&worker, BTreeMap::new(), TEST_WORKER_REQUEST_TIMEOUT)
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
        let super::LocalCompositionSessionLoss::HostLost(detail) = loss else {
            panic!("a stuck worker must be host loss, not a cell-level error");
        };
        assert!(
            detail.to_lowercase().contains("timeout")
                || detail.to_lowercase().contains("timed out"),
            "loss detail should name the expired request bound: {detail}"
        );

        // The pool killed and discarded the worker itself, so the checkout no
        // longer carries a diagnostic pid. An absent pid is never a receipt
        // that the worker was reaped, so shutdown stays fail-closed on a
        // session that was already lost rather than certifying a clean join.
        assert!(
            matches!(
                session.shutdown().await,
                super::LocalCompositionShutdownResult::Lost(_)
            ),
            "a session lost to a request timeout must not certify a joined shutdown"
        );
    })
    .await
    .expect("the stuck-worker scenario must finish inside its own bound");
}

#[tokio::test]
async fn terminal_cells_retain_state_without_retaining_response_streams() {
    let Some(worker) = monty_worker() else { return };
    let bundle = session_bundle(&worker, BTreeMap::new())
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
        super::LocalCompositionShutdownResult::Joined { .. }
    ));
}

fn session_with_exit(exit: super::ChildProcessExit) -> super::MontySession {
    use super::*;
    let (events_tx, _) = mpsc::channel(1);
    MontySession {
        cells: Arc::new(Mutex::new(BTreeMap::new())),
        checkout: Arc::new(Mutex::new(None)),
        feed_task: Arc::new(Mutex::new(None)),
        events_tx,
        next_cell: AtomicU64::new(1),
        bindings: Arc::new(BTreeMap::new()),
        last_child_exit: Arc::new(Mutex::new(Some(exit))),
        generation_lost: Arc::new(AtomicBool::new(false)),
    }
}

/// The exact monty-pool protocol context observed in the field (0.0.21) when a
/// raised exception carried a traceback frame whose end column preceded its
/// start column.
const UNDECODABLE_TRACEBACK: &str = "invalid exception payload: invalid value for StackFrame.end.column: 5 is before start column 12";

#[test]
fn undecodable_traceback_is_a_cell_failure_and_loses_only_the_generation() {
    use super::*;
    let lost = AtomicBool::new(false);
    let response = map_turn_error(
        7,
        "partial stdout\n",
        PoolError::Protocol(UNDECODABLE_TRACEBACK.into()),
        true,
        &lost,
    )
    .expect("a raised exception is a definite cell outcome, not a host loss");
    let LocalCompositionResponse::Result {
        cell_id,
        items,
        error,
    } = response
    else {
        panic!("expected a terminal cell result, got {response:?}");
    };
    assert_eq!(cell_id, "7");
    assert_eq!(
        items,
        vec![LocalCompositionContentItem::Text(
            "partial stdout\n".to_owned()
        )]
    );
    let error = error.expect("a raised cell reports an error");
    assert!(
        error.starts_with("Python cell failed; its traceback frame spans were undecodable"),
        "{error}"
    );
    assert!(error.ends_with(UNDECODABLE_TRACEBACK), "{error}");
    assert!(
        lost.load(Ordering::Acquire),
        "the violating reply discarded the worker, so the generation is gone"
    );
}

#[test]
fn other_protocol_violations_remain_host_losses() {
    use super::*;
    for context in [
        "unexpected event",
        "worker sent an event with no kind",
        "error event with no exception",
    ] {
        let discarded = AtomicBool::new(false);
        let outcome = map_turn_error(1, "", PoolError::Protocol(context.into()), true, &discarded)
            .expect_err("an unknown cell outcome is a host loss");
        assert!(
            matches!(outcome, LocalCompositionSessionLoss::HostLost(_)),
            "{outcome:?}"
        );
        assert!(!discarded.load(Ordering::Acquire));
    }
}

#[tokio::test]
async fn a_lost_generation_still_certifies_its_reaped_worker() {
    use super::*;
    let session = session_with_exit(ChildProcessExit {
        child: ChildProcessKind::PythonWorker,
        user_cpu: Duration::ZERO,
        system_cpu: Duration::ZERO,
        peak_rss_bytes: None,
        escalation: ChildExitEscalation::Voluntary,
    });
    session.generation_lost.store(true, Ordering::Release);
    assert!(session.generation_lost());
    assert!(matches!(
        session.shutdown().await,
        LocalCompositionShutdownResult::Joined {
            child_exit: Some(_)
        }
    ));
}

#[tokio::test]
async fn failed_reap_task_join_cannot_certify_joined_cleanup() {
    use super::*;
    let task = tokio::spawn(std::future::pending::<ChildReapEscalation>());
    task.abort();
    let exit = finish_reap(task, plakous_js_eval::sample_children_rusage()).await;
    assert_eq!(exit.escalation, ChildExitEscalation::Unreaped);
    let session = session_with_exit(exit);
    assert!(matches!(
        session.terminate("1".into()).await,
        LocalCompositionTerminateResult::Lost(_)
    ));
    assert!(matches!(
        session.shutdown().await,
        LocalCompositionShutdownResult::Lost(_)
    ));
}

#[tokio::test]
async fn queued_monty_reply_waits_for_correlated_worker_receipt() {
    let (nested_tx, mut nested_rx) = tokio::sync::mpsc::channel(1);
    let responder = Box::new(super::MontyNestedResponder {
        call_id: 7,
        nested_tx,
    });
    let delivery = tokio::spawn(responder.resolve(serde_json::json!("result")));
    let (call_id, reply) = nested_rx.recv().await.expect("queued reply");
    assert_eq!(call_id, 7);
    assert!(
        !delivery.is_finished(),
        "queue handoff must not certify acceptance"
    );
    reply
        .accepted
        .send(LocalCompositionNestedDelivery::Accepted)
        .expect("receipt listener");
    assert_eq!(
        delivery.await.expect("delivery task").expect("delivery"),
        LocalCompositionNestedDelivery::Accepted
    );
}

#[tokio::test]
async fn lost_monty_receipt_never_certifies_queued_delivery() {
    let (nested_tx, mut nested_rx) = tokio::sync::mpsc::channel(1);
    let responder = Box::new(super::MontyNestedResponder {
        call_id: 9,
        nested_tx,
    });
    let delivery = tokio::spawn(responder.reject("failure".into()));
    let (_, reply) = nested_rx.recv().await.expect("queued reply");
    drop(reply);
    assert_eq!(
        delivery.await.expect("delivery task").expect("delivery"),
        LocalCompositionNestedDelivery::HostLost
    );
}

async fn assert_real_worker_future_receipts(
    source: &str,
    expected_inputs: &[serde_json::Value],
    expected_output: &str,
) {
    let Some(worker) = monty_worker() else {
        eprintln!("skipping future receipt regression: set PLAK_MONTY_WORKER");
        return;
    };
    let bundle = session_bundle(
        &worker,
        BTreeMap::from([("fetch".into(), dictionary_tool("fetch_tool"))]),
    )
    .await
    .expect("monty session");
    let session = bundle.session;
    let mut events = bundle.events;
    let outcome = tokio::time::timeout(Duration::from_secs(30), async {
        let started = session
            .execute(LocalCompositionExecuteRequest {
                start_admission: plakous_agent::internal::LocalCompositionStartAdmission::new(
                    || Box::pin(async { Ok(()) }),
                ),
                tool_call_id: "future-receipts".into(),
                enabled_tools: Vec::new(),
                source: source.into(),
                yield_time_ms: 90_000,
                max_output_tokens: 10_000,
            })
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
        super::LocalCompositionShutdownResult::Joined { .. }
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

#[test]
fn futures_batch_validation_refuses_non_terminal_results() {
    for value in [
        monty_pool::ResumeValue::Future,
        monty_pool::ResumeValue::NotFound,
        monty_pool::ResumeValue::NotHandled,
    ] {
        assert!(super::validate_futures_batch(&[(1, value)]).is_err());
    }
}

#[test]
fn nested_results_are_bounded_in_depth_before_they_reach_the_wire() {
    // Monty 1.x carries values as a flat node arena with no depth limit of its
    // own, so the host bounds nesting where it converts JSON.
    let nested = |depth| {
        (0..depth).fold(serde_json::Value::Null, |value, _| {
            serde_json::json!([value])
        })
    };
    let value = codec::json_to_monty(&nested(128)).expect("JSON result at the bound");
    super::validate_futures_batch(&[(1, monty_pool::ResumeValue::Return(value))])
        .expect("a result at the depth bound fits the wire");
    let error = codec::json_to_monty(&nested(129)).expect_err("one level past the bound");
    assert!(error.contains("nests deeper than 128 levels"), "{error}");
}

#[tokio::test]
async fn undecodable_nested_result_never_acknowledges_delivery() {
    let Some(worker) = monty_worker() else {
        return;
    };
    let bundle = session_bundle(
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
        Err(super::LocalCompositionSessionLoss::ProtocolViolation(_))
    ));
    assert!(matches!(
        session.shutdown().await,
        super::LocalCompositionShutdownResult::Joined { .. }
    ));
}

#[tokio::test]
async fn uncaught_nested_rejection_is_accepted_and_preserves_python_exception() {
    let Some(worker) = monty_worker() else {
        return;
    };
    let bundle = session_bundle(
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
            source: "await bad({})".into(),
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
        super::LocalCompositionShutdownResult::Joined { .. }
    ));
}

#[tokio::test]
async fn allocator_exit_while_reading_nested_result_is_not_acceptance() {
    let Some(worker) = monty_worker() else {
        return;
    };
    let pool = Pool::new(PoolConfig {
        min_processes: 0,
        max_processes: 1,
        request_timeout: Some(Duration::from_secs(10)),
        ..PoolConfig::subprocess(&worker)
    })
    .await
    .expect("pool");
    let mut checkout = pool
        .checkout(&ReplConfig {
            limits: Some(monty_types::ResourceLimits {
                max_memory: Some(1024 * 1024),
                ..Default::default()
            }),
            ..ReplConfig::default()
        })
        .await
        .expect("checkout");
    let mut printed = super::Printed {
        text: String::new(),
        truncated: false,
        output: std::sync::Arc::new(std::sync::Mutex::new(super::PrintObservation::default())),
    };
    let mut on_print = monty_pool::on_print_sync(|_, _: &str| {});
    assert!(matches!(
        checkout
            .feed(
                "await fetch({})",
                monty_types::NamedValues::new(),
                Vec::new(),
                false,
                &mut on_print
            )
            .await
            .expect("feed"),
        monty_pool::TurnEvent::FunctionCall { .. }
    ));
    let pending_call_ids = match checkout
        .resume(monty_pool::ResumeValue::Future, &mut on_print)
        .await
        .expect("stash future")
    {
        monty_pool::TurnEvent::ResolveFutures {
            pending_call_ids, ..
        } => pending_call_ids,
        other => panic!("unexpected suspension: {other:?}"),
    };
    let results = vec![(
        pending_call_ids[0],
        monty_pool::ResumeValue::Return(monty_types::MontyObject::string(
            "x".repeat(16 * 1024 * 1024),
        )),
    )];
    super::validate_futures_batch(&results)
        .expect("valid protocol frame exceeds worker allocation budget");
    let (accepted, receipt) = tokio::sync::oneshot::channel();
    let result = super::resume_futures_batch(
        &mut checkout,
        &mut printed,
        super::FuturesBatch {
            results,
            acceptances: vec![accepted],
        },
    )
    .await;
    assert!(
        matches!(result, Err(monty_pool::PoolError::Runtime(ref error)) if error.exc_type() == monty_types::ExcType::MemoryError),
        "{result:?}"
    );
    assert!(checkout.pid().is_none());
    assert_eq!(
        receipt.await.expect("loss receipt"),
        LocalCompositionNestedDelivery::HostLost
    );
    pool.close().await;
}

/// A rejected nested call raised across a multi-line call expression is the
/// `deepseek/deepseek-v4-flash` `py-ptc` exact-copy shape, whose JSONL position
/// ended `recovery_required` with "nested local
/// composition result receiver disappeared or could not prove acceptance"
/// seventeen milliseconds after `apply_patch` reported a definite failure. The
/// cell's `await apply_patch({...})` spanned several lines, so the raised
/// traceback carried a frame whose end column preceded its start column,
/// monty-proto 0.0.21 refused the payload, and the acknowledgement of a result
/// the worker had provably consumed was reported as a loss.
///
/// The cell outcome must be a definite Python failure and the delivery must be
/// an acceptance, because the worker could only raise by running the resumed
/// cell. From 1.1.0 the traceback decodes, so the failure is ordinary.
#[tokio::test]
async fn nested_rejection_raised_across_a_multiline_call_is_an_ordinary_cell_failure() {
    let Some(worker) = monty_worker() else {
        eprintln!(
            "skipping nested_rejection_raised_across_a_multiline_call_is_an_ordinary_cell_failure: monty worker not available (set PLAK_MONTY_WORKER)"
        );
        return;
    };
    let bundle = session_bundle(
        &worker,
        BTreeMap::from([("apply_patch".into(), dictionary_tool("apply_patch_tool"))]),
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
            tool_call_id: "multiline-nested".into(),
            enabled_tools: Vec::new(),
            source: "result = await apply_patch({\n    \"patch\": \"\"\"not a diff\nat all\"\"\"\n})\nprint(result)"
                .into(),
            yield_time_ms: 90_000,
            max_output_tokens: 10_000,
        })
        .await
        .expect("execute");
    dispatch_nested_calls(
        &mut events,
        [(
            "apply_patch_tool".into(),
            NestedPlan::Reject("patch text is not a valid git-style unified diff".into()),
        )],
    )
    .await;
    let response = started
        .initial_response
        .await
        .expect("a raised cell is a definite outcome, never a host loss");
    let LocalCompositionResponse::Result { error, .. } = response else {
        panic!("expected a terminal cell result");
    };
    let error = error.expect("the cell raised");
    // Monty 1.1.0 renders multi-line call spans without caret math, so this
    // former undecodable-traceback shape is an ordinary Python failure.
    assert!(
        !error.contains(super::MONTY_UNDECODABLE_EXCEPTION_CONTEXT),
        "{error}"
    );
    assert!(
        error.contains("patch text is not a valid git-style unified diff"),
        "{error}"
    );
    assert!(matches!(
        session.shutdown().await,
        super::LocalCompositionShutdownResult::Joined { .. }
    ));
}

#[test]
fn an_undecodable_traceback_acknowledges_the_result_the_worker_consumed() {
    use super::{LocalCompositionNestedDelivery as Delivery, batch_acceptance};
    use monty_pool::PoolError;
    assert_eq!(
        batch_acceptance(
            &Err(PoolError::Protocol(UNDECODABLE_TRACEBACK.into())),
            None
        ),
        Delivery::Accepted,
        "only a worker that ran the resumed cell can raise, so it consumed the result"
    );
    for context in ["unexpected event", "worker sent an event with no kind"] {
        assert_eq!(
            batch_acceptance(&Err(PoolError::Protocol(context.into())), Some(99)),
            Delivery::HostLost,
            "{context} leaves acceptance undetermined"
        );
    }
    assert_eq!(
        batch_acceptance(
            &Err(PoolError::Runtime(monty_types::MontyException::new(
                monty_types::ExcType::MemoryError,
                None
            ))),
            None
        ),
        Delivery::HostLost,
        "a worker that is gone cannot prove it consumed the result"
    );
    assert_eq!(
        batch_acceptance(
            &Err(PoolError::Runtime(monty_types::MontyException::new(
                monty_types::ExcType::RuntimeError,
                None
            ))),
            Some(99)
        ),
        Delivery::Accepted,
        "a surviving checkout answered this exact request"
    );
}

#[tokio::test]
async fn a_result_delivered_after_the_cell_ended_is_definite_non_delivery() {
    // The feed loop owns the receiving half for exactly as long as the cell
    // runs, so a closed channel is the host's proof that the cell already
    // reached its own outcome and never saw this result.
    let (nested_tx, nested_rx) = tokio::sync::mpsc::channel(1);
    drop(nested_rx);
    let responder = Box::new(super::MontyNestedResponder {
        call_id: 11,
        nested_tx,
    });
    let delivery = responder
        .resolve(serde_json::json!("too late"))
        .await
        .expect("delivery");
    assert!(
        matches!(delivery, LocalCompositionNestedDelivery::NotObserved(_)),
        "an ended cell provably never observed the result: {delivery:?}"
    );
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
    let Some(worker) = monty_worker() else { return };
    let bundle = session_bundle(
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
        super::LocalCompositionShutdownResult::Joined { .. }
    ));
    drop(session);
    assert!(events.receive().await.expect("closed events").is_none());
    outcome.expect("persistent wrappers must complete");
}

#[tokio::test]
async fn malformed_public_tool_names_cannot_enter_worker_bootstrap() {
    let Some(worker) = monty_worker() else { return };
    for name in [
        "",
        "await",
        "has-dash",
        "fetch():\n    print('injected')\n#",
        "_plakous_host_0",
    ] {
        assert!(
            session_bundle(
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
    let Some(worker) = monty_worker() else { return };
    let bundle = session_bundle(
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
            super::MAX_OUTSTANDING_NESTED
        )))
        .await
        .expect("execute bounded gather");
    let mut responders = Vec::new();
    let received = tokio::time::timeout(Duration::from_secs(30), async {
        for index in 0..super::MAX_OUTSTANDING_NESTED {
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
        super::LocalCompositionTerminateResult::Joined(_)
    ));
    assert!(matches!(
        shutdown,
        super::LocalCompositionShutdownResult::Joined { .. }
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
    let Some(worker) = monty_worker() else { return };
    let bundle = session_bundle(
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
            super::MAX_OUTSTANDING_NESTED + 1
        )))
        .await
        .expect("execute oversized gather");
    let outcome = tokio::time::timeout(Duration::from_secs(30), started.initial_response).await;
    assert!(matches!(
        session.shutdown().await,
        super::LocalCompositionShutdownResult::Joined { .. }
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
    assert_eq!(dispatched, super::MAX_OUTSTANDING_NESTED);
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
    let Some(worker) = monty_worker() else { return };
    let bundle = session_bundle(
        &worker,
        BTreeMap::from([(
            "patch".into(),
            super::PythonToolBinding {
                tool_name: "patch_tool".into(),
                input_kind: super::PythonToolInput::String,
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
        super::LocalCompositionShutdownResult::Joined { .. }
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
    let Some(worker) = monty_worker() else { return };
    let bundle = session_bundle(
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
        super::LocalCompositionShutdownResult::Joined { .. }
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
    bindings: BTreeMap<String, super::PythonToolBinding>,
    sources: &[&str],
) -> Vec<LocalCompositionResponse> {
    let bundle = session_bundle(worker, bindings)
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

#[test]
fn repl_config_states_the_suspension_and_os_policy_explicitly() {
    let config = super::repl_config();
    let limits = config.limits.expect("resource limits");
    assert_eq!(
        limits.max_suspensions,
        usize::MAX,
        "monty-pool counts suspensions per checkout; a finite cap would expire the interpreter"
    );
    assert_eq!(limits.max_total_sleep, None);
    assert_eq!(config.os_policy.sleep, monty_types::SleepMode::CallHost);
    assert_eq!(
        config.os_policy.datetime,
        monty_types::DateTimeSource::System
    );
    assert_eq!(
        config.os_policy.random_start,
        monty_types::RandomStart::System
    );
    assert_eq!(
        config.os_policy.timezone,
        monty_types::SandboxTimeZone::utc()
    );
    assert!(!config.type_check);
}

#[test]
fn a_live_worker_memory_error_is_a_cell_failure_and_a_dead_one_is_host_loss() {
    use super::*;
    let memory_error = || {
        PoolError::Runtime(MontyException::new(
            ExcType::MemoryError,
            Some("memory limit exceeded".into()),
        ))
    };
    let lost = AtomicBool::new(false);
    let (_, error) = cell_outcome(
        &map_turn_error(3, "", memory_error(), true, &lost)
            .expect("the interpreter's own MemoryError leaves the worker alive"),
    );
    assert!(error.is_some_and(|error| error.contains("MemoryError")));
    assert!(matches!(
        map_turn_error(3, "", memory_error(), false, &lost),
        Err(LocalCompositionSessionLoss::HostLost(_))
    ));
    assert!(!lost.load(Ordering::Acquire));
}

#[tokio::test]
async fn os_calls_are_refused_and_clock_and_random_are_answered_in_the_worker() {
    let Some(worker) = monty_worker() else { return };
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
async fn new_builtin_modules_type_check_and_run_against_vendored_stubs() {
    use monty_pool::TurnEvent;
    use monty_types::NamedValues;

    let Some(worker) = monty_worker() else { return };
    let pool = Pool::new(super::pool_config(&worker, super::WORKER_REQUEST_TIMEOUT))
        .await
        .expect("pool");
    let mut checkout = pool
        .checkout(&ReplConfig {
            type_check: true,
            ..super::repl_config()
        })
        .await
        .expect("checkout");
    let mut on_print = monty_pool::on_print_sync(|_, _| {});
    let source = r#"import csv
import hashlib
import tomllib
import bisect
import os.path
assert list(csv.reader(["a,b", "1,2"])) == [["a", "b"], ["1", "2"]]
assert hashlib.sha256(b"x").hexdigest() == "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881"
assert tomllib.loads("x = 1")["x"] == 1
assert bisect.bisect([1, 3, 5], 4) == 2
assert os.path.join("/a", "b") == "/a/b"
True"#;
    let mut event = checkout
        .feed(source, NamedValues::new(), Vec::new(), false, &mut on_print)
        .await
        .expect("type checked feed");
    loop {
        match event {
            TurnEvent::Complete(value) => {
                assert_eq!(codec::format_monty(&value, 1024), "True");
                break;
            }
            TurnEvent::OsCall { function_name, .. } => {
                assert_eq!(function_name, "import_stubs");
                event = checkout
                    .resume(super::os_call_refusal(&function_name), &mut on_print)
                    .await
                    .expect("built-in stubs suffice");
            }
            other => panic!("unexpected built-in host request: {other:?}"),
        }
    }
    let event = checkout
        .feed(
            "import plak_host_module",
            NamedValues::new(),
            Vec::new(),
            false,
            &mut on_print,
        )
        .await
        .expect("unknown import requests stubs");
    let TurnEvent::OsCall {
        function_name,
        args,
        ..
    } = event
    else {
        panic!("expected import_stubs, got {event:?}");
    };
    assert_eq!(function_name, "import_stubs");
    assert!(format!("{args:?}").contains("plak_host_module"));
    let refusal = super::os_call_refusal(&function_name);
    assert!(matches!(refusal, monty_pool::ResumeValue::NotHandled));
    let error = checkout
        .resume(refusal, &mut on_print)
        .await
        .expect_err("no host stubs");
    assert!(matches!(error, monty_pool::PoolError::Typing(_)), "{error}");
    assert!(error.to_string().contains("plak_host_module"), "{error}");
    checkout.finish().await.expect("finish");
}

#[tokio::test]
async fn the_session_outlives_upstreams_default_suspension_budget() {
    let Some(worker) = monty_worker() else { return };
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
    let Some(worker) = monty_worker() else { return };
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
    let Some(worker) = monty_worker() else { return };
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
    assert!(text.len() <= super::MAX_PRINT_BYTES, "{}", text.len());
    assert!(text.ends_with("\n[Python result truncated at 1 MiB]"));
    assert_eq!(
        cell_outcome(&responses[1]),
        ("Python tool argument exceeds 1 MiB\nNone".to_owned(), None)
    );
}

#[tokio::test]
async fn print_streams_keep_their_order_within_a_cell() {
    let Some(worker) = monty_worker() else { return };
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
    let Some(worker) = monty_worker() else { return };
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

/// Runs `work` on its own thread and fails if it does not return within `limit`.
fn within<T: Send + 'static>(limit: Duration, work: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    rx.recv_timeout(limit)
        .unwrap_or_else(|_| panic!("host-side conversion ran past {limit:?}"))
}

#[test]
fn shared_dicts_cannot_make_argument_conversion_walk_unbounded_pairs() {
    use monty_types::unstable::{MontyGraph, MontyNode, NodeId, object_from_graph};
    // One arena of ~2 MB: 50k references to one dict of 100k keys would be
    // 5e9 pairs if every reference were walked.
    for string_keys in [false, true] {
        let mut graph = MontyGraph::new();
        let zero = graph.push(MontyNode::Int(0));
        let pairs: Vec<(NodeId, NodeId)> = (0..100_000)
            .map(|key| {
                let key = if string_keys {
                    MontyNode::String(key.to_string())
                } else {
                    MontyNode::Int(key)
                };
                (graph.push(key), zero)
            })
            .collect();
        let shared = graph.push(MontyNode::Dict(pairs));
        let references = graph.push(MontyNode::List(vec![shared; 50_000]));
        let name = graph.push(MontyNode::String("x".into()));
        let root = graph.push(MontyNode::Dict(vec![(name, references)]));
        let argument = object_from_graph(graph, root).expect("valid graph");
        let outcome = within(Duration::from_secs(10), move || {
            codec::call_to_json(argument.as_ref()).map(|json| json.to_string().len())
        });
        let expected = if string_keys {
            "Python tool argument exceeds 1 MiB"
        } else {
            "Python tool argument dict keys must be str"
        };
        assert_eq!(outcome, Err(expected.to_owned()));
    }
}

#[test]
fn the_argument_budget_is_the_compact_json_byte_count() {
    use monty_types::MontyObject;
    let exceeded = Err("Python tool argument exceeds 1 MiB".to_owned());
    let ints = MontyObject::list((0..1_000_000).map(|_| MontyObject::int(i64::MIN)));
    assert_eq!(
        codec::call_to_json(ints.as_ref()).map(|json| json.to_string().len()),
        exceeded
    );
    // A string's encoding is its text plus two quotes; a dict adds braces,
    // the quoted key and a colon.
    let fits = |text_bytes: usize| {
        let argument = MontyObject::dict([(
            MontyObject::string("k"),
            MontyObject::string("x".repeat(text_bytes)),
        )]);
        codec::call_to_json(argument.as_ref()).map(|json| json.to_string().len())
    };
    let overhead = r#"{"k":""}"#.len();
    assert_eq!(fits(1_048_576 - overhead), Ok(1_048_576));
    assert_eq!(fits(1_048_576 - overhead + 1), exceeded);
    let escaped = MontyObject::string("\n".repeat(600_000));
    assert_eq!(
        codec::call_to_json(escaped.as_ref()).map(|json| json.to_string().len()),
        exceeded,
        "escapes count as the bytes they encode to"
    );
}

#[test]
fn argument_ints_past_4300_digits_are_refused_like_cpython() {
    use monty_types::unstable::{MontyNode, object_from_node};
    let largest = (num_bigint::BigInt::from(1) << 14_284_u32) - num_bigint::BigInt::from(1);
    let digits = largest.to_string();
    let accepted = object_from_node(MontyNode::BigInt(largest));
    assert_eq!(
        codec::call_to_json(accepted.as_ref()),
        Ok(serde_json::Value::String(digits))
    );
    let refused = object_from_node(MontyNode::BigInt(
        num_bigint::BigInt::from(1) << 64_000_000_u32,
    ));
    let outcome = within(Duration::from_secs(10), move || {
        codec::call_to_json(refused.as_ref())
    });
    assert_eq!(
        outcome,
        Err(
            "Python tool argument int exceeds the limit (4300 digits) for integer string conversion"
                .to_owned()
        )
    );
}

#[test]
fn the_host_renders_a_large_int_value_in_bounded_time() {
    use monty_types::unstable::{MontyGraph, MontyNode, object_from_graph, object_from_node};
    for bits in [8_000_000_u32, 64_000_000] {
        let value = object_from_node(MontyNode::BigInt(num_bigint::BigInt::from(1) << bits));
        let rendered = within(Duration::from_secs(10), move || {
            codec::format_monty(&value, super::MAX_PRINT_BYTES)
        });
        assert_eq!(rendered, format!("<int with {} bits>", bits + 1));
    }
    let mut graph = MontyGraph::new();
    let big = graph.push(MontyNode::BigInt(num_bigint::BigInt::from(1) << 20_000_u32));
    let small = graph.push(MontyNode::Int(7));
    let root = graph.push(MontyNode::List(vec![big, small, big]));
    let nested = object_from_graph(graph, root).expect("valid graph");
    assert_eq!(
        codec::format_monty(&nested, super::MAX_PRINT_BYTES),
        "[<int with 20001 bits>, 7, <int with 20001 bits>]"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_large_int_result_renders_in_bounded_host_time_and_tears_down() {
    let Some(worker) = monty_worker() else { return };
    let bundle = session_bundle(&worker, BTreeMap::new())
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
        super::LocalCompositionShutdownResult::Joined { .. }
    ));
}
