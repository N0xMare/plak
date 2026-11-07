//! A returning host acknowledges answered effects after its ownership sweep.

use super::*;
use plak::store::session_records::SessionRecordStore as _;
use plakous::runner::protocol::{
    Bind, HostFrame, LineOwner, Operation, Request, RequestClass, RunnerFrame, TransferOwner,
    VersionTriple, frame, verbs,
};
use plakous::runner::{CallJournal, FileCallJournal, JournalEntry};
use plakous::sessions::{AgentSessions, SessionBackend, SessionOperationId};

fn received(stream: &mut UnixStream) -> RunnerFrame {
    use std::io::Read as _;
    let mut length = [0; 4];
    stream.read_exact(&mut length).unwrap();
    let mut body = vec![0; u32::from_be_bytes(length).try_into().unwrap()];
    stream.read_exact(&mut body).unwrap();
    frame::decode(&body).unwrap()
}

fn bind(fixture: &Fixture) -> (UnixStream, String) {
    let mut stream = UnixStream::connect(fixture.home().join(format!(
        "host/runner/v{}/run/runner.sock",
        plakous::runner::protocol::PROTOCOL_VERSION
    )))
    .unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    let RunnerFrame::Hello(hello) = received(&mut stream) else {
        panic!("no hello")
    };
    stream
        .write_all(
            &frame::encode(&HostFrame::Bind(Bind {
                host_id: std::fs::read_to_string(fixture.home().join("host-id"))
                    .unwrap()
                    .trim()
                    .into(),
                host_instance: "journal-probe".into(),
                version: VersionTriple::current("journal-probe", "test"),
                inspect: false,
            }))
            .unwrap(),
        )
        .unwrap();
    (stream, hello.instance)
}

fn take_line(fixture: &Fixture, session: &str, stream: &mut UnixStream) -> LineOwner {
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let locator = plak::store::session_records::SqliteSessionStore::new(paths.clone())
        .load(session)
        .unwrap()
        .plakous_locator
        .unwrap();
    let sessions =
        AgentSessions::new(SessionBackend::sqlite(paths.runtime_database()).unwrap()).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let line = sessions.line_head_of(locator).await.unwrap().line();
        let head = sessions
            .open_lease_only(
                &SessionOperationId::new(format!("journal:lease:{}", uuid::Uuid::new_v4()))
                    .unwrap(),
                line,
            )
            .await
            .unwrap();
        let owner = LineOwner {
            line: head.line().to_string(),
            generation: head.generation(),
            branch: head.locator().branch_id().to_string(),
            fence: head.fence(),
        };
        stream
            .write_all(
                &frame::encode(&HostFrame::TransferOwner(TransferOwner {
                    control_id: "journal-transfer".into(),
                    from: None,
                    to: owner.clone(),
                }))
                .unwrap(),
            )
            .unwrap();
        match received(stream) {
            RunnerFrame::TransferAccepted { .. } => {}
            RunnerFrame::StaleOwner { current, .. } => {
                stream
                    .write_all(
                        &frame::encode(&HostFrame::TransferOwner(TransferOwner {
                            control_id: "journal-transfer-retry".into(),
                            from: Some(current),
                            to: owner.clone(),
                        }))
                        .unwrap(),
                    )
                    .unwrap();
                assert!(matches!(
                    received(stream),
                    RunnerFrame::TransferAccepted { .. }
                ));
            }
            answer => panic!("transfer: {answer:?}"),
        }
        head.release().await.unwrap();
        owner
    })
}

