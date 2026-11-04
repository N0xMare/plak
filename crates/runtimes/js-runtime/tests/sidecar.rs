//! Isolation and smoke tests for the shipped `plakous-js-host` binary.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "integration tests"
)]

use std::{collections::HashMap, path::Path, time::Duration};

use plakous_cell::{
    CellClient, CellEvent, CellOutcome, CellPrelude, CellValue, ChildProcessUsage, EngineConfig,
    NestedDeliveryOutcome, OutputItem, QUICKJS_WORKER as WORKER, WorkerFrame,
};
use plakous_js_runtime::{DEFAULT_YIELD_TIME_MS, RuntimeEvent};
use serde_json::Value;

fn host_bin() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_plakous-js-host"))
}

/// The worker through `CellClient`, in the evaluator's event and delivery
/// vocabulary, so one contract runs against both transports.
struct Worker(CellClient);

impl Worker {
    async fn spawn(program: &Path) -> Result<Self, String> {
        CellClient::spawn(program, WORKER, EngineConfig::QUICKJS)
            .await
            .map(Self)
            .map_err(|error| error.to_string())
    }

    fn child_id(&self) -> Option<u32> {
        self.0.child_id()
    }

    fn start_cell(
        &self,
        cell: u64,
        source: &str,
        stored: HashMap<String, Value>,
        tools: Vec<Value>,
        window: u64,
    ) -> Result<(), String> {
        self.0.start_cell(
            cell,
            source,
            CellPrelude::Quickjs { tools, stored },
            Some(window),
        )
    }

    fn resume(&self, cell: u64, window: u64) -> Result<(), String> {
        self.0.resume(cell, window)
    }

    fn interrupt(&self) {
        self.0.cancel(1);
    }

    #[allow(
        clippy::similar_names,
        reason = "cell and call are the protocol's own field names"
    )]
    async fn deliver_tool_result(
        &self,
        cell: u64,
        call: u64,
        value: Value,
        success: bool,
    ) -> NestedDeliveryOutcome {
        let outcome = if success {
            Ok(CellValue::from(value))
        } else {
            Err(match value {
                Value::String(reason) => reason,
                other => other.to_string(),
            })
        };
        self.0.deliver(cell, call, outcome).await
    }

    #[allow(
        clippy::similar_names,
        reason = "cell and call are the protocol's own field names"
    )]
    async fn send_tool_result(
        &self,
        cell: u64,
        call: u64,
        value: Value,
        success: bool,
    ) -> Result<(), String> {
        match self
            .deliver_tool_result(cell, call, value, success)
            .await
            .message()
        {
            None => Ok(()),
            Some(message) => Err(message.to_owned()),
        }
    }

    async fn read_event(&self) -> Result<RuntimeEvent, String> {
        let event = self.0.read_event().await?;
        Ok(match event {
            CellEvent::Waiting { .. } => {
                return Err("QuickJS unexpectedly waited on a batch".into());
            }
            CellEvent::NestedCall {
                cell,
                call,
                name,
                input,
            } => RuntimeEvent::ToolCall {
                cell_id: cell,
                id: call,
                name,
                input: Value::try_from(input)?,
            },
            CellEvent::Output { cell, item } => match item {
                OutputItem::Notify { text } => RuntimeEvent::Notify {
                    cell_id: cell,
                    text,
                },
                OutputItem::Text { text } => RuntimeEvent::Content {
                    cell_id: cell,
                    content: serde_json::json!({"type": "input_text", "text": text}),
                },
                OutputItem::Image { url, .. } => RuntimeEvent::Content {
                    cell_id: cell,
                    content: serde_json::json!({"type": "input_image", "image_url": url}),
                },
                OutputItem::Audio { url } => RuntimeEvent::Content {
                    cell_id: cell,
                    content: serde_json::json!({"type": "input_audio", "audio_url": url}),
                },
            },
            CellEvent::Yielded { cell } => RuntimeEvent::Yielded { cell_id: cell },
            CellEvent::Done {
                outcome: CellOutcome::Stopped { reason },
                ..
            } => {
                return Err(format!(
                    "QuickJS worker returned an SDK foreground stop: {reason:?}"
                ));
            }
            CellEvent::Done {
                cell,
                outcome: CellOutcome::Completed { stored, .. },
            } => RuntimeEvent::Done {
                cell_id: cell,
                stored,
            },
            CellEvent::Done {
                cell,
                outcome:
                    CellOutcome::Failed {
                        message, stored, ..
                    },
            } => RuntimeEvent::Error {
                cell_id: cell,
                message,
                stored,
            },
        })
    }

    async fn terminate(&self) -> Result<Option<ChildProcessUsage>, String> {
        self.0.terminate().await
    }
}

/// A scripted worker in `directory`: it says hello, then runs `body` as `sh`.
/// `frames` are encoded into `frames.bin` beside it for the body to replay.
fn scripted_worker(directory: &Path, body: &str, frames: &[WorkerFrame]) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let hello = WorkerFrame::Hello {
        revision: plakous_cell::CELL_PROTOCOL_REVISION.to_owned(),
        engine: WORKER.engine.to_owned(),
        adapter: WORKER.adapter.to_owned(),
    };
    let hello = plakous_cell::encode_frame(&hello).expect("hello frame");
    let mut after = Vec::new();
    for frame in frames {
        after.extend(plakous_cell::encode_frame(frame).expect("frame"));
    }
    std::fs::write(directory.join("hello.bin"), &hello).expect("hello bytes");
    std::fs::write(directory.join("frames.bin"), &after).expect("frame bytes");
    let program = directory.join("worker.sh");
    let staging = directory.join("worker.pending");
    std::fs::write(
        &staging,
        format!(
            "#!/bin/sh\ncat '{}'\n{body}",
            directory.join("hello.bin").display()
        ),
    )
    .expect("program");
    std::fs::rename(staging, &program).expect("publish program");
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))
        .expect("permissions");
    program
}

#[tokio::test]
async fn sidecar_text_helper_round_trip() {
    let host = Worker::spawn(host_bin()).await.expect("spawn sidecar");
    host.start_cell(
        1,
        r#"text("sidecar-ok")"#,
        HashMap::new(),
        vec![serde_json::json!({
            "name": "noop",
            "tool_name": "noop",
            "kind": "function",
            "description": "noop",
        })],
        DEFAULT_YIELD_TIME_MS,
    )
    .expect("start");
    let mut saw = false;
    loop {
        match host.read_event().await.expect("event") {
            RuntimeEvent::Content { content, .. } => {
                assert_eq!(
                    content.get("text").and_then(Value::as_str),
                    Some("sidecar-ok")
                );
                saw = true;
            }
            RuntimeEvent::Done { .. } => break,
            RuntimeEvent::Error { message, .. } => panic!("{message}"),
            _ => {}
        }
    }
    assert!(saw);
    host.terminate()
        .await
        .expect("sidecar acknowledged shutdown");
}

