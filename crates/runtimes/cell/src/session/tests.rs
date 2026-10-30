use super::*;
use crate::protocol::{
    CELL_PROTOCOL_REVISION, DeliveryVerdict, EngineConfig, HostFrame, QUICKJS_WORKER, WorkerFrame,
};
use std::{fmt::Write as _, os::unix::fs::PermissionsExt as _, path::PathBuf};

mod abandonment;
mod observation;

const SOURCE: &str = "text('scripted')";
const WINDOW: u64 = 10_000;

/// One step of a scripted worker after its hello.
enum Step {
    /// Consume exactly these host frames.
    Read(Vec<HostFrame>),
    /// Consume and compare these exact host frames.
    ReadExact(Vec<HostFrame>),
    /// Write these worker frames.
    Write(Vec<WorkerFrame>),
}

fn configure() -> HostFrame {
    HostFrame::Configure {
        sdk_stubs: None,
        revision: CELL_PROTOCOL_REVISION.to_owned(),
        engine: EngineConfig::QUICKJS,
    }
}

fn start(cell: u64) -> HostFrame {
    HostFrame::StartCell {
        cell,
        source: SOURCE.to_owned(),
        prelude: CellPrelude::Quickjs {
            tools: Vec::new(),
            stored: HashMap::new(),
        },
        window: Some(WINDOW),
    }
}

fn nested_result(call: u64) -> HostFrame {
    HostFrame::NestedResult {
        cell: 1,
        call,
        outcome: Ok(CellValue::from(serde_json::json!(7))),
    }
}

#[allow(
    clippy::similar_names,
    reason = "cell and call are the protocol's own field names"
)]
fn nested_call(cell: u64, call: u64) -> WorkerFrame {
    WorkerFrame::NestedCall {
        cell,
        call,
        name: "lookup".into(),
        input: CellValue::from(serde_json::json!({})),
    }
}

fn done(cell: u64) -> WorkerFrame {
    WorkerFrame::CellDone {
        cell,
        outcome: CellOutcome::Completed {
            value: None,
            stored: HashMap::new(),
        },
    }
}

/// A worker that says hello as the `QuickJS` worker, runs `steps` and
/// exits; host frames are consumed by exact encoded length.
fn scripted_worker(directory: &std::path::Path, steps: &[Step]) -> PathBuf {
    scripted_worker_as(directory, QUICKJS_WORKER, steps, "")
}

/// A scripted worker naming `identity`, which runs `tail` after `steps`.
fn scripted_worker_as(
    directory: &std::path::Path,
    identity: crate::protocol::WorkerIdentity,
    steps: &[Step],
    tail: &str,
) -> PathBuf {
    let hello = WorkerFrame::Hello {
        revision: CELL_PROTOCOL_REVISION.to_owned(),
        engine: identity.engine.to_owned(),
        adapter: identity.adapter.to_owned(),
    };
    let mut writes = vec![encode_frame(&hello).expect("hello")];
    let mut body = String::from("#!/bin/sh\ncat \"$0.0\"\n");
    for step in steps {
        match step {
            Step::ReadExact(frames) => {
                let bytes: Vec<_> = frames
                    .iter()
                    .flat_map(|frame| encode_frame(frame).expect("host frame"))
                    .collect();
                let _ = writeln!(
                    body,
                    "dd bs=1 count={} 2>/dev/null | cmp - \"$0.{}\" || exit 1",
                    bytes.len(),
                    writes.len()
                );
                writes.push(bytes);
            }
            Step::Read(frames) => {
                let bytes: usize = frames
                    .iter()
                    .map(|frame| encode_frame(frame).expect("host frame").len())
                    .sum();
                let _ = writeln!(body, "dd bs=1 count={bytes} >/dev/null 2>&1");
            }
            Step::Write(frames) => {
                let _ = writeln!(body, "cat \"$0.{}\"", writes.len());
                writes.push(
                    frames
                        .iter()
                        .flat_map(|frame| encode_frame(frame).expect("worker frame"))
                        .collect(),
                );
            }
        }
    }
    body.push_str(tail);
    let program = directory.join("worker");
    for (index, bytes) in writes.iter().enumerate() {
        std::fs::write(directory.join(format!("worker.{index}")), bytes).expect("frames");
    }
    let staging = directory.join("worker.pending");
    std::fs::write(&staging, body).expect("program");
    std::fs::rename(staging, &program).expect("publish program");
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))
        .expect("permissions");
    program
}

