//! The Python worker's process boundary: the cell protocol's hello, its
//! checked configure, clean end of input and shutdown.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "process contract assertions"
)]

use std::{
    io::Write,
    process::{Command, Stdio},
};

use plakous_cell::{
    BindingInput, CELL_PROTOCOL_REVISION, CellClient, CellEvent, CellOutcome, CellPrelude,
    EngineConfig, HostFrame, LossClass, MAX_FRAME_BYTES, MONTY_WORKER, MontyBinding,
    MontyBootstrap, NestedDeliveryOutcome, WorkerFrame, encode_frame, read_frame_blocking,
};

/// Writes `frames`, closes stdin, and returns every frame the worker wrote
/// and whether it exited successfully.
fn exchange(frames: &[HostFrame]) -> (Vec<WorkerFrame>, bool) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_plakous-py-host"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("worker starts");
    let mut stdin = child.stdin.take().expect("stdin");
    for frame in frames {
        stdin
            .write_all(&encode_frame(frame).expect("frame"))
            .expect("write frame");
    }
    drop(stdin);
    let mut stdout = child.stdout.take().expect("stdout");
    let mut received = Vec::new();
    while let Some(frame) = read_frame_blocking(&mut stdout, MAX_FRAME_BYTES).expect("frame") {
        received.push(serde_json::from_slice(&frame).expect("worker frame"));
    }
    (received, child.wait().expect("worker exits").success())
}

fn configure(engine: EngineConfig) -> HostFrame {
    HostFrame::Configure {
        revision: CELL_PROTOCOL_REVISION.to_owned(),
        engine,
    }
}

#[test]
fn the_worker_says_hello_and_ends_cleanly_with_its_input() {
    let (received, success) = exchange(&[configure(EngineConfig::MONTY)]);
    assert_eq!(
        received,
        [WorkerFrame::Hello {
            revision: CELL_PROTOCOL_REVISION.to_owned(),
            engine: MONTY_WORKER.engine.to_owned(),
            adapter: MONTY_WORKER.adapter.to_owned(),
        }]
    );
    assert!(success);
}

#[test]
fn shutdown_reports_usage_then_exits() {
    let (received, success) = exchange(&[configure(EngineConfig::MONTY), HostFrame::Shutdown]);
    assert!(matches!(
        received.as_slice(),
        [
            WorkerFrame::Hello { .. },
            WorkerFrame::ResourceReport { .. }
        ]
    ));
    assert!(success);
}

#[test]
fn the_worker_refuses_other_limits_and_a_repeated_configure() {
    let other = EngineConfig::Monty {
        memory_bytes: 512 * 1024 * 1024,
    };
    for frames in [
        vec![configure(other)],
        vec![configure(EngineConfig::QUICKJS)],
        vec![
            configure(EngineConfig::MONTY),
            configure(EngineConfig::MONTY),
        ],
    ] {
        let (received, success) = exchange(&frames);
        assert!(
            matches!(
                received.last(),
                Some(WorkerFrame::HostLost {
                    class: LossClass::ProtocolViolation,
                    ..
                })
            ),
            "{received:?}"
        );
        assert!(!success, "a refused configure ends the worker with failure");
    }
}

#[test]
fn exception_reports_keep_cell_ordinals_and_the_tail_of_mutual_recursion() {
    let start = |cell, source: &str| HostFrame::StartCell {
        cell,
        source: source.into(),
        prelude: CellPrelude::Monty { bootstrap: None },
        window: None,
    };
    let (frames, success) = exchange(&[
        configure(EngineConfig::MONTY),
        start(
            81,
            "def left():\n    right()\ndef right():\n    left()\nleft()",
        ),
        HostFrame::Reset,
        start(
            24,
            "try:\n    raise KeyError('missing')\nexcept KeyError as cause:\n    error = ValueError('replacement')\n    error.add_note('the note survives')\n    raise error from cause",
        ),
        start(19, "'x' * 40000"),
    ]);
    assert!(success, "{frames:?}");
    let reports: Vec<_> = frames
        .iter()
        .filter_map(|frame| match frame {
            WorkerFrame::CellDone {
                cell,
                outcome:
                    CellOutcome::Failed {
                        message,
                        generation_lost: false,
                        ..
                    },
            } => Some((*cell, message.as_str())),
            _ => None,
        })
        .collect();
    let [(81, recursion), (24, chained)] = reports.as_slice() else {
        panic!("expected two reports")
    };
    assert!(recursion.starts_with("Traceback (most recent call last):"));
    assert!(recursion.contains("File \"<cell 1>\", line 5"));
    assert!(recursion.contains("in left") && recursion.contains("in right"));
    assert!(recursion.contains("bytes omitted") && recursion.len() <= 32 * 1024);
    assert!(
        recursion
            .lines()
            .last()
            .is_some_and(|line| line.starts_with("RecursionError:"))
    );
    assert!(chained.contains("File \"<cell 2>\", line 2"));
    assert!(chained.contains("KeyError: 'missing'"));
    assert!(chained.contains("The above exception was the direct cause"));
    assert!(chained.ends_with("ValueError: replacement\nthe note survives\n"));
    assert!(frames.iter().any(|frame| matches!(frame,
        WorkerFrame::CellDone { cell: 19, outcome: CellOutcome::Completed { value: Some(text), .. } }
            if text.len() == 40_000
    )));
}

