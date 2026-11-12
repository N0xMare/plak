use super::*;

type Frame = HostFrame<PlakExtension>;

fn store(fixture: &Fixture) -> plak::store::session_records::SqliteSessionStore {
    plak::store::session_records::SqliteSessionStore::new(plak::home::paths::PlakPaths::explicit(
        fixture.home(),
    ))
}

fn seed(fixture: &Fixture, variant: &str) -> String {
    fixture.write_script(&json!({"version":1,"repeat_last_step":true,"steps":[{"output":{"text":"fixture answer"}}]}));
    let output = fixture
        .command()
        .args(["--test-seed-abandoned-open", variant])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["provenance"]["reopened"], false);
    let submitted = variant == "with-submission";
    assert_eq!(receipt["with_submission"], submitted);
    let session = receipt["session"].as_str().unwrap().to_owned();
    let record = store(fixture).load(&session).unwrap();
    assert_eq!(record.pending_queue.len(), usize::from(submitted));
    let intents = store(fixture).load_intents(&session).unwrap();
    let submitted_root = intents
        .submissions
        .iter()
        .filter(|row| row.command_id == "face-fenced-root")
        .collect::<Vec<_>>();
    assert_eq!(submitted_root.len(), usize::from(submitted));
    if submitted {
        assert_eq!(
            submitted_root[0].kind,
            plakous_hosted::sessions::projector::SubmissionKind::RootStart
        );
        assert_eq!(submitted_root[0].queue_item_id, record.pending_queue[0].id);
    }
    session
}

fn connect(fixture: &Fixture) -> Bridge {
    let mut bridge = Bridge::start(fixture);
    bridge.hello(WIRE_VERSION);
    assert!(matches!(bridge.next(), HostFrame::Welcome(_)));
    bridge
}

fn baseline(bridge: &mut Bridge, request: u64, journey: &mut Vec<Frame>) -> Option<u64> {
    let (mut answered, mut baseline, mut attachment) = (false, false, None);
    let until = Instant::now() + WAIT;
    while !answered || !baseline {
        assert!(
            Instant::now() < until,
            "missing authoritative baseline: {journey:?}"
        );
        let frame = bridge.next();
        match &frame {
            HostFrame::Response { id, outcome } if *id == request => {
                match outcome {
                    Outcome::Ok(Answer::Attached { attachment: id }) => attachment = Some(*id),
                    Outcome::Ok(Answer::Accepted { duplicate: false }) => {}
                    other => panic!("read refused: {other:?}"),
                }
                answered = true;
            }
            HostFrame::Snapshot(page) if page.scope == TranscriptScope::Root && page.last => {
                baseline = true;
            }
            _ => {}
        }
        journey.push(frame);
    }
    attachment
}

fn flush(bridge: &mut Bridge, journey: &mut Vec<Frame>) {
    bridge.send(&ClientFrame::Request {
        id: 90,
        op: Op::Ping,
    });
    loop {
        let frame = bridge.next();
        let done = matches!(
            &frame,
            HostFrame::Response {
                id: 90,
                outcome: Outcome::Ok(Answer::Pong)
            }
        );
        journey.push(frame);
        if done {
            return;
        }
    }
}

fn verify(journey: &[Frame], session: &str) {
    let mut counts = (0, 0, 0, 0, 0);
    let mut row_index = None;
    let mut baseline_index = None;
    for (index, frame) in journey.iter().enumerate() {
        if matches!(frame, HostFrame::Snapshot(page) if page.last) {
            baseline_index = Some(index);
        }
        let HostFrame::Event { observation, .. } = frame else {
            continue;
        };
        match observation {
            SessionObservation::Recovered {
                claim,
                cause,
                operations,
            } => {
                assert!(claim.is_some());
                assert!(cause.is_some());
                assert_ne!(operations.len(), 0);
                counts.0 += 1;
            }
            SessionObservation::Row(row)
                if row
                    .text()
                    .is_some_and(|text| text.contains("recovered an interrupted turn")) =>
            {
                counts.1 += 1;
                row_index = Some(index);
                let jsonl = plak::agent_jsonl::project_host_event(
                    observation,
                    plakous_host_protocol::envelope::JsonlCorrelation::default(),
                    Some(session),
                )
                .unwrap();
                assert!(jsonl.contains("recovered an interrupted turn"));
            }
            SessionObservation::Binding { session_id, .. } => {
                assert_eq!(session_id.as_deref(), Some(session));
                counts.2 += 1;
            }
            SessionObservation::QueueSnapshot { .. } => counts.3 += 1,
            SessionObservation::PromptHistorySnapshot(_) => counts.4 += 1,
            _ => {}
        }
    }
    assert_eq!((counts.0, counts.1), (1, 1), "{journey:?}");
    assert!(counts.2 > 0 && counts.3 > 0 && counts.4 > 0);
    assert!(
        baseline_index.unwrap() > row_index.unwrap(),
        "the later baseline must follow the actual derived row"
    );
}

#[test]
fn client_producer_abandoned_open_emits_real_recovery_once_for_both_submission_states() {
    for variant in ["without-submission", "with-submission"] {
        let Some(fixture) = fixture() else { return };
        let session = seed(&fixture, variant);
        let dispatched = providers(&fixture);
        assert_eq!(dispatched, 1);
        let mut host = start_env(
            &fixture,
            &[],
            &[("PLAK_TEST_RECOVERY_FIRST_ATTACH", session.clone())],
        );
        let mut bridge = connect(&fixture);
        let Outcome::Ok(Answer::Sessions { rows }) = bridge.request(
            1,
            Op::ListSessions {
                workspace: None,
                runner: None,
                include_archived: false,
            },
        ) else {
            panic!("no saved session")
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session, session);
        let node = rows[0].node.clone();
        bridge.send(&ClientFrame::Request {
            id: 2,
            op: Op::Attach { node: node.clone() },
        });
        let mut journey = Vec::new();
        let attachment = baseline(&mut bridge, 2, &mut journey).unwrap();
        bridge.send(&ClientFrame::Request {
            id: 3,
            op: Op::Intent {
                attachment,
                client_id: None,
                blobs: Vec::new(),
                intent: SessionIntent::ResyncTranscript {
                    scope: TranscriptScope::Root,
                },
            },
        });
        baseline(&mut bridge, 3, &mut journey);
        flush(&mut bridge, &mut journey);
        bridge.send(&ClientFrame::Bye);
        bridge.finish();
        let mut reopened = connect(&fixture);
        reopened.send(&ClientFrame::Request {
            id: 4,
            op: Op::Attach { node },
        });
        baseline(&mut reopened, 4, &mut journey);
        flush(&mut reopened, &mut journey);
        reopened.send(&ClientFrame::Bye);
        reopened.finish();
        stop(&mut host);
        verify(&journey, &session);
        assert_eq!(
            providers(&fixture),
            dispatched,
            "open/resync/reattach must not dispatch a client continuation"
        );
        let record = store(&fixture).load(&session).unwrap();
        assert!(!record.transcript.iter().any(|row| {
            row.text()
                .is_some_and(|text| text.contains("recovered an interrupted turn"))
        }));
    }
}
