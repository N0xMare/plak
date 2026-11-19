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
    MontyBootstrap, NestedDeliveryOutcome, OutputItem, WorkerFrame, encode_frame,
    read_frame_blocking,
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

fn start(cell: u64, source: &str) -> HostFrame {
    HostFrame::StartCell {
        cell,
        source: source.into(),
        prelude: CellPrelude::Monty { bootstrap: None },
        window: None,
    }
}

#[test]
fn charged_heap_can_reach_the_soft_limit_with_the_worker_alive() {
    let source = r"s = 'a' * (60 << 20)
b = 'b' * (60 << 20)
c = 'c' * (60 << 20)
d = 'd' * (20 << 20)
try:
    s.upper()
except MemoryError:
    print('caught at the charged limit')
else:
    raise AssertionError('transient copy exceeded the heap limit')";
    let (frames, success) = exchange(&[
        configure(EngineConfig::MONTY),
        start(1, source),
        start(2, "(len(s) + len(b) + len(c) + len(d)) >> 20"),
        HostFrame::Shutdown,
    ]);
    assert!(success, "{frames:?}");
    assert!(frames.iter().any(|f| matches!(f, WorkerFrame::Output { cell: 1, item: OutputItem::Text { text } } if text == "caught at the charged limit\n")));
    assert!(frames.iter().any(|f| matches!(f, WorkerFrame::CellDone { cell: 2, outcome: CellOutcome::Completed { value: Some(value), .. } } if value == "200")));
}

#[test]
fn print_cap_is_cumulative_and_preserves_utf8_boundary_and_notice() {
    let notice = "\n[Python output truncated at 1 MiB]\n";
    let prefix_bytes = 1_048_576 - 64 - notice.len() - 1;
    let source =
        format!("print('a' * {prefix_bytes}, end='')\nprint('é', end='')\nprint('dropped')");
    let (frames, success) = exchange(&[configure(EngineConfig::MONTY), start(1, &source)]);
    assert!(success);
    let mut text = String::new();
    for frame in frames {
        if let WorkerFrame::Output {
            cell: 1,
            item: OutputItem::Text { text: chunk },
        } = frame
        {
            text.push_str(&chunk);
        }
    }
    assert_eq!(text.len(), prefix_bytes + notice.len());
    assert!(text[..prefix_bytes].bytes().all(|byte| byte == b'a'));
    assert_eq!(&text[prefix_bytes..], notice);
}

#[test]
fn sleep_and_unseeded_entropy_are_refused_without_nested_effects() {
    let source = r"
import os, random, time

def refused(thunk, message):
    try:
        thunk()
    except RuntimeError as error:
        assert message in str(error), str(error)
    else:
        raise AssertionError(message)

refused(lambda: time.sleep(0), 'time.sleep is not available')
try:
    await asyncio.sleep(0)
except RuntimeError as error:
    assert 'asyncio.sleep is not available' in str(error)
else:
    raise AssertionError('asyncio.sleep ran')
refused(lambda: os.urandom(8), 'os.urandom is not available')
refused(lambda: random.Random(), 'pass an explicit seed')
refused(lambda: random.random(), 'call random.seed first')
refused(lambda: random.SystemRandom().random(), 'no source of randomness')
try:
    import uuid
except ModuleNotFoundError:
    pass
else:
    refused(uuid.uuid4, 'os.urandom is not available')
expected = random.Random(42).random()
random.seed(42)
assert random.random() == expected
'refused ambient operations'
";
    let (frames, success) = exchange(&[configure(EngineConfig::MONTY), start(1, source)]);
    assert!(success);
    assert!(
        matches!(frames.as_slice(), [WorkerFrame::Hello { .. }, WorkerFrame::CellDone { outcome: CellOutcome::Completed { value: Some(value), .. }, .. }] if value == "refused ambient operations"),
        "{frames:?}"
    );
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