#[test]
fn startup_acknowledges_answered_keys_and_empties_its_journal() {
    let Some(fixture) = fixture() else { return };
    let (mut host, session, _) = start_job(&fixture, false);
    kill(&mut host);
    let (mut stream, instance) = bind(&fixture);
    let owner = take_line(&fixture, &session, &mut stream);
    let request = Request {
        request_id: "journal-answer".into(),
        class: RequestClass::EffectCall {
            call_id: "00000000-0000-0000-0000-0000000000c1".into(),
            attempt_id: "00000000-0000-0000-0000-0000000000a1".into(),
        },
        owner: Some(owner),
        deadline_ms: None,
        digest: "journal-digest".into(),
        replay: false,
        age_ms: None,
        operation: Operation {
            verb: verbs::PROCESS_WRITE.into(),
            payload: json!({"root": fixture.workspace(), "cwd": fixture.workspace(), "request": {"session":"00000000000000000000000000000000", "bytes":"", "close":true}}),
        },
    };
    let directory = fixture.home().join(format!(
        "host/runner/v{}/calls/{}",
        plakous::runner::protocol::PROTOCOL_VERSION,
        plak::home::state::state_key()
    ));
    let journal = FileCallJournal::open(directory).unwrap();
    let mut requests = [request.clone(), request];
    requests[1].request_id = "journal-answer-two".into();
    requests[1].class = RequestClass::EffectCall {
        call_id: "00000000-0000-0000-0000-0000000000c2".into(),
        attempt_id: "00000000-0000-0000-0000-0000000000a1".into(),
    };
    for request in &requests {
        journal
            .claim(&JournalEntry {
                request: request.clone(),
                claimed_at_ms: plakous::model::now_unix_millis(),
                dispatched_to: instance.clone(),
            })
            .unwrap();
        stream
            .write_all(&frame::encode(&HostFrame::Request(request.clone())).unwrap())
            .unwrap();
        let answer = received(&mut stream);
        assert!(
            matches!(
                answer,
                RunnerFrame::Error { .. } | RunnerFrame::Result { .. }
            ),
            "{answer:?}"
        );
    }
    // No Ack is sent: startup must acknowledge every answered key.
    drop(stream);
    let mut host = start(&fixture);
    assert!(
        journal.pending().unwrap().is_empty(),
        "startup left an answered key journaled"
    );
    let (mut stream, _) = bind(&fixture);
    take_line(&fixture, &session, &mut stream);
    for mut replay in requests {
        replay.request_id.push_str("-spent-proof");
        replay.replay = true;
        stream
            .write_all(&frame::encode(&HostFrame::Request(replay)).unwrap())
            .unwrap();
        assert!(
            matches!(received(&mut stream), RunnerFrame::Spent { .. }),
            "startup did not acknowledge an answered key"
        );
    }
    drop(stream);
    kill(&mut host);
}

#[test]
fn a_recorded_stop_of_a_predecessor_never_dispatches_on_its_successor_head() {
    let Some(fixture) = fixture() else { return };
    let (mut host, session, node) = start_job(&fixture, false);
    let mut client = Client::connect(&host);
    client.attach(1, &node);
    assert_eq!(
        client.request(
            2,
            Op::StopSession {
                session: session.clone()
            }
        ),
        Outcome::Ok(Answer::Stopped)
    );
    kill(&mut host);
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let product = plak::store::session_records::SqliteSessionStore::new(paths.clone());
    let source = product.load(&session).unwrap().plakous_locator.unwrap();
    sweep::record_stop(&fixture, &session);
    let stops = product.load_intents(&session).unwrap();
    let ordinal = stops.root_interrupts.last().unwrap().ordinal;
    let sessions =
        AgentSessions::new(SessionBackend::sqlite(paths.runtime_database()).unwrap()).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        use plakous_offline_crash_harness::protocol::CrashBoundary;
        use plakous_offline_crash_harness::script::ScriptPlan;
        use plakous_offline_crash_harness::{BoundaryController, scripted_runtime_binding};
        let controller = BoundaryController::quiet_from_evidence_path(
            CrashBoundary::None,
            fixture.root.join("carry-evidence"),
        )
        .unwrap();
        let binding = scripted_runtime_binding(
            &fixture.workspace(),
            controller,
            std::sync::Arc::new(ScriptPlan::read(&fixture.script()).unwrap()),
        )
        .unwrap()
        .with_job_host(std::sync::Arc::new(plakous::tools::LocalJobHost::new()));
        let plakous::sessions::CreateSessionOutcome::Created { session: next, .. } = sessions
            .create(
                plakous::sessions::CreateSessionRequest::new(
                    SessionOperationId::new("stop:carry").unwrap(),
                    binding,
                )
                .continuing_line(source),
            )
            .await
            .unwrap()
        else {
            panic!("carry not created")
        };
        assert_ne!(next.locator().session_id(), source.session_id());
        next.release().await.unwrap();
    });
    let mut host = start(&fixture);
    let (mut stream, _) = bind(&fixture);
    let owner = take_line(&fixture, &session, &mut stream);
    let request = Request {
        request_id: "predecessor-stop-proof".into(),
        class: RequestClass::OwnerControl {
            control_id: format!("plak:stop:{session}:{ordinal}"),
        },
        owner: Some(owner),
        deadline_ms: None,
        digest: "probe-only".into(),
        replay: true,
        age_ms: None,
        operation: Operation {
            verb: verbs::JOBS_STOP.into(),
            payload: json!({}),
        },
    };
    stream
        .write_all(&frame::encode(&HostFrame::Request(request)).unwrap())
        .unwrap();
    let answer = received(&mut stream);
    assert!(
        matches!(answer, RunnerFrame::NotSeen { .. }),
        "a predecessor stop reached the successor: {answer:?}"
    );
    drop(stream);
    kill(&mut host);
}