async fn session(program: &std::path::Path) -> (Arc<CellClient>, CellSession, CellSessionEvents) {
    let client = Arc::new(
        CellClient::spawn(program, QUICKJS_WORKER, EngineConfig::QUICKJS)
            .await
            .expect("worker hello"),
    );
    let (session, events) = CellSession::new(Arc::clone(&client), ObservationMode::EngineParked);
    (client, session, events)
}

async fn execute(session: &CellSession) -> StartedCell {
    session
        .execute(
            SOURCE,
            CellPrelude::Quickjs {
                tools: Vec::new(),
                stored: HashMap::new(),
            },
            WINDOW,
            async { Ok::<(), ()>(()) },
        )
        .await
        .map_err(|_| "start")
        .expect("start admitted")
}

#[tokio::test]
async fn wait_delivers_a_buffered_terminal_after_the_worker_ends() {
    let directory = tempfile::tempdir().expect("directory");
    let program = scripted_worker(
        directory.path(),
        &[
            Step::Read(vec![configure(), start(1)]),
            Step::Write(vec![WorkerFrame::Yielded { cell: 1 }, done(1)]),
        ],
    );
    let (client, session, _events) = session(&program).await;
    let started = execute(&session).await;
    // Either observation may win the terminal race, but a buffered
    // terminal takes precedence over an unconsumed yield.
    let initial = started.initial.await;
    // The worker ended after its terminal: no resume can reach it.
    let _ = client.wait_loss().await;
    if matches!(initial, Ok(CellResponse::Yielded { .. })) {
        assert!(matches!(
            session.wait("1", WINDOW).await,
            WaitOutcome::Live(CellResponse::Result {
                cell: 1,
                error: None,
                ..
            })
        ));
    } else {
        assert!(matches!(
            initial,
            Ok(CellResponse::Result {
                cell: 1,
                error: None,
                ..
            })
        ));
    }
    assert_eq!(
        session.wait("1", WINDOW).await,
        WaitOutcome::Missing { error: None }
    );
    let _ = session.shutdown().await;
}

struct Case {
    name: &'static str,
    steps: Vec<Step>,
    accepted: bool,
    completed: bool,
}

#[tokio::test]
async fn worker_eof_drains_a_terminal_while_the_nested_responder_is_pending() {
    for completed in [true, false] {
        let directory = tempfile::tempdir().expect("directory");
        let mut final_frames = vec![WorkerFrame::Delivery {
            cell: 1,
            call: 1,
            verdict: DeliveryVerdict::Accepted,
        }];
        if completed {
            final_frames.push(done(1));
        }
        let program = scripted_worker(
            directory.path(),
            &[
                Step::Read(vec![configure(), start(1)]),
                Step::Write(vec![nested_call(1, 1)]),
                Step::Read(vec![nested_result(1)]),
                Step::Write(final_frames),
            ],
        );
        let (client, session, mut events) = session(&program).await;
        let started = execute(&session).await;
        let Some(CellSessionEvent::NestedCall { responder, .. }) = events.receive().await else {
            panic!("nested call");
        };
        // Model the responder being descheduled after the worker accepts
        // its result, before it wakes the pump with its local reply.
        assert_eq!(
            client
                .deliver(1, 1, Ok(CellValue::from(serde_json::json!(7))))
                .await,
            NestedDeliveryOutcome::Accepted
        );
        let _ = client.wait_loss().await;
        let terminal = tokio::time::timeout(Duration::from_secs(5), started.initial)
            .await
            .expect("terminal despite the pending responder");
        assert_eq!(
            matches!(terminal, Ok(CellResponse::Result { error: None, .. })),
            completed,
            "completed={completed}: {terminal:?}"
        );
        if !completed {
            assert!(matches!(terminal, Err(ref loss) if loss == "cell worker closed"));
        }
        assert_eq!(
            session.wait("1", WINDOW).await,
            WaitOutcome::Missing { error: None }
        );
        drop(responder);
        let _ = session.shutdown().await;
    }
}