#[tokio::test]
async fn sidecar_yield_then_wait_then_content() {
    let host = Worker::spawn(host_bin()).await.expect("spawn sidecar");
    host.start_cell(
        2,
        r#"
await yield_control();
text("after-yield");
"#,
        HashMap::new(),
        vec![serde_json::json!({
            "name": "noop",
            "tool_name": "noop",
            "kind": "function",
            "description": "noop",
        })],
        DEFAULT_YIELD_TIME_MS,
    )
    .expect("start");
    let mut yielded = false;
    let mut after = false;
    loop {
        match host.read_event().await.expect("event") {
            RuntimeEvent::Yielded { cell_id } => {
                assert_eq!(cell_id, 2);
                assert!(!after, "content must not precede resume");
                yielded = true;
                host.resume(2, DEFAULT_YIELD_TIME_MS).expect("resume");
            }
            RuntimeEvent::Content { content, .. } => {
                assert!(yielded, "content must follow yield");
                assert_eq!(
                    content.get("text").and_then(Value::as_str),
                    Some("after-yield")
                );
                after = true;
            }
            RuntimeEvent::Done { .. } => break,
            RuntimeEvent::Error { message, .. } => panic!("{message}"),
            RuntimeEvent::ToolCall { .. } | RuntimeEvent::Notify { .. } => {}
        }
    }
    assert!(yielded && after);
    host.terminate()
        .await
        .expect("sidecar acknowledged shutdown");
}

#[tokio::test]
async fn sidecar_nested_tool_then_text() {
    let host = Worker::spawn(host_bin()).await.expect("spawn sidecar");
    host.start_cell(
        3,
        r"const r = await tools.echo({x: 1}); text(String(r.x));",
        HashMap::new(),
        vec![serde_json::json!({
            "name": "echo",
            "tool_name": "echo",
            "kind": "function",
            "description": "echo",
        })],
        DEFAULT_YIELD_TIME_MS,
    )
    .expect("start");
    let mut saw = false;
    loop {
        match host.read_event().await.expect("event") {
            RuntimeEvent::ToolCall {
                id, name, input, ..
            } => {
                assert_eq!(name, "echo");
                assert_eq!(input["x"], 1);
                host.send_tool_result(3, id, serde_json::json!({"x": 1}), true)
                    .await
                    .expect("result");
            }
            RuntimeEvent::Content { content, .. } => {
                assert_eq!(content.get("text").and_then(Value::as_str), Some("1"));
                saw = true;
            }
            RuntimeEvent::Done { .. } => break,
            RuntimeEvent::Error { message, .. } => panic!("{message}"),
            _ => {}
        }
    }
    assert!(saw);
    host.terminate()
        .await
        .expect("sidecar acknowledged shutdown");
}

#[tokio::test]
async fn sidecar_timers_do_not_fire_while_yielded() {
    let host = Worker::spawn(host_bin()).await.expect("spawn sidecar");
    host.start_cell(
        4,
        r#"
setTimeout(() => text("timer"), 1);
await yield_control();
"#,
        HashMap::new(),
        vec![serde_json::json!({
            "name": "noop",
            "tool_name": "noop",
            "kind": "function",
            "description": "noop",
        })],
        DEFAULT_YIELD_TIME_MS,
    )
    .expect("start");
    let mut yielded = false;
    loop {
        match host.read_event().await.expect("event") {
            RuntimeEvent::Yielded { cell_id } => {
                assert_eq!(cell_id, 4);
                yielded = true;
                match tokio::time::timeout(Duration::from_millis(50), host.read_event()).await {
                    Ok(Ok(RuntimeEvent::Content { .. })) => {
                        panic!("timer content must not arrive before resume")
                    }
                    Ok(Ok(other)) => panic!("unexpected {other:?}"),
                    Ok(Err(error)) => panic!("{error}"),
                    Err(_) => {}
                }
                host.resume(4, DEFAULT_YIELD_TIME_MS).expect("resume");
            }
            RuntimeEvent::Content { content, .. } => {
                assert!(yielded, "content must follow resume");
                assert_eq!(content.get("text").and_then(Value::as_str), Some("timer"));
            }
            RuntimeEvent::Done { .. } => break,
            RuntimeEvent::Error { message, .. } => panic!("{message}"),
            RuntimeEvent::ToolCall { .. } | RuntimeEvent::Notify { .. } => {}
        }
    }
    assert!(yielded);
    host.terminate()
        .await
        .expect("sidecar acknowledged shutdown");
}

#[tokio::test]
async fn sidecar_auto_yield_then_resume() {
    let host = Worker::spawn(host_bin()).await.expect("spawn sidecar");
    host.start_cell(
        5,
        r#"
await new Promise((resolve) => setTimeout(resolve, 400));
text("done");
"#,
        HashMap::new(),
        vec![serde_json::json!({
            "name": "noop",
            "tool_name": "noop",
            "kind": "function",
            "description": "noop",
        })],
        50,
    )
    .expect("start");
    let mut yielded = false;
    let mut saw_done = false;
    loop {
        match host.read_event().await.expect("event") {
            RuntimeEvent::Yielded { cell_id } => {
                assert_eq!(cell_id, 5);
                assert!(!saw_done, "content must not precede resume");
                yielded = true;
                host.resume(5, DEFAULT_YIELD_TIME_MS).expect("resume");
            }
            RuntimeEvent::Content { content, .. } => {
                assert!(yielded, "content must follow yield");
                assert_eq!(content.get("text").and_then(Value::as_str), Some("done"));
                saw_done = true;
            }
            RuntimeEvent::Done { .. } => break,
            RuntimeEvent::Error { message, .. } => panic!("{message}"),
            RuntimeEvent::ToolCall { .. } | RuntimeEvent::Notify { .. } => {}
        }
    }
    assert!(yielded && saw_done);
    host.terminate()
        .await
        .expect("sidecar acknowledged shutdown");
}

#[tokio::test]
async fn killing_sidecar_does_not_kill_parent() {
    let host = Worker::spawn(host_bin()).await.expect("spawn sidecar");
    host.terminate()
        .await
        .expect("sidecar acknowledged shutdown");
    let marker = std::env::temp_dir().join(format!(
        "plakous-js-host-parent-alive-{}",
        std::process::id()
    ));
    std::fs::write(&marker, b"alive").expect("parent still writes after child kill");
    let body = std::fs::read(&marker).expect("parent still reads after child kill");
    assert_eq!(body, b"alive");
    let _ = std::fs::remove_file(&marker);
}

