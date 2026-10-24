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
    CELL_PROTOCOL_REVISION, EngineConfig, HostFrame, LossClass, MAX_FRAME_BYTES, MONTY_WORKER,
    WorkerFrame, encode_frame, read_frame_blocking,
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

#[tokio::test]
async fn records_and_bytes_reach_the_interpreter_as_typed_values() {
    use plakous_cell::{
        BindingInput, CellClient, CellEvent, CellOutcome, CellPrelude, CellValue, MontyBinding,
        MontyBootstrap, NestedDeliveryOutcome,
    };
    let client = CellClient::spawn(
        std::path::Path::new(env!("CARGO_BIN_EXE_plakous-py-host")),
        MONTY_WORKER,
        EngineConfig::MONTY,
    )
    .await
    .expect("worker");
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