/// Interleavings of nested calls, verdicts, output and terminals.
fn interleavings() -> Vec<Case> {
    vec![
        Case {
            name: "accepted then done",
            steps: vec![
                Step::Read(vec![configure(), start(1)]),
                Step::Write(vec![nested_call(1, 1)]),
                Step::Read(vec![nested_result(1)]),
                Step::Write(vec![
                    WorkerFrame::Delivery {
                        cell: 1,
                        call: 1,
                        verdict: DeliveryVerdict::Accepted,
                    },
                    WorkerFrame::Output {
                        cell: 1,
                        item: OutputItem::Text { text: "7".into() },
                    },
                    done(1),
                ]),
            ],
            accepted: true,
            completed: true,
        },
        Case {
            name: "result read, worker ends before its verdict",
            steps: vec![
                Step::Read(vec![configure(), start(1)]),
                Step::Write(vec![nested_call(1, 1)]),
                Step::Read(vec![nested_result(1)]),
            ],
            accepted: false,
            completed: false,
        },
        Case {
            name: "not delivered then done",
            steps: vec![
                Step::Read(vec![configure(), start(1)]),
                Step::Write(vec![nested_call(1, 1)]),
                Step::Read(vec![nested_result(1)]),
                Step::Write(vec![
                    WorkerFrame::Delivery {
                        cell: 1,
                        call: 1,
                        verdict: DeliveryVerdict::NotDelivered("cancelled".into()),
                    },
                    done(1),
                ]),
            ],
            accepted: false,
            completed: true,
        },
        Case {
            name: "verdict for a result never sent",
            steps: vec![
                Step::Read(vec![configure(), start(1)]),
                Step::Write(vec![
                    WorkerFrame::Delivery {
                        cell: 1,
                        call: 9,
                        verdict: DeliveryVerdict::Accepted,
                    },
                    done(1),
                ]),
            ],
            accepted: false,
            completed: false,
        },
        Case {
            name: "a frame naming another cell",
            steps: vec![
                Step::Read(vec![configure(), start(1)]),
                Step::Write(vec![done(2), done(1)]),
            ],
            accepted: false,
            completed: false,
        },
        Case {
            name: "two terminals",
            steps: vec![
                Step::Read(vec![configure(), start(1)]),
                Step::Write(vec![done(1), done(1)]),
            ],
            accepted: false,
            completed: true,
        },
    ]
}

/// Every scripted interleaving ends the cell with exactly one terminal,
/// and a nested result is `Accepted` only when the worker said so.
#[tokio::test]
async fn every_interleaving_has_one_terminal_and_no_unproven_acceptance() {
    for case in interleavings() {
        let directory = tempfile::tempdir().expect("directory");
        let program = scripted_worker(directory.path(), &case.steps);
        let (_client, session, mut events) = session(&program).await;
        let started = execute(&session).await;
        let answer = tokio::spawn(async move {
            let mut verdicts = Vec::new();
            while let Some(event) = events.receive().await {
                match event {
                    CellSessionEvent::NestedCall { responder, .. } => {
                        verdicts.push(responder.resolve(serde_json::json!(7)).await);
                    }
                    CellSessionEvent::Notification { responder, .. } => {
                        let _ = responder.acknowledge();
                    }
                }
            }
            verdicts
        });
        let terminal = tokio::time::timeout(Duration::from_secs(5), started.initial)
            .await
            .expect("one terminal without the turn's timeout");
        assert_eq!(
            matches!(terminal, Ok(CellResponse::Result { error: None, .. })),
            case.completed,
            "{}: {terminal:?}",
            case.name
        );
        assert_eq!(
            session.wait("1", WINDOW).await,
            WaitOutcome::Missing { error: None },
            "{}: a second terminal",
            case.name
        );
        let _ = session.shutdown().await;
        drop(session);
        let verdicts = answer.await.expect("answers");
        assert_eq!(
            verdicts.contains(&NestedDeliveryOutcome::Accepted),
            case.accepted,
            "{}: {verdicts:?}",
            case.name
        );
    }
}