/// A worker that never acknowledges `Shutdown` is killed inside the settle
/// bound and reported as a loss that names the sidecar; the caller must not be
/// left waiting on it, and the outcome must not be claimed joined.
#[tokio::test]
async fn sidecar_that_never_acknowledges_shutdown_is_killed_and_reported_lost() {
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    let directory = std::env::temp_dir().join(format!(
        "plakous-js-host-never-ack-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos())
    ));
    std::fs::create_dir_all(&directory).expect("scratch directory");
    let program = scripted_worker(&directory, "exec sleep 60\n", &[]);
    let host = Worker::spawn(&program)
        .await
        .expect("spawn never-ack worker");
    let pid = host.child_id().expect("worker pid");

    let started = Instant::now();
    let loss = host
        .terminate()
        .await
        .expect_err("an unacknowledged shutdown is a loss, not a join");
    let elapsed = started.elapsed();
    assert!(
        loss.contains("cell worker did not acknowledge shutdown"),
        "the loss must name the sidecar: {loss}"
    );
    assert!(
        elapsed >= plakous_cell::WORKER_SHUTDOWN_SETTLE_TIMEOUT
            && elapsed < plakous_cell::WORKER_SHUTDOWN_SETTLE_TIMEOUT + Duration::from_secs(3),
        "terminate must settle inside its bound, took {elapsed:?}"
    );
    assert_eq!(
        nix::sys::wait::waitpid(
            nix::unistd::Pid::from_raw(i32::try_from(pid).expect("pid fits")),
            Some(nix::sys::wait::WaitPidFlag::WNOHANG)
        ),
        Err(nix::errno::Errno::ECHILD),
        "terminate must reap rather than tolerate a zombie"
    );
    let _ = std::fs::remove_dir_all(&directory);
}

