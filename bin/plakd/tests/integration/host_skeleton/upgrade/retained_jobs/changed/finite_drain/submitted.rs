//! A continued child must deliver its submitted output after finalization.

use super::*;
use plakous::sessions::AgentSessions;
use plakous::sessions::feed::{
    FeedCursor, FeedFact, FeedPage, FeedPageRequest, FeedRecord, FeedTurnOutcome,
};
use plakous_recursive_subagents::protocol::AttemptStatus;

#[test]
fn an_automatically_continued_child_delivers_its_submission() {
    continued_child_delivers_submission(false, false);
}

#[test]
fn a_manually_resumed_child_delivers_its_submission() {
    continued_child_delivers_submission(true, false);
}

#[test]
fn a_submitted_child_result_survives_a_crash_after_the_fence() {
    continued_child_delivers_submission(false, true);
}

fn continued_child_delivers_submission(manual: bool, crash: bool) {
    let Some(fixture) = fixture() else { return };
    let fixture = if crash {
        fixture.with_session_lease_seconds(2)
    } else {
        fixture
    };
    let mut sut = FiniteDrain::new(fixture);
    let cut = sut.fixture.root.join("process-cleanup-cut");
    std::fs::create_dir(&cut).unwrap();
    sut.environment.push((
        "PLAK_TEST_PROCESS_CLEANUP_CUT",
        cut.to_str().unwrap().to_owned(),
    ));
    if crash {
        sut.environment.push((
            "PLAK_TEST_HOST_READOPT_CUT",
            plakous_hosted::test_support_runner::HostCut::AfterChildFence
                .name()
                .into(),
        ));
    }
    let (mut host, child) = child_with_pending_root(
        &sut,
        manual.then(|| {
            (
                "PLAK_TEST_HOST_UPGRADE_PREPARE_FAILURE",
                "child-handoff-no-budget".into(),
            )
        }),
    );
    let terminal = sut.fixture.root.join("finish-child-turn");
    std::fs::write(
        sut.fixture.root.join("child.json"),
        json!({
            "version": 1, "process_tools": true,
            "steps": [
                {"gate": {"file": sut.fixture.root.join("resume-release")},
                 "output": {"tool": {"name": "wait", "arguments": {"timeout_ms": 30_000}}},
                 "finish": "tool_calls"},
                {
                 "output": {"tool": {"name": "submit_result", "arguments": {
                    "turn_token": 1, "output_json": "{\"summary\":\"review complete\"}"
                 }}}, "finish": "tool_calls"},
                {"gate": {"file": terminal}, "output": {"text": "review complete"}}
            ]
        })
        .to_string(),
    )
    .unwrap();
    sut.upgrade(&mut host, "0");
    let paths = plakd::home::paths::PlakPaths::explicit(sut.fixture.home());
    let store = SqliteTreeStore::open(&paths.subagents_database()).unwrap();
    let read = || ProductAgentTree::reopen(store.load(&child.parent.tree_id).unwrap().unwrap());
    let id = read()
        .nodes()
        .values()
        .find(|node| node.parent_agent_id.is_some())
        .unwrap()
        .agent_id
        .clone();
    if manual {
        explicitly_resume(&sut, &host, &child.parent, &id);
    }
    let deadline = Instant::now() + WAIT;
    while crash::client_control::child_dispatches(&sut.fixture) < 2 {
        assert!(Instant::now() < deadline, "the child did not continue");
        std::thread::sleep(Duration::from_millis(10));
    }
    let locator = child_locator(&store, &child);
    release_child_wait(
        &sut.fixture,
        &paths.runtime_database(),
        locator,
        child.original.0,
    );
    sut.finish_job(child.original);
    root_wake::submitted_result(&sut.fixture, &child.parent);
    let armed = cut.join(locator.session_id().to_string());
    std::fs::write(&armed, "").unwrap();
    std::fs::write(terminal, "").unwrap();
    if crash {
        use std::os::unix::process::ExitStatusExt as _;
        assert_eq!(
            runner::cut_exit(host.child.as_mut().unwrap()).signal(),
            Some(9)
        );
        let crashed = read();
        assert_eq!(
            crashed.attempt(&id).unwrap().status,
            AttemptStatus::Submitted
        );
        assert!(crashed.attempt(&id).unwrap().failure.is_none());
        assert!(!crashed.node(&id).unwrap().active_turn);
        let mut replacement = image_command(&sut.fixture, &sut.image);
        replacement
            .envs(
                sut.environment
                    .iter()
                    .filter(|(key, _)| *key != "PLAK_TEST_HOST_READOPT_CUT")
                    .cloned(),
            )
            .arg("host");
        host = await_serving_host(spawn_host(&mut replacement)).unwrap();
        let mut client = Client::connect(&host);
        runner::remote_ready(&mut client, &sut.paired.id);
        client.attach(12, &child.parent);
    }

    let deadline = Instant::now() + WAIT;
    let command_id =
        loop {
            let tree = read();
            let attempt = tree.attempt(&id).unwrap();
            if !matches!(
                attempt.status,
                AttemptStatus::Running | AttemptStatus::Submitted
            ) {
                assert_eq!(
                    attempt.status,
                    AttemptStatus::Completed,
                    "{:?}",
                    attempt.failure
                );
                assert_eq!(
                    attempt.output_json.as_deref(),
                    Some("{\"summary\":\"review complete\"}")
                );
                assert_eq!(loss_resumptions(&tree), 1);
                assert!(!armed.exists(), "the child missed the cleanup cut");
                assert!(tree.operations().values().all(|row| {
                    row.caller_agent_id != id || row.tool_name != "host_turn_failure"
                }));
                let results: Vec<_> = tree
                    .messages()
                    .values()
                    .filter(|row| {
                        row.from == id
                            && row.purpose
                                == plakous_recursive_subagents::protocol::MessagePurpose::Result
                    })
                    .collect();
                assert_eq!(results.len(), 1);
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&results[0].body).unwrap()["output"],
                    "{\"summary\":\"review complete\"}"
                );
                break tree
                    .commands()
                    .keys()
                    .find(|id| id.starts_with(ProductAgentTree::LOSS_RESUME_COMMAND_PREFIX))
                    .unwrap()
                    .clone();
            }
            assert!(Instant::now() < deadline, "child attempt did not settle");
            std::thread::sleep(Duration::from_millis(10));
        };
    stop(&mut host);
    let FeedPage::Records { records, .. } = AgentSessions::inspect_feed_sqlite(
        paths.runtime_database(),
        FeedPageRequest::after(FeedCursor::start(locator.session_id())),
    )
    .unwrap() else {
        panic!("continued child terminal should still be retained");
    };
    let terminals: Vec<_> = records
        .iter()
        .flat_map(FeedRecord::facts)
        .filter_map(|fact| match fact {
            FeedFact::TurnTerminal {
                command_id: id,
                outcome,
                ..
            } if id.as_str() == command_id => Some(*outcome),
            _ => None,
        })
        .collect();
    assert_eq!(terminals, [FeedTurnOutcome::RecoveryRequired]);
}

fn explicitly_resume(sut: &FiniteDrain, host: &Host, parent: &NodeAddress, id: &str) {
    let mut client = Client::connect(host);
    runner::remote_ready(&mut client, &sut.paired.id);
    client.retain_observations();
    let attachment = client.attach(10, parent);
    wait_for_notice(&mut client, |text| {
        text.contains(id) && text.contains("nothing continues until you resume")
    });
    assert_eq!(
        client.request(
            11,
            Op::Intent {
                attachment,
                client_id: None,
                intent: SessionIntent::ResumeLossContinuation {
                    agent_id: Some(id.to_owned()),
                },
                blobs: Vec::new(),
            }
        ),
        Outcome::Ok(Answer::Accepted { duplicate: false }),
    );
    assert_eq!(
        wait_for_notice(&mut client, |text| text.contains(id)
            && text.starts_with("resumed ")),
        format!("resumed #{id}")
    );
}