fn monty_configure() -> HostFrame {
    HostFrame::Configure {
        sdk_stubs: None,
        revision: CELL_PROTOCOL_REVISION.to_owned(),
        engine: EngineConfig::MONTY,
    }
}

fn monty_start(cell: u64) -> HostFrame {
    HostFrame::StartCell {
        cell,
        source: SOURCE.to_owned(),
        prelude: CellPrelude::Monty { bootstrap: None },
        window: None,
    }
}

async fn monty_session(
    program: &std::path::Path,
    request_bound: Duration,
) -> (CellSession, CellSessionEvents) {
    let client = Arc::new(
        CellClient::spawn(program, crate::protocol::MONTY_WORKER, EngineConfig::MONTY)
            .await
            .expect("worker hello"),
    );
    CellSession::new(client, ObservationMode::HostWindow { request_bound })
}

async fn monty_execute(session: &CellSession, window: u64) -> StartedCell {
    session
        .execute(
            SOURCE,
            CellPrelude::Monty { bootstrap: None },
            window,
            async { Ok::<(), ()>(()) },
        )
        .await
        .map_err(|_| "start")
        .expect("start admitted")
}

fn text(text: &str) -> WorkerFrame {
    WorkerFrame::Output {
        cell: 1,
        item: OutputItem::Text { text: text.into() },
    }
}

/// A host-window cell keeps running: an observation that ends first
/// returns its buffered output as one text, and a later wait returns the
/// rest with the terminal's value after it.
#[tokio::test]
async fn a_host_window_cell_yields_its_buffered_output_and_keeps_running() {
    let directory = tempfile::tempdir().expect("directory");
    let program = scripted_worker_as(
        directory.path(),
        crate::protocol::MONTY_WORKER,
        &[
            Step::Read(vec![monty_configure(), monty_start(1)]),
            Step::Write(vec![
                text("a"),
                text("b"),
                nested_call(1, 7),
                WorkerFrame::Waiting {
                    cell: 1,
                    calls: vec![7],
                },
            ]),
            Step::Read(vec![nested_result(7)]),
            Step::Write(vec![
                WorkerFrame::Delivery {
                    cell: 1,
                    call: 7,
                    verdict: DeliveryVerdict::Accepted,
                },
                text("c"),
                WorkerFrame::CellDone {
                    cell: 1,
                    outcome: CellOutcome::Completed {
                        value: Some("v".into()),
                        stored: HashMap::new(),
                    },
                },
            ]),
        ],
        "exec sleep 30\n",
    );
    let (session, mut events) = monty_session(&program, Duration::from_secs(30)).await;
    let started = monty_execute(&session, 0).await;
    let Some(CellSessionEvent::NestedCall { responder, .. }) = events.receive().await else {
        panic!("the nested call arrives after the output before it");
    };
    assert_eq!(session.active_cell().await, Some(1));
    assert_eq!(
        started.initial.await,
        Ok(CellResponse::Yielded {
            cell: 1,
            items: vec![OutputItem::Text { text: "ab".into() }],
        })
    );
    assert_eq!(
        responder.resolve(serde_json::json!(7)).await,
        NestedDeliveryOutcome::Accepted
    );
    assert_eq!(
        session.wait("1", WINDOW).await,
        WaitOutcome::Live(CellResponse::Result {
            cell: 1,
            items: vec![
                OutputItem::Text { text: "c".into() },
                OutputItem::Text { text: "v".into() },
            ],
            error: None,
        })
    );
    assert_eq!(session.active_cell().await, None);
    assert!(matches!(
        session.stop().await,
        Ok(WorkerStop { forced: true, .. })
    ));
}