#[cfg(unix)]
#[tokio::test]
async fn nonreading_stdin_cannot_block_start_or_cancel() {
    let directory = std::env::temp_dir().join(format!("plak-js-full-pipe-{}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("directory");
    let program = scripted_worker(&directory, "exec sleep 60\n", &[]);
    let host = Worker::spawn(&program).await.expect("host");
    let pid = host.child_id().expect("pid");
    let start = std::time::Instant::now();
    host.start_cell(1, &" ".repeat(256 * 1024), HashMap::new(), vec![], 10_000)
        .expect("bounded write admission");
    assert!(start.elapsed() < Duration::from_secs(1));
    // A liveness bound only: generous, so load never trips it.
    let loss = tokio::time::timeout(Duration::from_secs(30), host.terminate())
        .await
        .expect("cancel deadline");
    assert!(loss.is_err(), "nonreading worker must be killed");
    assert_eq!(
        host.terminate().await,
        loss,
        "cleanup loss must remain sticky"
    );
    assert_eq!(
        nix::sys::wait::waitpid(
            nix::unistd::Pid::from_raw(i32::try_from(pid).expect("pid fits")),
            Some(nix::sys::wait::WaitPidFlag::WNOHANG)
        ),
        Err(nix::errno::Errno::ECHILD)
    );
    std::fs::remove_dir_all(directory).expect("remove directory");
}

#[cfg(unix)]
#[tokio::test]
async fn written_nested_result_is_not_accepted_without_worker_acknowledgement() {
    let directory =
        std::env::temp_dir().join(format!("plak-js-no-result-ack-{}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("directory");
    // The worker reads nothing it is sent: it emits one nested call and, a
    // moment after the result is written, ends without a delivery verdict.
    let program = scripted_worker(
        &directory,
        &format!(
            "cat '{}'\nsleep 0.5\n",
            directory.join("frames.bin").display()
        ),
        &[WorkerFrame::NestedCall {
            cell: 1,
            call: 1,
            name: "echo".into(),
            input: CellValue::Dict(Vec::new()),
        }],
    );
    let host = Worker::spawn(&program).await.expect("host");
    host.start_cell(1, "await tools.echo({})", HashMap::new(), vec![], 10_000)
        .expect("start");
    assert!(matches!(
        host.read_event().await,
        Ok(RuntimeEvent::ToolCall { id: 1, .. })
    ));
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        host.send_tool_result(1, 1, serde_json::json!("exact result"), true),
    )
    .await
    .expect("EOF resolves pending acceptance");
    assert!(
        result.is_err(),
        "writing to stdin is not interpreter acceptance"
    );
    let _ = host.terminate().await;
    std::fs::remove_dir_all(directory).expect("cleanup");
}

// Exercise the same acknowledged settlement contract through both transports.
macro_rules! nested_failure_contract {
    ($name:ident, $spawn:expr) => {
        #[tokio::test]
        async fn $name() {
            for (source, success, expected, fails) in [
                ("text(await tools.echo({}));", true, "exact\r\nUTF-8 é", false),
                ("try { await tools.echo({}); text('unreachable'); } catch (error) { text(error); }", false, "exact\r\nUTF-8 é", false),
                ("text('before'); await tools.echo({}); text('unreachable');", false, "before", true),
                ("const [r] = await Promise.allSettled([tools.echo({})]); text(r.status);", false, "rejected", false),
            ] {
                let host = $spawn.await.expect("spawn");
                host.start_cell(1, source, HashMap::new(), vec![serde_json::json!({
                    "name": "echo", "tool_name": "echo", "kind": "function", "description": "echo"
                })], DEFAULT_YIELD_TIME_MS).expect("start");
                let mut output = Vec::new();
                let mut delivered = false;
                loop {
                    match tokio::time::timeout(Duration::from_secs(5), host.read_event()).await.expect("event deadline").expect("event") {
                        RuntimeEvent::ToolCall { id, .. } => {
                            host.send_tool_result(1, id, Value::String("exact\r\nUTF-8 é".into()), success).await.expect("exact settlement acknowledged");
                            delivered = true;
                        }
                        RuntimeEvent::Content { content, .. } => output.push(content["text"].as_str().expect("text").to_owned()),
                        RuntimeEvent::Done { .. } => { assert!(!fails); break; }
                        RuntimeEvent::Error { message, .. } => {
                            assert!(fails, "unexpected failure: {message}");
                            assert!(message.contains("UTF-8 é"), "{message}");
                            break;
                        }
                        event => panic!("unexpected event: {event:?}"),
                    }
                }
                assert!(delivered);
                assert_eq!(output, [expected]);
                host.terminate().await.expect("cleanup joined");
            }
        }
    };
}

nested_failure_contract!(sidecar_nested_failure_contract, Worker::spawn(host_bin()));
nested_failure_contract!(embedded_nested_failure_contract, async {
    plakous_js_runtime::EmbeddedHost::spawn()
});

// One pending call stays below every resource bound. Hold its result until an
// observed cell boundary, so neither outcome depends on a sleep or scheduling.
macro_rules! finite_single_delayed_call_contract {
    ($name:ident, $spawn:expr) => {
        #[tokio::test]
        async fn $name() {
            for awaited in [false, true] {
                tokio::time::timeout(Duration::from_secs(5), async {
                    let host = $spawn.await.expect("spawn");
                    let source = if awaited {
                        "const pending = tools.echo({x: 1}); await yield_control(); text(await pending);"
                    } else {
                        "tools.echo({x: 1});"
                    };
                    host.start_cell(
                        1,
                        source,
                        HashMap::new(),
                        vec![serde_json::json!({
                            "name": "echo", "tool_name": "echo", "kind": "function"
                        })],
                        DEFAULT_YIELD_TIME_MS,
                    )
                    .expect("start");
                    let RuntimeEvent::ToolCall {
                        cell_id, id, name, input,
                    } = host.read_event().await.expect("single nested call") else {
                        panic!("expected the single nested call first");
                    };
                    assert_eq!(cell_id, 1);
                    assert_eq!(name, "echo");
                    assert_eq!(input, serde_json::json!({"x": 1}));

                    // Unawaited JS finishes its script before settlement, but the
                    // host keeps the execution alive until every nested promise
                    // is acknowledged.
                    if awaited {
                        let boundary = host.read_event().await.expect("cell boundary");
                        assert!(matches!(boundary, RuntimeEvent::Yielded { cell_id: 1 }));
                    }
                    host.send_tool_result(1, id, Value::String("settled-once".into()), true)
                        .await
                        .expect("outstanding tool settles before the cell terminal");
                    if awaited {
                        host.resume(1, DEFAULT_YIELD_TIME_MS).expect("resume");
                        let RuntimeEvent::Content { cell_id, content } =
                            host.read_event().await.expect("settled output")
                        else {
                            panic!("expected awaited result output");
                        };
                        assert_eq!(cell_id, 1);
                        assert_eq!(content["text"], "settled-once");
                    }
                    assert!(matches!(
                        host.read_event().await.expect("one terminal"),
                        RuntimeEvent::Done { cell_id: 1, .. }
                    ));
                    host.terminate().await.expect("owned cleanup joined");
                    assert!(
                        host.read_event().await.is_err(),
                        "no second call, output or duplicate terminal remains after join"
                    );
                })
                .await
                .expect("delayed settlement and cleanup must not deadlock");
            }
        }
    };
}

macro_rules! parallel_promise_all_defers_terminal_until_settled {
    ($name:ident, $spawn:expr) => {
        #[tokio::test]
        async fn $name() {
            tokio::time::timeout(Duration::from_secs(5), async {
            let host = $spawn.await.expect("spawn");
            let source = "try { await Promise.all([tools.echo({a:1}), tools.echo({b:2}).then(v => { text('late'); store('late', v); return v; })]); text('ok'); } catch { text('err'); store('caught', 1); }";
            host.start_cell(
                1,
                source,
                HashMap::new(),
                vec![serde_json::json!({
                    "name": "echo", "tool_name": "echo", "kind": "function", "description": "echo"
                })],
                DEFAULT_YIELD_TIME_MS,
            )
            .expect("start");
            let mut ids = Vec::new();
            for _ in 0..2 {
                let RuntimeEvent::ToolCall { id, .. } =
                    host.read_event().await.expect("parallel nested call")
                else {
                    panic!("expected two nested calls");
                };
                ids.push(id);
            }
            host.send_tool_result(1, ids[0], Value::String("denied".into()), false)
                .await
                .expect("first outstanding tool rejects");
            let RuntimeEvent::Content { cell_id, content } =
                host.read_event().await.expect("failure output")
            else {
                panic!("expected failure output");
            };
            assert_eq!(cell_id, 1);
            assert_eq!(content["text"], "err");
            host.send_tool_result(1, ids[1], serde_json::json!(41), true)
                .await
                .expect("second outstanding tool settles before the cell terminal");
            let RuntimeEvent::Done { cell_id, stored } =
                host.read_event().await.expect("terminal")
            else {
                panic!("late callback executed after script completion");
            };
            assert_eq!(cell_id, 1);
            assert_eq!(stored, HashMap::from([("caught".into(), serde_json::json!(1))]));
            host.terminate().await.expect("cleanup joined");
            })
            .await
            .expect("fail-fast settlement is bounded");
        }
    };
}

parallel_promise_all_defers_terminal_until_settled!(
    embedded_parallel_promise_all_defers_terminal,
    async { plakous_js_runtime::EmbeddedHost::spawn() }
);
parallel_promise_all_defers_terminal_until_settled!(
    sidecar_parallel_promise_all_defers_terminal,
    Worker::spawn(host_bin())
);

macro_rules! late_callbacks_do_not_survive_script_completion {
    ($name:ident, $spawn:expr, $source:literal, $success:literal, $expected_error:expr) => {
        #[tokio::test]
        async fn $name() {
            tokio::time::timeout(Duration::from_secs(5), async {
                let host = $spawn.await.expect("spawn");
                host.start_cell(
                    1,
                    $source,
                    HashMap::new(),
                    vec![serde_json::json!({
                        "name": "echo", "tool_name": "echo", "kind": "function"
                    })],
                    0,
                )
                .expect("start");
                let RuntimeEvent::ToolCall { cell_id, id, .. } =
                    host.read_event().await.expect("emitted request")
                else {
                    panic!("expected one tool request");
                };
                assert_eq!(cell_id, 1);
                assert!(matches!(
                    host.read_event().await.expect("auto-yield while settling (deterministic: yield_time_ms is 0)"),
                    RuntimeEvent::Yielded { cell_id: 1 }
                ));
                host.resume(1, DEFAULT_YIELD_TIME_MS).expect("resume");
                host.send_tool_result(1, id, serde_json::json!(41), $success)
                    .await
                    .expect("result accepted without reopening script");
                let expected_error: Option<&str> = $expected_error;
                let stored = match (
                    host.read_event().await.expect("frozen terminal"),
                    expected_error,
                ) {
                    (RuntimeEvent::Done { cell_id, stored }, None) => {
                        assert_eq!(cell_id, 1);
                        stored
                    }
                    (
                        RuntimeEvent::Error {
                            cell_id,
                            message,
                            stored,
                        },
                        Some(expected),
                    ) => {
                        assert_eq!(cell_id, 1);
                        assert!(message.contains(expected), "{message}");
                        stored
                    }
                    (event, expected) => {
                        panic!("unexpected terminal {event:?} for expected error {expected:?}")
                    }
                };
                assert_eq!(stored, HashMap::from([("before".into(), serde_json::json!(7))]));
                host.start_cell(
                    2,
                    "text(load('before')); text(load('late'));",
                    stored,
                    Vec::new(),
                    DEFAULT_YIELD_TIME_MS,
                )
                .expect("next cell");
                for expected in ["7", "undefined"] {
                    let RuntimeEvent::Content { cell_id, content } =
                        host.read_event().await.expect("next-cell output")
                    else {
                        panic!("old cell work leaked into next cell");
                    };
                    assert_eq!(cell_id, 2);
                    assert_eq!(content["text"], expected);
                }
                assert!(matches!(
                    host.read_event().await.expect("next-cell terminal"),
                    RuntimeEvent::Done { cell_id: 2, .. }
                ));
                host.terminate().await.expect("joined cleanup");
                assert!(host.read_event().await.is_err());
            })
            .await
            .expect("cell settlement and retirement are bounded");
        }
    };
}

late_callbacks_do_not_survive_script_completion!(
    embedded_late_callbacks_are_discarded,
    async { plakous_js_runtime::EmbeddedHost::spawn() },
    "store('before', 7); tools.echo({}).then(v => { store('late', v); text('late'); tools.echo({late:true}); }); setTimeout(() => text('late timer'), 0);",
    true,
    None
);
late_callbacks_do_not_survive_script_completion!(
    sidecar_late_callbacks_are_discarded,
    Worker::spawn(host_bin()),
    "store('before', 7); tools.echo({}).then(v => { store('late', v); text('late'); tools.echo({late:true}); }); setTimeout(() => text('late timer'), 0);",
    true,
    None
);
late_callbacks_do_not_survive_script_completion!(
    embedded_late_rejection_callbacks_are_discarded,
    async { plakous_js_runtime::EmbeddedHost::spawn() },
    "store('before', 7); tools.echo({}).catch(() => { store('late', 1); text('late'); tools.echo({late:true}); });",
    false,
    None
);
late_callbacks_do_not_survive_script_completion!(
    sidecar_late_rejection_callbacks_are_discarded,
    Worker::spawn(host_bin()),
    "store('before', 7); tools.echo({}).catch(() => { store('late', 1); text('late'); tools.echo({late:true}); });",
    false,
    None
);
late_callbacks_do_not_survive_script_completion!(
    embedded_late_callbacks_after_exit_are_discarded,
    async { plakous_js_runtime::EmbeddedHost::spawn() },
    "store('before', 7); tools.echo({}).then(() => text('late')); exit(); text('unreachable');",
    true,
    None
);
late_callbacks_do_not_survive_script_completion!(
    sidecar_late_callbacks_after_exit_are_discarded,
    Worker::spawn(host_bin()),
    "store('before', 7); tools.echo({}).then(() => text('late')); exit(); text('unreachable');",
    true,
    None
);
late_callbacks_do_not_survive_script_completion!(
    embedded_late_callbacks_after_error_are_discarded,
    async { plakous_js_runtime::EmbeddedHost::spawn() },
    "store('before', 7); tools.echo({}).then(() => text('late')); throw new Error('expected script failure');",
    true,
    Some("expected script failure")
);
late_callbacks_do_not_survive_script_completion!(
    sidecar_late_callbacks_after_error_are_discarded,
    Worker::spawn(host_bin()),
    "store('before', 7); tools.echo({}).then(() => text('late')); throw new Error('expected script failure');",
    true,
    Some("expected script failure")
);

/// A settling cell keeps honouring `wait`: every resume re-arms the yield
/// deadline and yields again, so a consumer's `wait` stays bounded by its
/// `yield_time_ms` instead of blocking until the nested request settles.
macro_rules! settling_cell_yields_again_on_every_resume {
    ($name:ident, $spawn:expr) => {
        #[tokio::test]
        async fn $name() {
            tokio::time::timeout(Duration::from_secs(5), async {
                let host = $spawn.await.expect("spawn");
                host.start_cell(
                    1,
                    "store('before', 7); tools.echo({});",
                    HashMap::new(),
                    vec![serde_json::json!({
                        "name": "echo", "tool_name": "echo", "kind": "function"
                    })],
                    0,
                )
                .expect("start");
                let RuntimeEvent::ToolCall { cell_id, id, .. } =
                    host.read_event().await.expect("emitted request")
                else {
                    panic!("expected one tool request");
                };
                assert_eq!(cell_id, 1);
                for round in 0..3 {
                    assert!(
                        matches!(
                            host.read_event().await.expect("yield"),
                            RuntimeEvent::Yielded { cell_id: 1 }
                        ),
                        "settling cell must yield again on resume round {round}"
                    );
                    host.resume(1, 0).expect("resume");
                }
                assert!(matches!(
                    host.read_event().await.expect("yield after final resume"),
                    RuntimeEvent::Yielded { cell_id: 1 }
                ));
                host.send_tool_result(1, id, serde_json::json!(41), true)
                    .await
                    .expect("result accepted");
                let RuntimeEvent::Done { cell_id, stored } =
                    host.read_event().await.expect("terminal")
                else {
                    panic!("expected the frozen terminal");
                };
                assert_eq!(cell_id, 1);
                assert_eq!(stored, HashMap::from([("before".into(), serde_json::json!(7))]));
                host.terminate().await.expect("joined cleanup");
            })
            .await
            .expect("resumes on a settling cell are bounded");
        }
    };
}

settling_cell_yields_again_on_every_resume!(
    embedded_settling_cell_yields_again_on_every_resume,
    async { plakous_js_runtime::EmbeddedHost::spawn() }
);
settling_cell_yields_again_on_every_resume!(
    sidecar_settling_cell_yields_again_on_every_resume,
    Worker::spawn(host_bin())
);

/// The command queue holds a full nested-call budget delivered at once.
///
/// The driver polls every pending delivery concurrently, so a profile that
/// admits sixteen outstanding nested calls can settle sixteen results in one
/// burst while the cell is parked and the engine drains nothing until the
/// sends are already queued. Every one of them is a genuine pending request of
/// the owning cell, so every one must be accepted: a full queue at this depth
/// would report evaluator loss for results the evaluator is perfectly able to
/// take. Spawning the deliveries and joining them issues all sixteen sends
/// before the first acknowledgement is awaited, which is the burst.
macro_rules! a_full_nested_budget_delivered_at_once_is_accepted {
    ($name:ident, $spawn:expr) => {
        #[tokio::test]
        async fn $name() {
            const BURST: usize = 16;
            tokio::time::timeout(Duration::from_secs(20), async {
                let host = std::sync::Arc::new($spawn.await.expect("spawn"));
                host.start_cell(
                    1,
                    "const calls = [];
                     for (let i = 0; i < 16; i += 1) { calls.push(tools.echo({i})); }
                     const values = await Promise.all(calls);
                     store('answered', values.length);
                     text(String(values.length));",
                    HashMap::new(),
                    vec![serde_json::json!({
                        "name": "echo", "tool_name": "echo", "kind": "function"
                    })],
                    DEFAULT_YIELD_TIME_MS,
                )
                .expect("start");
                let mut ids = Vec::new();
                while ids.len() < BURST {
                    match host.read_event().await.expect("emitted request") {
                        RuntimeEvent::ToolCall { cell_id, id, .. } => {
                            assert_eq!(cell_id, 1);
                            ids.push(id);
                        }
                        RuntimeEvent::Yielded { cell_id } => {
                            host.resume(cell_id, DEFAULT_YIELD_TIME_MS).expect("resume");
                        }
                        RuntimeEvent::Done { .. } => {
                            panic!("the cell completed before its requests were answered")
                        }
                        RuntimeEvent::Error { message, .. } => panic!("{message}"),
                        RuntimeEvent::Content { .. } | RuntimeEvent::Notify { .. } => {}
                    }
                }
                let mut deliveries = tokio::task::JoinSet::new();
                for id in ids {
                    let host = std::sync::Arc::clone(&host);
                    deliveries.spawn(async move {
                        host.deliver_tool_result(1, id, serde_json::json!(id), true)
                            .await
                    });
                }
                let mut accepted = 0;
                while let Some(joined) = deliveries.join_next().await {
                    assert_eq!(
                        joined.expect("delivery task"),
                        NestedDeliveryOutcome::Accepted,
                        "a full-budget burst is neither a refusal nor evaluator loss"
                    );
                    accepted += 1;
                }
                assert_eq!(
                    accepted, BURST,
                    "every result in a full-budget burst must be accepted"
                );
                let mut answered = None;
                let mut terminals = 0;
                let stored = loop {
                    match host.read_event().await.expect("terminal") {
                        RuntimeEvent::Content { content, .. } => {
                            answered = content
                                .get("text")
                                .and_then(Value::as_str)
                                .map(str::to_owned);
                        }
                        RuntimeEvent::Yielded { cell_id } => {
                            host.resume(cell_id, DEFAULT_YIELD_TIME_MS).expect("resume");
                        }
                        RuntimeEvent::Done { cell_id, stored } => {
                            assert_eq!(cell_id, 1);
                            terminals += 1;
                            break stored;
                        }
                        RuntimeEvent::Error { message, .. } => panic!("{message}"),
                        RuntimeEvent::ToolCall { .. } | RuntimeEvent::Notify { .. } => {}
                    }
                };
                assert_eq!(terminals, 1, "the cell completes once");
                assert_eq!(answered.as_deref(), Some("16"));
                assert_eq!(
                    stored,
                    HashMap::from([("answered".into(), serde_json::json!(16))])
                );
                host.terminate().await.expect("joined cleanup");
            })
            .await
            .expect("a full-budget burst settles without evaluator loss");
        }
    };
}

a_full_nested_budget_delivered_at_once_is_accepted!(
    embedded_a_full_nested_budget_delivered_at_once_is_accepted,
    async { plakous_js_runtime::EmbeddedHost::spawn() }
);
a_full_nested_budget_delivered_at_once_is_accepted!(
    sidecar_a_full_nested_budget_delivered_at_once_is_accepted,
    Worker::spawn(host_bin())
);

macro_rules! awaited_storage_propagates_to_next_cell {
    ($name:ident, $spawn:expr) => {
        #[tokio::test]
        async fn $name() {
            tokio::time::timeout(Duration::from_secs(5), async {
            let host = $spawn.await.expect("spawn");
            host.start_cell(
                1,
                "const v = await tools.echo({}); store('before', v); text(v);",
                HashMap::new(),
                vec![serde_json::json!({
                    "name": "echo", "tool_name": "echo", "kind": "function"
                })],
                DEFAULT_YIELD_TIME_MS,
            )
            .expect("start");
            let RuntimeEvent::ToolCall { cell_id, id, .. } =
                host.read_event().await.expect("awaited request")
            else {
                panic!("expected one tool request");
            };
            assert_eq!(cell_id, 1);
            host.send_tool_result(1, id, serde_json::json!(7), true)
                .await
                .expect("result accepted");
            let RuntimeEvent::Content { cell_id, content } =
                host.read_event().await.expect("awaited output")
            else {
                panic!("expected awaited output");
            };
            assert_eq!(cell_id, 1);
            assert_eq!(content["text"], "7");
            let RuntimeEvent::Done { cell_id, stored } =
                host.read_event().await.expect("first terminal")
            else {
                panic!("expected first cell completion");
            };
            assert_eq!(cell_id, 1);
            assert_eq!(stored, HashMap::from([("before".into(), serde_json::json!(7))]));
            host.start_cell(
                2,
                "text(load('before'));",
                stored,
                Vec::new(),
                DEFAULT_YIELD_TIME_MS,
            )
            .expect("next cell");
            let RuntimeEvent::Content { cell_id, content } =
                host.read_event().await.expect("next-cell output")
            else {
                panic!("expected next-cell output");
            };
            assert_eq!(cell_id, 2);
            assert_eq!(content["text"], "7");
            assert!(matches!(
                host.read_event().await.expect("next-cell terminal"),
                RuntimeEvent::Done { cell_id: 2, .. }
            ));
            host.terminate().await.expect("joined cleanup");
            assert!(host.read_event().await.is_err());
            })
            .await
            .expect("awaited storage and cleanup are bounded");
        }
    };
}

awaited_storage_propagates_to_next_cell!(embedded_awaited_storage_propagates_to_next_cell, async {
    plakous_js_runtime::EmbeddedHost::spawn()
});
awaited_storage_propagates_to_next_cell!(
    sidecar_awaited_storage_propagates_to_next_cell,
    Worker::spawn(host_bin())
);

finite_single_delayed_call_contract!(embedded_finite_single_delayed_call_contract, async {
    plakous_js_runtime::EmbeddedHost::spawn()
});
finite_single_delayed_call_contract!(
    sidecar_finite_single_delayed_call_contract,
    Worker::spawn(host_bin())
);

/// An unawaited request that is still outstanding when the script completes
/// leaves the cell in settlement. `interrupt()` is a documented no-op there, so
/// hard termination is the only control; it must produce exactly one outcome.
macro_rules! terminate_during_settlement_has_one_outcome {
    ($name:ident, $spawn:expr) => {
        #[tokio::test]
        async fn $name() {
            tokio::time::timeout(Duration::from_secs(5), async {
                let host = $spawn.await.expect("spawn");
                host.start_cell(
                    1,
                    "store('before', 7); tools.echo({});",
                    HashMap::new(),
                    vec![serde_json::json!({
                        "name": "echo", "tool_name": "echo", "kind": "function"
                    })],
                    0,
                )
                .expect("start");
                let RuntimeEvent::ToolCall { cell_id, id, .. } =
                    host.read_event().await.expect("emitted request")
                else {
                    panic!("expected one tool request");
                };
                assert_eq!(cell_id, 1);
                assert!(matches!(
                    host.read_event().await.expect("auto-yield while settling (deterministic: yield_time_ms is 0)"),
                    RuntimeEvent::Yielded { cell_id: 1 }
                ));
                host.interrupt();
                let _ = host.terminate().await;
                // Either the transport is lost or it closes quietly, but the
                // settling cell must never also publish a terminal.
                loop {
                    match host.read_event().await {
                        Err(_) => break,
                        Ok(event @ (RuntimeEvent::Done { .. } | RuntimeEvent::Error { .. })) => {
                            panic!("terminated settlement still produced {event:?}")
                        }
                        Ok(_) => {}
                    }
                }
                assert!(
                    host.send_tool_result(1, id, Value::String("too late".into()), true)
                        .await
                        .is_err(),
                    "a result for a terminated cell must be refused"
                );
            })
            .await
            .expect("terminating a settling cell is bounded");
        }
    };
}

terminate_during_settlement_has_one_outcome!(
    embedded_terminate_during_settlement_has_one_outcome,
    async { plakous_js_runtime::EmbeddedHost::spawn() }
);
terminate_during_settlement_has_one_outcome!(
    sidecar_terminate_during_settlement_has_one_outcome,
    Worker::spawn(host_bin())
);

/// Core cancellation reaches the evaluator as a rejection-shaped result. It is
/// acknowledged like any other settlement, and it releases the frozen terminal
/// with the storage the script had before it completed.
macro_rules! cancelled_result_settles_frozen_terminal {
    ($name:ident, $spawn:expr) => {
        #[tokio::test]
        async fn $name() {
            tokio::time::timeout(Duration::from_secs(5), async {
                let host = $spawn.await.expect("spawn");
                host.start_cell(
                    1,
                    "store('before', 7); tools.echo({}).catch((reason) => { store('late', reason); text('late'); });",
                    HashMap::new(),
                    vec![serde_json::json!({
                        "name": "echo", "tool_name": "echo", "kind": "function"
                    })],
                    0,
                )
                .expect("start");
                let RuntimeEvent::ToolCall { cell_id, id, .. } =
                    host.read_event().await.expect("emitted request")
                else {
                    panic!("expected one tool request");
                };
                assert_eq!(cell_id, 1);
                assert!(matches!(
                    host.read_event().await.expect("auto-yield while settling (deterministic: yield_time_ms is 0)"),
                    RuntimeEvent::Yielded { cell_id: 1 }
                ));
                host.send_tool_result(1, id, Value::String("cancelled".into()), false)
                    .await
                    .expect("a cancellation is acknowledged like any settlement");
                let RuntimeEvent::Done { cell_id, stored } =
                    host.read_event().await.expect("frozen terminal")
                else {
                    panic!("a cancelled nested call must not reopen the script");
                };
                assert_eq!(cell_id, 1);
                assert_eq!(stored, HashMap::from([("before".into(), serde_json::json!(7))]));
                host.terminate().await.expect("cleanup joined");
                assert!(
                    host.read_event().await.is_err(),
                    "no late output follows the frozen terminal"
                );
            })
            .await
            .expect("cancelled settlement is bounded");
        }
    };
}

cancelled_result_settles_frozen_terminal!(
    embedded_cancelled_result_settles_frozen_terminal,
    async { plakous_js_runtime::EmbeddedHost::spawn() }
);
cancelled_result_settles_frozen_terminal!(
    sidecar_cancelled_result_settles_frozen_terminal,
    Worker::spawn(host_bin())
);

/// One request id settles once. A repeated acknowledgement is refused and adds
/// no second terminal to the cell that already completed.
macro_rules! duplicate_result_is_refused_after_one_terminal {
    ($name:ident, $spawn:expr) => {
        #[tokio::test]
        async fn $name() {
            tokio::time::timeout(Duration::from_secs(5), async {
                let host = $spawn.await.expect("spawn");
                host.start_cell(
                    1,
                    "store('before', 7); tools.echo({});",
                    HashMap::new(),
                    vec![serde_json::json!({
                        "name": "echo", "tool_name": "echo", "kind": "function"
                    })],
                    0,
                )
                .expect("start");
                let RuntimeEvent::ToolCall { cell_id, id, .. } =
                    host.read_event().await.expect("emitted request")
                else {
                    panic!("expected one tool request");
                };
                assert_eq!(cell_id, 1);
                assert!(matches!(
                    host.read_event().await.expect("auto-yield while settling (deterministic: yield_time_ms is 0)"),
                    RuntimeEvent::Yielded { cell_id: 1 }
                ));
                host.send_tool_result(1, id, serde_json::json!(41), true)
                    .await
                    .expect("the genuine result is accepted");
                let RuntimeEvent::Done { cell_id, stored } =
                    host.read_event().await.expect("single terminal")
                else {
                    panic!("expected the frozen terminal");
                };
                assert_eq!(cell_id, 1);
                assert_eq!(stored, HashMap::from([("before".into(), serde_json::json!(7))]));
                assert!(
                    host.send_tool_result(1, id, serde_json::json!(41), true)
                        .await
                        .is_err(),
                    "a settled request id must not be acknowledged twice"
                );
                // A refused duplicate adds nothing: the one terminal above was
                // the cell's only one, and the host joins with nothing queued.
                host.terminate().await.expect("joined cleanup");
                assert!(host.read_event().await.is_err());
            })
            .await
            .expect("duplicate acknowledgement is bounded");
        }
    };
}

duplicate_result_is_refused_after_one_terminal!(
    embedded_duplicate_result_is_refused_after_one_terminal,
    async { plakous_js_runtime::EmbeddedHost::spawn() }
);
duplicate_result_is_refused_after_one_terminal!(
    sidecar_duplicate_result_is_refused_after_one_terminal,
    Worker::spawn(host_bin())
);

/// A result the cell never asked for - an unissued id, or a foreign cell id -
/// is refused to its sender without disturbing the outstanding request the
/// cell does own. Only a failure to settle a genuine pending promise is a
/// host loss; the sidecar carries the refusal as its own protocol frame.
macro_rules! foreign_result_is_refused_without_disturbing_the_cell {
    ($name:ident, $spawn:expr) => {
        #[tokio::test]
        async fn $name() {
            tokio::time::timeout(Duration::from_secs(5), async {
                let host = $spawn.await.expect("spawn");
                host.start_cell(
                    1,
                    "store('before', 7); tools.echo({});",
                    HashMap::new(),
                    vec![serde_json::json!({
                        "name": "echo", "tool_name": "echo", "kind": "function"
                    })],
                    0,
                )
                .expect("start");
                let RuntimeEvent::ToolCall { cell_id, id, .. } =
                    host.read_event().await.expect("emitted request")
                else {
                    panic!("expected one tool request");
                };
                assert_eq!(cell_id, 1);
                assert!(matches!(
                    host.read_event().await.expect("auto-yield while settling (deterministic: yield_time_ms is 0)"),
                    RuntimeEvent::Yielded { cell_id: 1 }
                ));
                assert!(
                    host.send_tool_result(9, id, serde_json::json!(1), true)
                        .await
                        .is_err(),
                    "a result for a foreign cell must be refused"
                );
                assert!(
                    host.send_tool_result(1, id.saturating_add(1000), serde_json::json!(1), true)
                        .await
                        .is_err(),
                    "a result for an unissued request must be refused"
                );
                host.send_tool_result(1, id, serde_json::json!(41), true)
                    .await
                    .expect("the genuine result still settles the cell");
                let RuntimeEvent::Done { cell_id, stored } =
                    host.read_event().await.expect("terminal")
                else {
                    panic!("a refused foreign result must not disturb the cell");
                };
                assert_eq!(cell_id, 1);
                assert_eq!(stored, HashMap::from([("before".into(), serde_json::json!(7))]));
                host.terminate().await.expect("cleanup joined");
                assert!(host.read_event().await.is_err());
            })
            .await
            .expect("foreign acknowledgement is bounded");
        }
    };
}

foreign_result_is_refused_without_disturbing_the_cell!(
    embedded_foreign_result_is_refused_without_disturbing_the_cell,
    async { plakous_js_runtime::EmbeddedHost::spawn() }
);
foreign_result_is_refused_without_disturbing_the_cell!(
    sidecar_foreign_result_is_refused_without_disturbing_the_cell,
    Worker::spawn(host_bin())
);

/// Every cell gets a fresh `QuickJS` runtime: neither a global written by the
/// previous cell nor a timer it left behind can reach the next one.
macro_rules! each_cell_gets_a_fresh_runtime {
    ($name:ident, $spawn:expr) => {
        #[tokio::test]
        async fn $name() {
            tokio::time::timeout(Duration::from_secs(5), async {
                let host = $spawn.await.expect("spawn");
                host.start_cell(
                    1,
                    "globalThis.leak = 1; setTimeout(() => text('cell1 timer'), 0); tools.echo({});",
                    HashMap::new(),
                    vec![serde_json::json!({
                        "name": "echo", "tool_name": "echo", "kind": "function"
                    })],
                    0,
                )
                .expect("start");
                let RuntimeEvent::ToolCall { cell_id, id, .. } =
                    host.read_event().await.expect("emitted request")
                else {
                    panic!("expected one tool request");
                };
                assert_eq!(cell_id, 1);
                assert!(matches!(
                    host.read_event().await.expect("auto-yield while settling (deterministic: yield_time_ms is 0)"),
                    RuntimeEvent::Yielded { cell_id: 1 }
                ));
                host.send_tool_result(1, id, serde_json::json!(41), true)
                    .await
                    .expect("result accepted");
                assert!(matches!(
                    host.read_event().await.expect("first terminal"),
                    RuntimeEvent::Done { cell_id: 1, .. }
                ));
                host.start_cell(
                    2,
                    "text(typeof globalThis.leak); text('cell2');",
                    HashMap::new(),
                    Vec::new(),
                    DEFAULT_YIELD_TIME_MS,
                )
                .expect("next cell");
                for expected in ["undefined", "cell2"] {
                    let RuntimeEvent::Content { cell_id, content } =
                        host.read_event().await.expect("next-cell output")
                    else {
                        panic!("the retired cell leaked work into the fresh runtime");
                    };
                    assert_eq!(cell_id, 2);
                    assert_eq!(content["text"], expected);
                }
                assert!(matches!(
                    host.read_event().await.expect("next-cell terminal"),
                    RuntimeEvent::Done { cell_id: 2, .. }
                ));
                host.terminate().await.expect("cleanup joined");
                assert!(host.read_event().await.is_err());
            })
            .await
            .expect("per-cell runtime isolation is bounded");
        }
    };
}

each_cell_gets_a_fresh_runtime!(embedded_each_cell_gets_a_fresh_runtime, async {
    plakous_js_runtime::EmbeddedHost::spawn()
});
each_cell_gets_a_fresh_runtime!(
    sidecar_each_cell_gets_a_fresh_runtime,
    Worker::spawn(host_bin())
);

/// A cell that sends `echo` an object nested `depth` levels and prints how
/// the call ended.
fn deep_call_cell(depth: usize) -> String {
    format!(
        "let v = 1; for (let i = 0; i < {depth}; i++) v = {{a: v}};
try {{ const r = await tools.echo(v); text('ok:' + r); }}
catch (e) {{ text('refused:' + String((e && e.message) || e)); }}"
    )
}

#[tokio::test]
async fn nested_inputs_cross_up_to_the_value_depth_and_deeper_ones_are_refused_in_cell() {
    let host = Worker::spawn(host_bin()).await.expect("spawn sidecar");
    let echo = vec![serde_json::json!({
        "name": "echo", "tool_name": "echo", "kind": "function", "description": "echo"
    })];
    for (cell, depth) in [(1_u64, 40_usize), (2, 50)] {
        host.start_cell(
            cell,
            &deep_call_cell(depth),
            HashMap::new(),
            echo.clone(),
            DEFAULT_YIELD_TIME_MS,
        )
        .expect("start");
        let mut printed = None;
        loop {
            match host.read_event().await.expect("the worker survives") {
                RuntimeEvent::ToolCall { id, input, .. } => {
                    assert_eq!(depth, 40, "a too-deep input must not reach the host");
                    assert_eq!(plakous_cell::json_depth(&input), 40);
                    host.send_tool_result(cell, id, serde_json::json!(7), true)
                        .await
                        .expect("result");
                }
                RuntimeEvent::Content { content, .. } => {
                    printed = content["text"].as_str().map(str::to_owned);
                }
                RuntimeEvent::Done { .. } => break,
                RuntimeEvent::Error { message, .. } => panic!("{message}"),
                _ => {}
            }
        }
        let printed = printed.expect("the cell reports its call");
        if depth == 40 {
            assert_eq!(printed, "ok:7");
        } else {
            assert!(printed.contains("nests deeper than 40 levels"), "{printed}");
        }
    }
    host.terminate()
        .await
        .expect("sidecar acknowledged shutdown");
}

/// Reads the worker's frames until it exits, then its exit status.
fn worker_exchange(frames: &[plakous_cell::HostFrame]) -> (Vec<WorkerFrame>, bool) {
    use std::io::Write as _;
    let mut child = std::process::Command::new(host_bin())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("worker starts");
    let mut stdin = child.stdin.take().expect("stdin");
    for frame in frames {
        stdin
            .write_all(&plakous_cell::encode_frame(frame).expect("frame"))
            .expect("write frame");
    }
    let mut stdout = child.stdout.take().expect("stdout");
    let mut received = Vec::new();
    while let Some(frame) =
        plakous_cell::read_frame_blocking(&mut stdout, plakous_cell::MAX_FRAME_BYTES)
            .expect("frame")
    {
        received.push(serde_json::from_slice(&frame).expect("worker frame"));
    }
    drop(stdin);
    let status = child.wait().expect("worker exits");
    (received, status.success())
}

#[test]
fn the_worker_refuses_other_limits_and_a_repeated_configure() {
    let configure = |engine| plakous_cell::HostFrame::Configure {
        sdk_stubs: None,
        revision: plakous_cell::CELL_PROTOCOL_REVISION.to_owned(),
        engine,
    };
    let other = EngineConfig::Quickjs {
        heap_bytes: 128 * 1024 * 1024,
        stack_bytes: 512 * 1024,
    };
    for frames in [
        vec![configure(other)],
        vec![
            configure(EngineConfig::QUICKJS),
            configure(EngineConfig::QUICKJS),
        ],
    ] {
        let (received, success) = worker_exchange(&frames);
        assert!(matches!(received.first(), Some(WorkerFrame::Hello { .. })));
        assert!(
            matches!(
                received.last(),
                Some(WorkerFrame::HostLost {
                    class: plakous_cell::LossClass::ProtocolViolation,
                    ..
                })
            ),
            "{received:?}"
        );
        assert!(!success, "a refused configure ends the worker with failure");
    }
}