async fn client() -> CellClient {
    CellClient::spawn(
        std::path::Path::new(env!("CARGO_BIN_EXE_plakous-py-host")),
        MONTY_WORKER,
        EngineConfig::MONTY,
    )
    .await
    .expect("worker")
}

async fn tool_failure_report() -> String {
    let client = client().await;
    client.start_cell(72, "x = 1\nawait fetch(value=x)", CellPrelude::Monty { bootstrap: Some(MontyBootstrap {
        source: "async def fetch(*args, **kwargs):\n    return await _fetch(*args, **kwargs)\n".into(),
        bindings: vec![MontyBinding { function: "_fetch".into(), tool: "fetch".into(), input: BindingInput::Dictionary }],
    }) }, None).expect("start");
    let CellEvent::NestedCall { call, .. } = client.read_event().await.expect("call") else {
        panic!("nested call")
    };
    assert_eq!(
        client
            .deliver(72, call, Err("fixture tool denied".into()))
            .await,
        NestedDeliveryOutcome::Accepted
    );
    let message = loop {
        if let CellEvent::Done { outcome, .. } = client.read_event().await.expect("event") {
            let CellOutcome::Failed { message, .. } = outcome else {
                panic!("expected tool failure")
            };
            break message;
        }
    };
    client.terminate().await.expect("joined");
    message
}

#[tokio::test]
async fn tool_failure_points_to_the_cell_and_keeps_the_class_and_message() {
    let message = tool_failure_report().await;
    assert!(message.contains("File \"<cell 1>\", line 2"), "{message}");
    assert!(
        message.contains("ToolError: fixture tool denied"),
        "{message}"
    );
}

#[tokio::test]
async fn tool_failure_does_not_show_bootstrap_frames() {
    let message = tool_failure_report().await;
    assert!(
        !message.contains("prelude") && !message.contains("bootstrap"),
        "{message}"
    );
    assert_eq!(
        message,
        concat!(
            "Traceback (most recent call last):\n",
            "  File \"<cell 1>\", line 2, in <module>\n",
            "    await fetch(value=x)\n",
            "ToolError: fixture tool denied\n",
        )
    );
}

#[tokio::test]
async fn records_and_bytes_reach_the_interpreter_as_typed_values() {
    use plakous_cell::CellValue;

    let client = client().await;
    client
        .start_cell(
            1,
            "reply = await fetch({})\n(reply.label, reply.data)",
            CellPrelude::Monty {
                bootstrap: Some(MontyBootstrap {
                    source: String::new(),
                    bindings: vec![MontyBinding {
                        function: "fetch".into(),
                        tool: "fetch".into(),
                        input: BindingInput::Dictionary,
                    }],
                }),
            },
            None,
        )
        .expect("start");
    let CellEvent::NestedCall { call, .. } = client.read_event().await.expect("call") else {
        panic!("nested call");
    };
    let record = CellValue::Record {
        name: "Reply".into(),
        fields: vec![
            ("label".into(), CellValue::Str("value".into())),
            ("data".into(), CellValue::Bytes(b"hi".to_vec())),
        ],
    };
    assert_eq!(
        client.deliver(1, call, Ok(record)).await,
        NestedDeliveryOutcome::Accepted
    );
    loop {
        if let CellEvent::Done { outcome, .. } = client.read_event().await.expect("event") {
            assert!(
                matches!(outcome, CellOutcome::Completed { value: Some(ref value), .. } if value == "('value', b'hi')"),
                "{outcome:?}"
            );
            break;
        }
    }
    client.terminate().await.expect("joined");
}