/// Computation that reaches no host-visible boundary within the request
/// bound kills the worker; that loss is sticky.
#[tokio::test]
async fn the_request_bound_kills_a_worker_that_never_reaches_a_boundary() {
    let directory = tempfile::tempdir().expect("directory");
    let program = scripted_worker_as(
        directory.path(),
        crate::protocol::MONTY_WORKER,
        &[Step::Read(vec![monty_configure(), monty_start(1)])],
        "exec sleep 30\n",
    );
    let (session, _events) = monty_session(&program, Duration::from_millis(300)).await;
    let started = monty_execute(&session, 20_000).await;
    let loss = tokio::time::timeout(Duration::from_secs(10), started.initial)
        .await
        .expect("the bound ends the observation")
        .expect_err("a killed worker is a loss");
    assert!(loss.contains("timed out"), "{loss}");
    assert!(session.shutdown().await.is_err(), "the kill stays a loss");
}

/// A buffered terminal is visible without consuming it, and results for
/// a cell that already ended are definitely unobserved.
#[tokio::test]
async fn a_buffered_terminal_is_ready_and_late_results_are_unobserved() {
    let directory = tempfile::tempdir().expect("directory");
    let program = scripted_worker_as(
        directory.path(),
        crate::protocol::MONTY_WORKER,
        &[
            Step::Read(vec![monty_configure(), monty_start(1)]),
            Step::Write(vec![nested_call(1, 3), done(1)]),
        ],
        "exec sleep 30\n",
    );
    let (session, mut events) = monty_session(&program, Duration::from_secs(30)).await;
    let started = monty_execute(&session, 0).await;
    drop(started);
    let Some(CellSessionEvent::NestedCall { responder, .. }) = events.receive().await else {
        panic!("nested call");
    };
    let ready = tokio::time::timeout(Duration::from_secs(5), async {
        while !session.terminal_ready(1).await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(ready.is_ok(), "the terminal is buffered");
    assert!(matches!(
        responder.resolve(serde_json::json!(1)).await,
        NestedDeliveryOutcome::NotObserved(_)
    ));
    assert!(matches!(
        session.wait("1", 0).await,
        WaitOutcome::Live(CellResponse::Result { error: None, .. })
    ));
    assert_eq!(
        session.wait("1", 0).await,
        WaitOutcome::Missing { error: None }
    );
    let _ = session.stop().await;
}

#[tokio::test]
async fn a_lost_generation_still_certifies_its_reaped_worker() {
    let directory = tempfile::tempdir().expect("directory");
    let program = scripted_worker_as(
        directory.path(),
        crate::protocol::MONTY_WORKER,
        &[
            Step::Read(vec![monty_configure(), monty_start(1)]),
            Step::Write(vec![
                text("partial stdout"),
                WorkerFrame::CellDone {
                    cell: 1,
                    outcome: CellOutcome::Failed {
                        message: "undecodable traceback".into(),
                        stored: HashMap::new(),
                        generation_lost: true,
                    },
                },
            ]),
        ],
        "",
    );
    let (session, _events) = monty_session(&program, Duration::from_secs(30)).await;
    let started = monty_execute(&session, WINDOW).await;
    assert_eq!(
        started.initial.await,
        Ok(CellResponse::Result {
            cell: 1,
            items: vec![OutputItem::Text {
                text: "partial stdout".into()
            }],
            error: Some("undecodable traceback".into()),
        })
    );
    assert!(session.generation_lost());
    assert!(
        session.shutdown().await.is_ok(),
        "a definite failed cell does not erase its worker's reaped receipt"
    );
}
